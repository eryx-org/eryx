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
//! covers the journal entries (via deterministic protobuf encoding), its expiry
//! time, and the [`JournalBinding`]: the script code, the state snapshot the run
//! started from, and the declared callback names. A journal therefore only
//! replays into the same script, starting state and callback set that produced
//! it, and only until it expires. This is correct for the primary replay use
//! case (re-running the same script); for edit-and-retry, the signature won't
//! verify and the server rejects the replay before executing the script.
//! Starting an edited script requires a new execution without the old journal.
//!
//! The signing key should be injected at server startup (via
//! `ERYX_JOURNAL_SIGNING_KEY`) so that all replicas share the same key and
//! journals are portable across instances. If no key is configured, a random
//! ephemeral key is generated and a warning is logged — previously-signed
//! journals (or those from other replicas) will fail verification and replay
//! will be rejected before execution.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eryx::{CallbackJournal, CallbackJournalEntry, SuspendedCallback};
use hmac::{Hmac, KeyInit, Mac};
use prost::Message;
use sha2::Sha256;

use crate::proto::eryx::v1 as pb;

type HmacSha256 = Hmac<Sha256>;

/// Versions the MAC input layout, so journals signed under an older layout
/// fail verification instead of being read under the new one.
const MAC_DOMAIN: &[u8] = b"eryx-journal-v2";

/// The request context a journal is bound to, besides its entries.
#[derive(Debug, Clone, Copy)]
pub struct JournalBinding<'a> {
    /// The script code.
    pub code: &'a str,
    /// The state snapshot the run started from, or `None` when state is not
    /// persisted.
    pub state: Option<&'a [u8]>,
    /// The declared callbacks; their names are bound, in any order.
    pub callbacks: &'a [pb::CallbackDeclaration],
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// HMAC-SHA256 signer/verifier for callback journals.
///
/// The MAC covers the journal entries (deterministic protobuf encoding, which
/// naturally length-prefixes all variable-length fields), the expiry time and
/// the [`JournalBinding`] of the run that produced them.
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
        // rand 0.10 renamed the core trait `RngCore` -> `Rng` (and the old
        // `Rng` extension trait -> `RngExt`), and `thread_rng()` -> `rng()`.
        use rand::Rng;
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        Self { key }
    }

    /// Sign a proto journal in place, binding it to `binding` and making it
    /// expire `ttl` from now.
    pub fn sign(
        &self,
        journal: &mut pb::CallbackJournal,
        binding: JournalBinding<'_>,
        ttl: Duration,
    ) {
        journal.expires_at_unix_secs = now_unix_secs().saturating_add(ttl.as_secs());
        let mut mac = self.new_mac();
        Self::feed_mac(&mut mac, journal, binding);
        journal.signature = mac.finalize().into_bytes().to_vec();
    }

    /// Verify a proto journal's signature against `binding`. Returns `true` if
    /// the signature is present and valid and the journal has not expired.
    #[must_use]
    pub fn verify(&self, journal: &pb::CallbackJournal, binding: JournalBinding<'_>) -> bool {
        if journal.signature.is_empty() || journal.expires_at_unix_secs <= now_unix_secs() {
            return false;
        }
        let mut mac = self.new_mac();
        Self::feed_mac(&mut mac, journal, binding);
        mac.verify_slice(&journal.signature).is_ok()
    }

    fn feed_mac(mac: &mut HmacSha256, journal: &pb::CallbackJournal, binding: JournalBinding<'_>) {
        // Every variable-length input is length-prefixed so the boundaries are
        // unambiguous and the MAC stays injective over its inputs. Without
        // that, e.g. `code || <entry stream>` could be re-split - a script
        // whose bytes happen to embed an entry's encoding could feed-collide
        // with a different (code, entries) pair.
        let mut feed = |bytes: &[u8]| {
            mac.update(&(bytes.len() as u64).to_le_bytes());
            mac.update(bytes);
        };
        feed(MAC_DOMAIN);
        feed(&journal.expires_at_unix_secs.to_le_bytes());
        feed(binding.code.as_bytes());
        match binding.state {
            None => feed(&[0]),
            Some(state) => {
                feed(&[1]);
                feed(state);
            }
        }
        let mut names: Vec<&str> = binding.callbacks.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        feed(&(names.len() as u64).to_le_bytes());
        for name in names {
            feed(name.as_bytes());
        }
        // Deterministic protobuf encoding of each entry.
        for entry in &journal.entries {
            feed(&entry.encode_to_vec());
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
/// matches on callback name + canonical args (FIFO per key), not the code.
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
        // OK (and any unexpected value) is treated as a success value. Parse the
        // stored JSON, falling back to a string if it is not valid JSON.
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
        ..Default::default()
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
        let proto = pb::CallbackJournal::default();
        let journal = journal_from_proto("code", &proto);
        assert!(journal.is_empty());
    }

    // -- JournalSigner tests --

    const TTL: Duration = Duration::from_secs(60);

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

    fn binding(code: &str) -> JournalBinding<'_> {
        JournalBinding {
            code,
            state: None,
            callbacks: &[],
        }
    }

    fn signed(signer: &JournalSigner, binding: JournalBinding<'_>) -> pb::CallbackJournal {
        let mut journal = pb::CallbackJournal {
            entries: vec![test_entry()],
            ..Default::default()
        };
        signer.sign(&mut journal, binding, TTL);
        journal
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let signer = JournalSigner::random();
        let unsigned = pb::CallbackJournal {
            entries: vec![test_entry()],
            ..Default::default()
        };
        assert!(
            !signer.verify(&unsigned, binding("code")),
            "unsigned journal should not verify"
        );

        let journal = signed(&signer, binding("code"));
        assert!(!journal.signature.is_empty());
        assert!(
            signer.verify(&journal, binding("code")),
            "signed journal should verify"
        );
    }

    #[test]
    fn tampered_entry_fails_verification() {
        let signer = JournalSigner::random();
        let mut journal = signed(&signer, binding("code"));
        journal.entries[0].value = r#"{"v":999}"#.into();
        assert!(
            !signer.verify(&journal, binding("code")),
            "tampered journal should not verify"
        );
    }

    #[test]
    fn different_code_fails_verification() {
        let signer = JournalSigner::random();
        let journal = signed(&signer, binding("original script"));
        assert!(signer.verify(&journal, binding("original script")));
        assert!(
            !signer.verify(&journal, binding("different script")),
            "journal signed for one script should not verify against a different one"
        );
    }

    #[test]
    fn journal_is_bound_to_starting_state() {
        let signer = JournalSigner::random();
        let with_state = |state| JournalBinding {
            state,
            ..binding("code")
        };
        let journal = signed(&signer, with_state(Some(b"state-a")));
        assert!(signer.verify(&journal, with_state(Some(b"state-a"))));
        assert!(!signer.verify(&journal, with_state(Some(b"state-b"))));
        assert!(!signer.verify(&journal, with_state(None)));
        assert!(!signer.verify(&journal, with_state(Some(b""))));
    }

    #[test]
    fn journal_is_bound_to_callback_names() {
        let decl = |name: &str| pb::CallbackDeclaration {
            name: name.into(),
            ..Default::default()
        };
        let ab = [decl("a"), decl("b")];
        let ba = [decl("b"), decl("a")];
        let ac = [decl("a"), decl("c")];
        let with = |callbacks| JournalBinding {
            callbacks,
            ..binding("code")
        };
        let signer = JournalSigner::random();
        let journal = signed(&signer, with(&ab));
        assert!(signer.verify(&journal, with(&ba)), "order must not matter");
        assert!(!signer.verify(&journal, with(&ac)));
        assert!(!signer.verify(&journal, with(&ab[..1])));
    }

    #[test]
    fn expired_journal_fails_verification() {
        let signer = JournalSigner::random();
        let mut journal = pb::CallbackJournal {
            entries: vec![test_entry()],
            ..Default::default()
        };
        signer.sign(&mut journal, binding("code"), Duration::ZERO);
        assert!(!signer.verify(&journal, binding("code")));

        // Pushing the expiry forward breaks the signature.
        journal.expires_at_unix_secs += 3600;
        assert!(!signer.verify(&journal, binding("code")));
    }

    #[test]
    fn different_signer_fails_verification() {
        let journal = signed(&JournalSigner::random(), binding("code"));
        assert!(
            !JournalSigner::random().verify(&journal, binding("code")),
            "different key should not verify"
        );
    }

    #[test]
    fn empty_journal_signs_and_verifies() {
        let signer = JournalSigner::random();
        let mut journal = pb::CallbackJournal::default();
        signer.sign(&mut journal, binding("code"), TTL);
        assert!(signer.verify(&journal, binding("code")));
    }

    #[test]
    fn from_key_is_deterministic() {
        let key: [u8; 32] = rand::random();
        let journal = signed(&JournalSigner::from_key(key), binding("code"));
        assert!(
            JournalSigner::from_key(key).verify(&journal, binding("code")),
            "same key should verify across instances"
        );
    }
}
