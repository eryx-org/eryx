//! Bridge between the gRPC replay journal types and eryx's replay primitives.
//!
//! Converts protobuf [`CallbackJournal`](crate::proto::eryx::v1::CallbackJournal)
//! to/from [`eryx::CallbackJournal`], and wraps callbacks with
//! [`eryx::ReplayCallback`] so that journaled invocations are replayed from cache
//! instead of being dispatched to the gRPC client.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use eryx::{
    Callback, CallbackJournal, CallbackJournalEntry, ReplayCallback, ReplayState, SuspendedCallback,
};

use crate::proto::eryx::v1 as pb;

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
/// for inclusion in `ExecuteResult`.
#[must_use]
pub fn journal_to_proto(journal: &CallbackJournal) -> pb::CallbackJournal {
    pb::CallbackJournal {
        entries: journal.entries.iter().map(entry_to_proto).collect(),
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
        let proto = pb::CallbackJournal { entries: vec![] };
        let journal = journal_from_proto("code", &proto);
        assert!(journal.is_empty());
    }
}
