//! Bridge between the gRPC replay journal types and eryx's replay primitives.
//!
//! Converts protobuf [`CallbackJournal`](crate::proto::eryx::v1::CallbackJournal)
//! to/from [`eryx::CallbackJournal`], and wraps callbacks with
//! [`eryx::ReplayCallback`] so that journaled invocations are replayed from cache
//! instead of being dispatched to the gRPC client.
//!
//! # Journal signing
//!
//! Replayed journal entries are returned to Python code verbatim — the callback
//! is not re-executed. A valid signature attests that the journal is an
//! unmodified record of a real execution *of a specific script* on this server.
//! It does not (and cannot) stop the callback-answering client from choosing
//! arbitrary values live — it always could. What signing provides is
//! **provenance and tamper detection**: an auditor or downstream service can
//! trust that a signed journal was not spliced, edited, or replayed against a
//! different script after the fact.
//!
//! [`JournalSigner`] provides HMAC-SHA256 signing and verification. The MAC
//! covers both the journal entries (via deterministic protobuf encoding) and the
//! script code, so a journal is bound to the exact script that produced it.
//! This is correct for the primary replay use case (suspend/resume with the
//! same script); for edit-and-retry, the signature won't verify and the server
//! falls back to fresh execution (the safe default).
//!
//! The signing key should be injected at server startup (via
//! `ERYX_JOURNAL_SIGNING_KEY`) so that all replicas share the same key and
//! journals are portable across instances. If no key is configured, a random
//! ephemeral key is generated and a warning is logged — previously-signed
//! journals (or those from other replicas) will fail verification and replay
//! will fall back to fresh execution.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use eryx::{
    Callback, CallbackJournal, CallbackJournalEntry, ReplayCallback, ReplayState, SuspendedCallback,
};
use hmac::{Hmac, Mac};
use prost::Message;
use sha2::Sha256;

use crate::proto::eryx::v1 as pb;

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 signer/verifier for callback journals.
///
/// The MAC covers both the journal entries (deterministic protobuf encoding,
/// which naturally length-prefixes all variable-length fields) and the script
/// code that produced them, so a journal is bound to its originating script.
///
/// Create with [`from_key`](Self::from_key) for production (shared key across
/// replicas) or [`random`](Self::random) for tests/dev.
#[derive(Clone)]
pub struct JournalSigner {
    key: [u8; 32],
}

impl std::fmt::Debug for JournalSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalSigner")
            .field("key", &"[redacted]")
            .finish()
    }
}

impl JournalSigner {
    /// Create a signer from an explicit 32-byte key.
    ///
    /// Use this in production so all server replicas share the same key and
    /// journals are portable across instances.
    #[must_use]
    pub fn from_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Create a signer with a fresh random key.
    ///
    /// Journals signed with this key cannot be verified by other processes or
    /// after a restart. Intended for tests and single-instance dev servers.
    #[must_use]
    pub fn random() -> Self {
        use rand::RngCore;
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        Self { key }
    }

    /// Sign a proto journal in place, binding it to `code`.
    pub fn sign(&self, journal: &mut pb::CallbackJournal, code: &str) {
        journal.signature = self.compute_mac(&journal.entries, code);
    }

    /// Verify a proto journal's signature against `code`. Returns `true` if the
    /// signature is present and valid, `false` otherwise.
    #[must_use]
    pub fn verify(&self, journal: &pb::CallbackJournal, code: &str) -> bool {
        if journal.signature.is_empty() {
            return false;
        }
        let mut mac = self.new_mac();
        self.feed_mac(&mut mac, &journal.entries, code);
        mac.verify_slice(&journal.signature).is_ok()
    }

    fn compute_mac(&self, entries: &[pb::CallbackJournalEntry], code: &str) -> Vec<u8> {
        let mut mac = self.new_mac();
        self.feed_mac(&mut mac, entries, code);
        mac.finalize().into_bytes().to_vec()
    }

    fn feed_mac(&self, mac: &mut HmacSha256, entries: &[pb::CallbackJournalEntry], code: &str) {
        mac.update(code.as_bytes());
        // Deterministic protobuf encoding of each entry. Prost's `encode_to_vec`
        // length-prefixes all variable-length fields on the wire, so there is no
        // concatenation ambiguity between entries — but we also prefix each
        // entry's encoded length for belt-and-suspenders clarity.
        for entry in entries {
            let encoded = entry.encode_to_vec();
            mac.update(&(encoded.len() as u32).to_le_bytes());
            mac.update(&encoded);
        }
    }

    fn new_mac(&self) -> HmacSha256 {
        // HMAC-SHA256 accepts any key size (it pads or hashes internally), so
        // `new_from_slice` on a 32-byte key cannot fail. Use the infallible
        // constructor.
        HmacSha256::new_from_slice(&self.key)
            .unwrap_or_else(|_| unreachable!("HMAC-SHA256 accepts any key size"))
    }
}

impl Default for JournalSigner {
    fn default() -> Self {
        Self::random()
    }
}

/// Build an eryx [`CallbackJournal`] (to replay from) out of a request's proto
/// journal. `code` is the script being executed — it is informational; replay
/// matches on the callback invocation sequence, not the code.
#[must_use]
pub fn journal_from_proto(code: &str, proto: &pb::CallbackJournal) -> CallbackJournal {
    CallbackJournal {
        code: code.to_string(),
        entries: proto.entries.iter().map(entry_from_proto).collect(),
    }
}

fn entry_from_proto(entry: &pb::CallbackJournalEntry) -> CallbackJournalEntry {
    let result = match pb::CallbackOutcome::try_from(entry.outcome) {
        Ok(pb::CallbackOutcome::Error) => Err(entry.value.clone()),
        // OK (and any unexpected value, including the never-journaled SUSPEND)
        // is treated as a success value. Parse the stored JSON, falling back to
        // a string if it is not valid JSON.
        _ => Ok(serde_json::from_str(&entry.value)
            .unwrap_or_else(|_| serde_json::Value::String(entry.value.clone()))),
    };
    CallbackJournalEntry {
        index: entry.index,
        name: entry.name.clone(),
        args_hash: entry.args_hash,
        args_json: entry.args_json.clone(),
        result,
    }
}

/// Convert an eryx [`CallbackJournal`] recorded during a run into its proto form
/// for inclusion in `ExecuteResult`. The returned journal is **unsigned** — call
/// [`JournalSigner::sign`] before sending it to the client.
#[must_use]
pub fn journal_to_proto(journal: &CallbackJournal) -> pb::CallbackJournal {
    pb::CallbackJournal {
        entries: journal.entries.iter().map(entry_to_proto).collect(),
        signature: Vec::new(),
    }
}

fn entry_to_proto(entry: &CallbackJournalEntry) -> pb::CallbackJournalEntry {
    let (outcome, value) = match &entry.result {
        Ok(value) => (pb::CallbackOutcome::Ok, value.to_string()),
        Err(message) => (pb::CallbackOutcome::Error, message.clone()),
    };
    pb::CallbackJournalEntry {
        index: entry.index,
        name: entry.name.clone(),
        args_hash: entry.args_hash,
        args_json: entry.args_json.clone(),
        outcome: outcome as i32,
        value,
    }
}

/// Convert an eryx [`SuspendedCallback`] into its proto form.
#[must_use]
pub fn suspended_to_proto(suspended: &SuspendedCallback) -> pb::SuspendedCallback {
    pb::SuspendedCallback {
        name: suspended.name.clone(),
        args_json: suspended.args_json.clone(),
        reason: suspended.reason.clone(),
    }
}

/// Wrap every callback in `callbacks` with an [`eryx::ReplayCallback`] sharing
/// `state`, so journaled invocations replay from cache and live invocations are
/// recorded.
#[must_use]
pub fn wrap_for_replay(
    callbacks: &HashMap<String, Arc<dyn Callback>>,
    state: &Arc<Mutex<ReplayState>>,
) -> HashMap<String, Arc<dyn Callback>> {
    callbacks
        .iter()
        .map(|(name, callback)| {
            let wrapped: Arc<dyn Callback> =
                Arc::new(ReplayCallback::new(Arc::clone(callback), Arc::clone(state)));
            (name.clone(), wrapped)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn proto_roundtrip_ok_and_err_entries() {
        let journal = CallbackJournal {
            code: "code".into(),
            entries: vec![
                CallbackJournalEntry {
                    index: 0,
                    name: "fetch".into(),
                    args_hash: 42,
                    args_json: r#"{"q":"x"}"#.into(),
                    result: Ok(json!({"v": 1})),
                },
                CallbackJournalEntry {
                    index: 1,
                    name: "fail".into(),
                    args_hash: 7,
                    args_json: "{}".into(),
                    result: Err("execution failed: boom".into()),
                },
            ],
        };

        let proto = journal_to_proto(&journal);
        assert_eq!(proto.entries.len(), 2);
        assert_eq!(proto.entries[0].outcome, pb::CallbackOutcome::Ok as i32);
        assert_eq!(proto.entries[1].outcome, pb::CallbackOutcome::Error as i32);
        assert_eq!(proto.entries[1].value, "execution failed: boom");

        let back = journal_from_proto("code", &proto);
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries[0].result, Ok(json!({"v": 1})));
        assert_eq!(back.entries[0].args_hash, 42);
        assert_eq!(back.entries[1].result, Err("execution failed: boom".into()));
    }

    #[test]
    fn empty_proto_journal_yields_empty_eryx_journal() {
        let proto = pb::CallbackJournal {
            entries: vec![],
            signature: Vec::new(),
        };
        let journal = journal_from_proto("code", &proto);
        assert!(journal.is_empty());
    }

    // -- JournalSigner tests --

    fn test_entry() -> pb::CallbackJournalEntry {
        pb::CallbackJournalEntry {
            index: 0,
            name: "fetch".into(),
            args_hash: 42,
            args_json: r#"{"q":"x"}"#.into(),
            outcome: pb::CallbackOutcome::Ok as i32,
            value: r#"{"v":1}"#.into(),
        }
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let signer = JournalSigner::random();
        let mut journal = pb::CallbackJournal {
            entries: vec![test_entry()],
            signature: Vec::new(),
        };

        assert!(
            !signer.verify(&journal, "code"),
            "unsigned journal should not verify"
        );

        signer.sign(&mut journal, "code");
        assert!(!journal.signature.is_empty());
        assert!(
            signer.verify(&journal, "code"),
            "signed journal should verify"
        );
    }

    #[test]
    fn tampered_entry_fails_verification() {
        let signer = JournalSigner::random();
        let mut journal = pb::CallbackJournal {
            entries: vec![test_entry()],
            signature: Vec::new(),
        };
        signer.sign(&mut journal, "code");
        assert!(signer.verify(&journal, "code"));

        journal.entries[0].value = r#"{"v":999}"#.into();
        assert!(
            !signer.verify(&journal, "code"),
            "tampered journal should not verify"
        );
    }

    #[test]
    fn different_code_fails_verification() {
        let signer = JournalSigner::random();
        let mut journal = pb::CallbackJournal {
            entries: vec![test_entry()],
            signature: Vec::new(),
        };
        signer.sign(&mut journal, "original script");
        assert!(signer.verify(&journal, "original script"));
        assert!(
            !signer.verify(&journal, "different script"),
            "journal signed for one script should not verify against a different one"
        );
    }

    #[test]
    fn different_signer_fails_verification() {
        let signer1 = JournalSigner::random();
        let signer2 = JournalSigner::random();
        let mut journal = pb::CallbackJournal {
            entries: vec![test_entry()],
            signature: Vec::new(),
        };
        signer1.sign(&mut journal, "code");
        assert!(signer1.verify(&journal, "code"));
        assert!(
            !signer2.verify(&journal, "code"),
            "different key should not verify"
        );
    }

    #[test]
    fn empty_journal_signs_and_verifies() {
        let signer = JournalSigner::random();
        let mut journal = pb::CallbackJournal {
            entries: vec![],
            signature: Vec::new(),
        };
        signer.sign(&mut journal, "code");
        assert!(signer.verify(&journal, "code"));
    }

    #[test]
    fn from_key_is_deterministic() {
        let key = [42u8; 32];
        let signer1 = JournalSigner::from_key(key);
        let signer2 = JournalSigner::from_key(key);
        let mut journal = pb::CallbackJournal {
            entries: vec![test_entry()],
            signature: Vec::new(),
        };
        signer1.sign(&mut journal, "code");
        assert!(
            signer2.verify(&journal, "code"),
            "same key should verify across instances"
        );
    }
}
