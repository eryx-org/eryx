//! Callback result replay.
//!
//! When an LLM iterates on a Python script that drives expensive callbacks
//! (tool calls, API requests, database queries), an error in a later part of
//! the script normally forces the whole script to be re-run from scratch —
//! re-invoking callbacks that already succeeded. This module avoids that by
//! *journaling* callback results during execution and *replaying* them on a
//! subsequent run.
//!
//! Rather than checkpointing the Python interpreter (which can't capture
//! mid-execution frames), we record the ordered sequence of callback
//! invocations and their results. On resubmission the entire script is
//! re-executed, but callbacks that match the recorded journal short-circuit to
//! the cached result instead of making a real call. Because callbacks are the
//! expensive part, and Python execution between them is comparatively free,
//! this is both fast and robust to arbitrary code structure (loops,
//! conditionals, nested functions) — the journal operates on the *invocation
//! sequence*, not on the code.
//!
//! At this layer, replay matching tolerates code edits: callbacks are matched by
//! name, arguments, and position in the invocation sequence, so a journal can
//! replay its still-matching prefix even if the script changed between runs (the
//! first divergence switches to live mode). Note, however, that a caller that
//! signs journals and binds the signature to the exact script (as the
//! `eryx-server` layer does) will reject an edited script's journal *before* it
//! reaches this matching logic, restricting replay to byte-identical re-runs
//! (e.g. suspend/resume). Whether edits can replay is therefore a property of
//! the integrity policy layered on top, not of this module.
//!
//! Replay is implemented entirely as a [`Callback`] wrapper ([`ReplayCallback`]):
//! no changes are needed to the WASM runtime, the WIT interface, or the Python
//! code. The wrapper consults a shared [`ReplayState`] before delegating to the
//! real callback.
//!
//! # Suspension
//!
//! A callback can also *defer* — signal that it cannot complete right now but
//! should be retried later (e.g. waiting on external approval or a rate-limit
//! cooldown). A callback signals this by returning
//! [`CallbackError::Suspend`]. When that happens
//! during a replay-aware run, the suspending callback is recorded as
//! [`ReplayOutcome::suspended`](crate::ReplayOutcome::suspended), the journal of
//! all *completed* callbacks up to that point is still returned, and the
//! exception propagates into Python (terminating the script). The caller can
//! persist the journal, act on the suspension reason, and later re-execute the
//! script with the journal — the previously-completed callbacks replay from
//! cache and the suspending callback runs live.
//!
//! Eryx does not interpret the suspension reason; it is an opaque, caller-defined
//! string.
//!
//! ## What suspension does and does not guarantee
//!
//! Suspension is enforced by two mechanisms working together:
//!
//! 1. A **synchronous gate**: once any callback has suspended, every *subsequent
//!    callback* the guest dispatches is rejected under the state lock (the
//!    internal `Decision::AlreadySuspended` path), so no further callback runs —
//!    even if Python caught the suspend exception in a `try/except`. This is
//!    deterministic and independent of timing.
//! 2. An **asynchronous epoch interrupt** (a backstop wired up by the executor):
//!    after the suspend result is delivered to the guest, the engine epoch is
//!    bumped, trapping the WASM guest so that *pure* (non-callback) Python code
//!    after the suspension point stops running too.
//!
//! Because the epoch interrupt is asynchronous (the ticker polls on a ~10ms
//! granularity), there is a brief window after the suspend exception is raised
//! in which non-callback Python code may still execute if the script catches the
//! exception. The synchronous gate closes this window for *callbacks*, but other
//! host-backed side effects that do not funnel through the callback gate — most
//! notably **outbound network requests and VFS writes** — are *not* gated and
//! may still occur within that window. Suspension is therefore a clean stop for
//! callback side effects, but is best-effort for other side effects. Callers
//! that need a hard guarantee against post-suspension network or filesystem
//! activity must enforce it at a higher layer.
//!
//! # Security: journal trust boundary
//!
//! Replayed journal entries are returned to Python code as-is — eryx does not
//! re-execute the callback. This means a crafted journal can inject arbitrary
//! values into a script's execution. **The journal is a trusted input.**
//!
//! When journals round-trip through the caller (e.g. stored in a database and
//! loaded later, or returned to a client and echoed back), the caller must
//! ensure they have not been tampered with. The gRPC server layer
//! (`eryx-server`) provides HMAC-SHA256 signing for this purpose; the core
//! `eryx` crate is agnostic to signing and trusts whatever journal it receives.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::callback::{Callback, CallbackError};
use crate::schema::Schema;

/// The three-way outcome of a callback invocation, as understood by replay.
///
/// This mirrors what a callback can communicate back through the
/// [`Callback`] trait: a success value, a permanent error, or a request to
/// suspend. It is the conceptual model the journal is built from — only
/// [`CallbackOutcome::Ok`] and [`CallbackOutcome::Err`] are ever journaled;
/// [`CallbackOutcome::Suspend`] terminates execution before an entry is recorded.
#[derive(Debug, Clone)]
pub enum CallbackOutcome {
    /// The callback succeeded. The value is returned to Python and journaled.
    Ok(serde_json::Value),
    /// The callback failed permanently. Python receives an exception and the
    /// error is journaled (so a later replay reproduces the same failure).
    Err(String),
    /// The callback cannot complete now. Python receives an exception,
    /// execution stops, and the suspension is surfaced to the caller. The
    /// reason string is opaque to eryx.
    Suspend(String),
}

impl CallbackOutcome {
    /// Classify the result of a real [`Callback::invoke`] into an outcome.
    fn from_invoke(result: &Result<serde_json::Value, CallbackError>) -> Self {
        match result {
            Ok(value) => Self::Ok(value.clone()),
            Err(CallbackError::Suspend(reason)) => Self::Suspend(reason.clone()),
            Err(other) => Self::Err(other.to_string()),
        }
    }
}

/// A single recorded callback invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallbackJournalEntry {
    /// Position in the invocation sequence (0-indexed, in initiation order).
    pub index: u32,
    /// Callback name.
    pub name: String,
    /// Stable hash of the canonicalized arguments JSON, for fast comparison.
    pub args_hash: u64,
    /// The canonicalized arguments JSON (for verification and debugging).
    pub args_json: String,
    /// The result, as returned by the callback. `Ok` carries the success value;
    /// `Err` carries the error message exactly as Python observed it. Suspended
    /// callbacks are never recorded.
    pub result: Result<serde_json::Value, String>,
}

/// An ordered journal of callback invocations from a single execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CallbackJournal {
    /// The script that produced this journal.
    pub code: String,
    /// Ordered callback invocations, in the order they were initiated.
    pub entries: Vec<CallbackJournalEntry>,
}

impl CallbackJournal {
    /// Create an empty journal for the given script.
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            entries: Vec::new(),
        }
    }

    /// Number of recorded entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal has no recorded entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Details of the callback that suspended execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspendedCallback {
    /// Name of the callback that suspended.
    pub name: String,
    /// Canonicalized arguments JSON the callback was invoked with.
    pub args_json: String,
    /// The opaque reason string the callback returned.
    pub reason: String,
}

/// Mutable replay state shared across all wrapped callbacks in one execution.
///
/// A single state is shared (behind a mutex) by every [`ReplayCallback`] in an
/// execution, so the cursor advances across callbacks of different names in
/// strict initiation order.
#[derive(Debug)]
pub struct ReplayState {
    /// The previous journal being replayed from.
    previous: CallbackJournal,
    /// Current position in `previous.entries`.
    cursor: usize,
    /// Once a mismatch is hit we switch to live mode permanently.
    live_mode: bool,
    /// Monotonic counter assigning each invocation its position.
    next_seq: usize,
    /// Entries recorded for *this* execution, indexed by sequence number.
    /// `None` marks a reserved-but-unrecorded slot (in-flight or suspended).
    entries: Vec<Option<CallbackJournalEntry>>,
    /// Set if a callback suspended; only the first suspension is recorded.
    suspended: Option<SuspendedCallback>,
    /// How many invocations were served from the previous journal.
    replayed_count: u32,
}

/// The decision made for a single invocation while holding the state lock.
enum Decision {
    /// Served from the journal; the result is returned without a live call.
    Hit(Result<serde_json::Value, String>),
    /// Not in the journal (or journal diverged); invoke live and record at `seq`.
    Miss { seq: usize },
    /// A previous callback already suspended; reject this invocation without
    /// invoking the real callback or recording anything.
    ///
    /// This is the deterministic guarantee that no side-effecting callback runs
    /// after a suspension: because every invocation funnels through `decide`,
    /// any callback the guest dispatches *after* the suspension is recorded
    /// (e.g. from a `try/except` that caught the suspend exception) is rejected
    /// here, regardless of the asynchronous epoch interrupt's timing. The epoch
    /// trap is a backstop for pure (non-callback) post-suspension code.
    AlreadySuspended,
}

impl ReplayState {
    /// Create fresh replay state that replays from `previous`.
    ///
    /// Pass an empty journal (see [`CallbackJournal::new`]) to record a fresh
    /// journal without replaying anything.
    #[must_use]
    pub fn new(previous: CallbackJournal) -> Self {
        Self {
            previous,
            cursor: 0,
            live_mode: false,
            next_seq: 0,
            entries: Vec::new(),
            suspended: None,
            replayed_count: 0,
        }
    }

    /// Decide how to handle an invocation, advancing the cursor as needed.
    ///
    /// This runs synchronously in callback-initiation order, so the assigned
    /// sequence numbers and cursor advancement are deterministic even when
    /// callbacks are launched concurrently via `asyncio.gather`.
    fn decide(&mut self, name: &str, args_hash: u64, args_json: &str) -> Decision {
        // Once any callback has suspended, reject every subsequent invocation.
        // This deterministically prevents post-suspension callbacks from firing
        // even if Python catches the suspend exception (the epoch interrupt is
        // asynchronous and cannot guarantee this on its own).
        if self.suspended.is_some() {
            return Decision::AlreadySuspended;
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        self.ensure_slot(seq);

        if !self.live_mode
            && let Some(entry) = self.previous.entries.get(self.cursor)
        {
            // Match by recorded initiation position (`index`), not by raw cursor
            // offset. A suspended callback leaves no entry, so the previous
            // journal can have holes; matching on `index` keeps later entries
            // replayable across those holes (e.g. a concurrent `gather` where the
            // suspender was initiated before a sibling that completed).
            let entry_index = entry.index as usize;
            if entry_index == seq {
                if entry.name == name
                    && entry.args_hash == args_hash
                    && entry.args_json == args_json
                {
                    // Cache hit: advance the cursor and record the replayed entry.
                    self.cursor += 1;
                    self.replayed_count += 1;
                    let result = entry.result.clone();
                    self.entries[seq] = Some(CallbackJournalEntry {
                        index: u32::try_from(seq).unwrap_or(u32::MAX),
                        name: name.to_string(),
                        args_hash,
                        args_json: args_json.to_string(),
                        result: result.clone(),
                    });
                    return Decision::Hit(result);
                }
                // A recorded position now has a different call: the trace
                // genuinely diverged. Switch to live mode for the rest of the
                // run — later cached entries may depend on earlier results that
                // are now different.
                self.live_mode = true;
            } else if entry_index < seq {
                // Defensive: a recorded invocation is missing at its position
                // (should not happen while matching, since we either consume an
                // entry at its index or diverge there). Treat as divergence.
                self.live_mode = true;
            }
            // entry_index > seq: this position was a hole in the previous run
            // (its callback suspended), so there is nothing to replay here. Fall
            // through to a live Miss WITHOUT entering live mode, leaving the
            // entry for when `seq` reaches its index so later calls still replay.
        }
        // Otherwise the previous journal is exhausted (or we are already live):
        // fall through to live invocation.

        Decision::Miss { seq }
    }

    /// Ensure `entries` has a slot at `seq`.
    fn ensure_slot(&mut self, seq: usize) {
        if self.entries.len() <= seq {
            self.entries.resize(seq + 1, None);
        }
    }

    /// Record the result of a live invocation at its reserved slot.
    fn record_live(&mut self, entry: CallbackJournalEntry) {
        let seq = entry.index as usize;
        self.ensure_slot(seq);
        self.entries[seq] = Some(entry);
    }

    /// Record a suspension (only the first one is kept).
    ///
    /// In-flight callbacks launched concurrently (e.g. `asyncio.gather`) may
    /// complete and call [`record_live`](Self::record_live) after a suspension
    /// is recorded. This is intentional: those callbacks *did* complete
    /// successfully on the host side, so journaling them lets the next resume
    /// replay them from cache instead of re-running expensive calls.
    fn record_suspend(&mut self, suspended: SuspendedCallback) {
        if self.suspended.is_none() {
            self.suspended = Some(suspended);
        }
    }

    /// Number of invocations served from the previous journal.
    #[must_use]
    pub fn replayed_count(&self) -> u32 {
        self.replayed_count
    }

    /// The suspension recorded during this run, if any.
    #[must_use]
    pub fn suspended(&self) -> Option<&SuspendedCallback> {
        self.suspended.as_ref()
    }

    /// Build the journal recorded during this run for the given script.
    ///
    /// Reserved-but-unrecorded slots (e.g. the suspending callback) are dropped;
    /// recorded entries keep their initiation-order positions.
    #[must_use]
    pub fn build_journal(&self, code: impl Into<String>) -> CallbackJournal {
        CallbackJournal {
            code: code.into(),
            entries: self.entries.iter().flatten().cloned().collect(),
        }
    }
}

/// Wraps a real callback with journal-aware replay logic.
///
/// All wrapped callbacks in a single execution share one [`ReplayState`], so the
/// replay cursor advances across callbacks of different names in the order they
/// are invoked.
pub struct ReplayCallback {
    /// The real callback to delegate to on a cache miss.
    inner: Arc<dyn Callback>,
    /// Shared replay state (cursor + previous journal + journal being recorded).
    state: Arc<Mutex<ReplayState>>,
}

impl std::fmt::Debug for ReplayCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayCallback")
            .field("name", &self.inner.name())
            .finish_non_exhaustive()
    }
}

impl ReplayCallback {
    /// Wrap `inner`, sharing the given replay `state`.
    #[must_use]
    pub fn new(inner: Arc<dyn Callback>, state: Arc<Mutex<ReplayState>>) -> Self {
        Self { inner, state }
    }
}

impl Callback for ReplayCallback {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Schema {
        self.inner.parameters_schema()
    }

    fn invoke(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>> {
        let name = self.inner.name().to_string();
        let args_json = canonical_json(&args);
        let args_hash = fnv1a_64(args_json.as_bytes());

        // The journal decision runs synchronously, before the future is
        // returned, so it happens in callback-initiation order.
        let decision = lock_state(&self.state).decide(&name, args_hash, &args_json);

        match decision {
            Decision::AlreadySuspended => Box::pin(async {
                Err(CallbackError::Suspend(
                    "execution already suspended by a previous callback".into(),
                ))
            }),
            Decision::Hit(result) => {
                let invoke_result = match result {
                    Ok(value) => Ok(value),
                    // Use the transparent `Replayed` variant so the callback
                    // handler re-emits the original error text verbatim rather
                    // than wrapping it with another error prefix.
                    Err(message) => Err(CallbackError::Replayed(message)),
                };
                Box::pin(async move { invoke_result })
            }
            Decision::Miss { seq } => {
                let inner = Arc::clone(&self.inner);
                let state = Arc::clone(&self.state);
                Box::pin(async move {
                    let result = inner.invoke(args).await;
                    let index = u32::try_from(seq).unwrap_or(u32::MAX);
                    match CallbackOutcome::from_invoke(&result) {
                        CallbackOutcome::Ok(value) => {
                            lock_state(&state).record_live(CallbackJournalEntry {
                                index,
                                name,
                                args_hash,
                                args_json,
                                result: Ok(value),
                            });
                        }
                        CallbackOutcome::Err(message) => {
                            lock_state(&state).record_live(CallbackJournalEntry {
                                index,
                                name,
                                args_hash,
                                args_json,
                                result: Err(message),
                            });
                        }
                        CallbackOutcome::Suspend(reason) => {
                            // Do not journal a suspended callback; only record
                            // that it suspended so the caller can act on it.
                            lock_state(&state).record_suspend(SuspendedCallback {
                                name,
                                args_json,
                                reason,
                            });
                        }
                    }
                    result
                })
            }
        }
    }
}

/// Wrap every callback in `callbacks` with a [`ReplayCallback`] sharing `state`.
pub(crate) fn wrap_callbacks(
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

/// Lock the replay state, recovering from poisoning rather than panicking.
fn lock_state(state: &Mutex<ReplayState>) -> MutexGuard<'_, ReplayState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Produce a canonical JSON string for `value` with object keys sorted
/// recursively, so logically-equal arguments hash identically regardless of key
/// order.
fn canonical_json(value: &serde_json::Value) -> String {
    canonicalize(value).to_string()
}

/// Recursively rebuild `value` with object keys in sorted order.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                if let Some(v) = map.get(key) {
                    sorted.insert(key.clone(), canonicalize(v));
                }
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize).collect())
        }
        other => other.clone(),
    }
}

/// 64-bit FNV-1a hash — small, dependency-free, and deterministic across runs.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A test callback that returns a configurable outcome and counts how many
    /// times it is actually invoked (so we can prove replay skips live calls).
    struct ProgrammableCallback {
        name: String,
        calls: Arc<AtomicU32>,
        outcome: Outcome,
    }

    #[derive(Clone)]
    enum Outcome {
        Ok(serde_json::Value),
        Err(String),
        Suspend(String),
    }

    impl Callback for ProgrammableCallback {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "test"
        }
        fn parameters_schema(&self) -> Schema {
            Schema::empty()
        }
        fn invoke(
            &self,
            _args: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self.outcome.clone();
            Box::pin(async move {
                match outcome {
                    Outcome::Ok(v) => Ok(v),
                    Outcome::Err(m) => Err(CallbackError::ExecutionFailed(m)),
                    Outcome::Suspend(r) => Err(CallbackError::Suspend(r)),
                }
            })
        }
    }

    fn programmable(name: &str, outcome: Outcome) -> (Arc<ProgrammableCallback>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let cb = Arc::new(ProgrammableCallback {
            name: name.to_string(),
            calls: Arc::clone(&calls),
            outcome,
        });
        (cb, calls)
    }

    fn entry_ok(
        index: u32,
        name: &str,
        args: &serde_json::Value,
        value: serde_json::Value,
    ) -> CallbackJournalEntry {
        let args_json = canonical_json(args);
        CallbackJournalEntry {
            index,
            name: name.to_string(),
            args_hash: fnv1a_64(args_json.as_bytes()),
            args_json,
            result: Ok(value),
        }
    }

    // ---- CallbackOutcome classification -------------------------------------

    #[test]
    fn outcome_classifies_ok_err_suspend() {
        assert!(matches!(
            CallbackOutcome::from_invoke(&Ok(json!(1))),
            CallbackOutcome::Ok(_)
        ));
        assert!(matches!(
            CallbackOutcome::from_invoke(&Err(CallbackError::ExecutionFailed("x".into()))),
            CallbackOutcome::Err(_)
        ));
        assert!(matches!(
            CallbackOutcome::from_invoke(&Err(CallbackError::Suspend("wait".into()))),
            CallbackOutcome::Suspend(_)
        ));
    }

    // ---- canonicalization / hashing -----------------------------------------

    #[test]
    fn canonicalization_is_key_order_independent() {
        let a = json!({"a": 1, "b": {"c": 2, "d": 3}});
        let b = json!({"b": {"d": 3, "c": 2}, "a": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(
            fnv1a_64(canonical_json(&a).as_bytes()),
            fnv1a_64(canonical_json(&b).as_bytes())
        );
    }

    // ---- replay behavior ----------------------------------------------------

    #[tokio::test]
    async fn full_match_replays_without_invoking() {
        let args = json!({"q": "hello"});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![entry_ok(0, "fetch", &args, json!({"v": 1}))],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));
        let (inner, calls) = programmable("fetch", Outcome::Ok(json!({"v": 999})));
        let wrapper = ReplayCallback::new(inner, Arc::clone(&state));

        let result = wrapper.invoke(args.clone()).await.unwrap();

        assert_eq!(result, json!({"v": 1}), "should return cached value");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "real callback not invoked");
        let st = lock_state(&state);
        assert_eq!(st.replayed_count(), 1);
        let journal = st.build_journal("code");
        assert_eq!(journal.entries.len(), 1);
    }

    #[tokio::test]
    async fn empty_journal_goes_live_and_records() {
        let args = json!({"q": "hi"});
        let state = Arc::new(Mutex::new(ReplayState::new(CallbackJournal::new("code"))));
        let (inner, calls) = programmable("fetch", Outcome::Ok(json!({"v": 7})));
        let wrapper = ReplayCallback::new(inner, Arc::clone(&state));

        let result = wrapper.invoke(args.clone()).await.unwrap();

        assert_eq!(result, json!({"v": 7}));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "real callback invoked once"
        );
        let st = lock_state(&state);
        assert_eq!(st.replayed_count(), 0);
        let journal = st.build_journal("code");
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].result, Ok(json!({"v": 7})));
    }

    #[tokio::test]
    async fn mismatch_switches_to_live_mode_permanently() {
        let args = json!({"q": "a"});
        // Journal expects "fetch" first, but we'll invoke "other" -> mismatch.
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![
                entry_ok(0, "fetch", &args, json!("cached")),
                entry_ok(1, "fetch", &args, json!("cached2")),
            ],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));

        let (other, other_calls) = programmable("other", Outcome::Ok(json!("live-other")));
        let other_wrapper = ReplayCallback::new(other, Arc::clone(&state));
        let r1 = other_wrapper.invoke(args.clone()).await.unwrap();
        assert_eq!(r1, json!("live-other"));
        assert_eq!(other_calls.load(Ordering::SeqCst), 1);

        // Even though "fetch" now matches the cursor, live mode is sticky.
        let (fetch, fetch_calls) = programmable("fetch", Outcome::Ok(json!("live-fetch")));
        let fetch_wrapper = ReplayCallback::new(fetch, Arc::clone(&state));
        let r2 = fetch_wrapper.invoke(args.clone()).await.unwrap();
        assert_eq!(r2, json!("live-fetch"), "no re-sync after divergence");
        assert_eq!(fetch_calls.load(Ordering::SeqCst), 1);

        let st = lock_state(&state);
        assert_eq!(st.replayed_count(), 0);
        assert_eq!(st.build_journal("code").entries.len(), 2);
    }

    #[tokio::test]
    async fn args_mismatch_is_a_miss() {
        let cached_args = json!({"q": "a"});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![entry_ok(0, "fetch", &cached_args, json!("cached"))],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));
        let (inner, calls) = programmable("fetch", Outcome::Ok(json!("live")));
        let wrapper = ReplayCallback::new(inner, Arc::clone(&state));

        // Same name, different args -> mismatch -> live.
        let result = wrapper.invoke(json!({"q": "DIFFERENT"})).await.unwrap();
        assert_eq!(result, json!("live"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn replayed_error_is_returned_transparently() {
        let args = json!({});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![CallbackJournalEntry {
                index: 0,
                name: "fail".into(),
                args_hash: fnv1a_64(canonical_json(&args).as_bytes()),
                args_json: canonical_json(&args),
                result: Err("execution failed: boom".into()),
            }],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));
        let (inner, calls) = programmable("fail", Outcome::Ok(json!("unused")));
        let wrapper = ReplayCallback::new(inner, Arc::clone(&state));

        let err = wrapper.invoke(args).await.unwrap_err();
        // Replayed errors display verbatim (no extra prefix).
        assert_eq!(err.to_string(), "execution failed: boom");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn suspend_during_live_records_suspension_not_entry() {
        let state = Arc::new(Mutex::new(ReplayState::new(CallbackJournal::new("code"))));
        let (inner, calls) = programmable("approve", Outcome::Suspend("needs approval".into()));
        let wrapper = ReplayCallback::new(inner, Arc::clone(&state));

        let err = wrapper.invoke(json!({"id": 5})).await.unwrap_err();
        assert!(matches!(err, CallbackError::Suspend(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let st = lock_state(&state);
        let suspended = st.suspended().expect("suspension recorded");
        assert_eq!(suspended.name, "approve");
        assert_eq!(suspended.reason, "needs approval");
        // The suspended callback is NOT journaled.
        assert!(st.build_journal("code").is_empty());
    }

    #[tokio::test]
    async fn suspend_after_replaying_prefix() {
        // Journal has one completed callback; replay it, then a new callback suspends.
        let fetch_args = json!({"q": "x"});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![entry_ok(0, "fetch", &fetch_args, json!("cached"))],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));

        let (fetch, fetch_calls) = programmable("fetch", Outcome::Ok(json!("live")));
        let fetch_wrapper = ReplayCallback::new(fetch, Arc::clone(&state));
        let r1 = fetch_wrapper.invoke(fetch_args).await.unwrap();
        assert_eq!(r1, json!("cached"));
        assert_eq!(fetch_calls.load(Ordering::SeqCst), 0, "fetch replayed");

        let (approve, _) = programmable("approve", Outcome::Suspend("wait".into()));
        let approve_wrapper = ReplayCallback::new(approve, Arc::clone(&state));
        let err = approve_wrapper.invoke(json!({})).await.unwrap_err();
        assert!(matches!(err, CallbackError::Suspend(_)));

        let st = lock_state(&state);
        assert_eq!(st.replayed_count(), 1);
        assert_eq!(st.suspended().unwrap().name, "approve");
        // Only the replayed `fetch` is journaled; the suspended `approve` is not.
        let journal = st.build_journal("code");
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].name, "fetch");
    }

    #[tokio::test]
    async fn live_error_is_journaled() {
        let state = Arc::new(Mutex::new(ReplayState::new(CallbackJournal::new("code"))));
        let (inner, _) = programmable("fail", Outcome::Err("boom".into()));
        let wrapper = ReplayCallback::new(inner, Arc::clone(&state));

        let err = wrapper.invoke(json!({})).await.unwrap_err();
        assert!(matches!(err, CallbackError::ExecutionFailed(_)));

        let st = lock_state(&state);
        let journal = st.build_journal("code");
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(
            journal.entries[0].result,
            Err("execution failed: boom".to_string())
        );
    }

    #[tokio::test]
    async fn suspend_records_metadata_without_journaling() {
        // The actual execution halt is enforced by epoch-based cancellation at
        // the WASM level (tested in integration tests). This unit test only
        // verifies that the replay layer correctly records the suspension
        // metadata and does not journal the suspended callback.
        let state = Arc::new(Mutex::new(ReplayState::new(CallbackJournal::new("code"))));

        let (suspender, suspend_calls) = programmable("approve", Outcome::Suspend("wait".into()));
        let suspend_wrapper = ReplayCallback::new(suspender, Arc::clone(&state));
        let err = suspend_wrapper.invoke(json!({"id": 5})).await.unwrap_err();
        assert!(matches!(err, CallbackError::Suspend(_)));
        assert_eq!(suspend_calls.load(Ordering::SeqCst), 1);

        let st = lock_state(&state);
        let suspended = st.suspended().expect("suspension should be recorded");
        assert_eq!(suspended.name, "approve");
        assert_eq!(suspended.reason, "wait");
        assert!(
            st.build_journal("code").is_empty(),
            "suspended callback must not be journaled"
        );
    }

    #[tokio::test]
    async fn callbacks_dispatched_after_suspension_are_rejected() {
        // A callback the guest dispatches *after* the suspension is recorded
        // (e.g. from a try/except that caught the suspend) must be rejected
        // without invoking the real callback — deterministically, independent
        // of the asynchronous epoch interrupt.
        let state = Arc::new(Mutex::new(ReplayState::new(CallbackJournal::new("code"))));

        let (suspender, _) = programmable("approve", Outcome::Suspend("wait".into()));
        let suspend_wrapper = ReplayCallback::new(suspender, Arc::clone(&state));
        let _ = suspend_wrapper.invoke(json!({})).await;

        // A subsequent invocation — distinct from any in-flight gather sibling,
        // since this is initiated after `invoke` above already returned — is
        // rejected and never reaches the real callback.
        let (fetch, fetch_calls) = programmable("fetch", Outcome::Ok(json!("live")));
        let fetch_wrapper = ReplayCallback::new(fetch, Arc::clone(&state));
        let err = fetch_wrapper.invoke(json!({})).await.unwrap_err();
        assert!(matches!(err, CallbackError::Suspend(_)));
        assert_eq!(
            fetch_calls.load(Ordering::SeqCst),
            0,
            "callback dispatched after suspension must not run"
        );

        // Nothing new journaled.
        let st = lock_state(&state);
        assert!(st.build_journal("code").is_empty());
    }

    #[tokio::test]
    async fn replays_across_a_suspended_hole() {
        // Models resume after a concurrent `gather` where the suspender was
        // initiated first (seq 0, not journaled) and a sibling completed
        // (seq 1, journaled at index 1). The journal has a hole at index 0.
        let fetch_args = json!({"q": "x"});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![entry_ok(1, "fetch", &fetch_args, json!("cached"))],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));

        // seq 0: the retried suspender — a hole in the journal — runs live.
        let (approve, approve_calls) = programmable("approve", Outcome::Ok(json!("approved")));
        let approve_wrapper = ReplayCallback::new(approve, Arc::clone(&state));
        let r0 = approve_wrapper.invoke(json!({})).await.unwrap();
        assert_eq!(r0, json!("approved"));
        assert_eq!(
            approve_calls.load(Ordering::SeqCst),
            1,
            "suspender runs live"
        );

        // seq 1: the sibling matches index 1 across the hole and replays from
        // cache instead of re-running.
        let (fetch, fetch_calls) = programmable("fetch", Outcome::Ok(json!("LIVE")));
        let fetch_wrapper = ReplayCallback::new(fetch, Arc::clone(&state));
        let r1 = fetch_wrapper.invoke(fetch_args).await.unwrap();
        assert_eq!(r1, json!("cached"), "sibling replayed across the hole");
        assert_eq!(
            fetch_calls.load(Ordering::SeqCst),
            0,
            "completed sibling must not re-run"
        );

        let st = lock_state(&state);
        assert_eq!(st.replayed_count(), 1);
    }
}
