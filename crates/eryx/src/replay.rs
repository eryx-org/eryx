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
//! mid-execution frames), we record each callback invocation and its result. On
//! resubmission the entire script is re-executed, but callbacks that match the
//! recorded journal short-circuit to the cached result instead of making a real
//! call. Because callbacks are the expensive part, and Python execution between
//! them is comparatively free, this is both fast and robust to arbitrary code
//! structure (loops, conditionals, nested functions) — the journal operates on
//! the callback invocations, not on the code.
//!
//! # Matching model: keyed FIFO multiset with a divergence guard
//!
//! Callbacks are matched by their **name plus canonicalized arguments**, treated
//! as a FIFO multiset. When a journal is loaded, every recorded result is bucketed
//! by its `(name, args)` key in recorded order; each live invocation pops the next
//! cached result for its key. Repeated identical calls therefore replay their
//! results in the order they were originally recorded.
//!
//! While replay is active, matching is **independent of invocation order**: a
//! concurrently launched batch of callbacks (e.g. `asyncio.gather`) replays
//! correctly no matter which future the scheduler polls first or which call
//! completes first, because a call is matched by what it *is*, not by its position
//! in the sequence.
//!
//! ## Divergence guard (safety)
//!
//! The first invocation that does *not* match any remaining cached result for its
//! `(name, args)` key — a **miss** — is treated as a divergence from the recorded
//! run: replay stops, and that call *and every subsequent call* run live for the
//! rest of the execution (`live_mode` is sticky). This is the safety property.
//!
//! Without it, keyed matching alone could replay a **stale** result across a real
//! divergence. Consider a script edited to write before it reads:
//!
//! ```text
//! run 1:                          run 2 (edited):
//! y = await read_counter()  # 5     await set_counter(10)   # new key -> miss
//!                                   y = await read_counter() # args {} -> would
//!                                                            # pop cached 5, but
//!                                                            # the true value is 10
//! ```
//!
//! The `set_counter(10)` call is a new key and misses. The guard then forces the
//! following `read_counter()` to run live (returning the true `10`) instead of
//! replaying the now-stale cached `5`. The `gather` win is unaffected: in that
//! case every call is a hit, so no miss fires and all calls still replay.
//!
//! A caller that signs journals and binds the signature to the exact script (as
//! the `eryx-server` layer does) will reject an edited script's journal *before*
//! it reaches this matching logic, restricting replay to re-runs of the same
//! script. Whether edited scripts can replay is therefore a property of the
//! integrity policy layered on top, not of this module.
//!
//! ## Concurrent identity is not part of the contract
//!
//! The replay matching identity is exactly **(callback name, canonical args)**.
//! Repeated *concurrent* calls that share an identity are therefore
//! **indistinguishable to replay**: FIFO ordering of cached results is guaranteed
//! for *sequential* identical calls, but it is **not** a stable per-task identity
//! for *concurrent* identical calls. A script may behave correctly against a
//! particular live scheduler (e.g. relying on which `gather` task happened to get
//! which result), but replay cannot preserve that per-task assignment without an
//! invocation id, so **stable assignment of concurrent identical calls is outside
//! the replay contract**. Callers that need a stable assignment must make each
//! call's identity unique — include a **nonce or correlation key in the callback
//! args** — so the calls no longer share a key.
//!
//! Replay is implemented entirely as a [`Callback`] wrapper ([`ReplayCallback`]):
//! no changes are needed to the WASM runtime, the WIT interface, or the Python
//! code. The wrapper consults a shared [`ReplayState`] before delegating to the
//! real callback.
//!
//! # Suspension
//!
//! A callback can return [`CallbackError::Suspend`] to defer its work
//! ("retry later"). When that happens the wrapper records a [`SuspendedCallback`]
//! (name, args, opaque reason) but does **not** journal the call, and the
//! suspension propagates back to the host import, which poisons the WASM fuel to
//! halt the guest synchronously. The suspended call therefore re-runs live when a
//! later run replays the recorded prefix. Two layers guarantee no further
//! callback runs after a suspension:
//!
//! 1. A **synchronous gate**: once any callback has suspended, every callback
//!    *dispatched after* that point funneling through [`ReplayState`] is rejected
//!    (the internal `Decision::AlreadySuspended` path), covering later `gather`
//!    siblings.
//! 2. The **fuel-poison halt** in the host import, which traps the guest before
//!    it can dispatch any further callback or perform I/O.
//!
//! # Determinism
//!
//! Replay short-circuits callbacks but re-executes the Python *between* them
//! live on every run, so it reproduces callback *results*, not whole-program
//! state. Script-level nondeterminism — an unseeded `random`, wall-clock time,
//! anything that varies run to run — is recomputed each time. If it feeds
//! callback arguments, the recomputed args miss and the divergence guard falls
//! back to live execution; if it drives control flow, the replayed run may
//! dispatch a different set of callbacks; and values the script computes itself
//! (rather than via a callback) are not reproduced. The guard keeps this
//! *safe* — a miss never replays a stale result — but replay is only fully
//! *transparent* for scripts whose callback names, arguments, and control flow
//! are deterministic given the same callback results. Routing a nondeterministic
//! input through a callback records it in the journal, so it replays
//! deterministically like any other result.
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

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::callback::{Callback, CallbackError};
use crate::schema::Schema;

/// The outcome of a callback invocation, as understood by replay.
///
/// This mirrors what a callback can communicate back through the [`Callback`]
/// trait: a success value, a permanent error, or a request to suspend. Only
/// [`CallbackOutcome::Ok`] and [`CallbackOutcome::Err`] are ever journaled so a
/// later replay reproduces the same result; [`CallbackOutcome::Suspend`]
/// terminates execution before an entry is recorded, so the suspended call is
/// re-attempted live on the resuming run.
#[derive(Debug, Clone)]
pub enum CallbackOutcome {
    /// The callback succeeded. The value is returned to Python and journaled.
    Ok(serde_json::Value),
    /// The callback failed permanently. Python receives an exception and the
    /// error is journaled (so a later replay reproduces the same failure).
    Err(String),
    /// The callback asked to suspend (via [`CallbackError::Suspend`]). Execution
    /// stops, the suspension is surfaced to the caller, and nothing is journaled
    /// for this call. The reason string is opaque to eryx.
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
    /// `Err` carries the error message exactly as Python observed it.
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
///
/// Recorded when a callback returns [`CallbackError::Suspend`]; surfaced to the
/// caller (e.g. via [`ReplayOutcome::suspended`](crate::ReplayOutcome::suspended))
/// so it can act on the reason and later resume by re-executing with the journal.
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
/// execution. While replay is active, matching is by `(name, canonical args)` as
/// a FIFO multiset, so it is independent of the order in which callbacks are
/// initiated or completed. The first miss trips the sticky divergence guard
/// (`live_mode`), after which all calls run live.
#[derive(Debug)]
pub struct ReplayState {
    /// Cached results from the previous journal, bucketed by `(name, args_json)`
    /// key. Each deque holds the recorded results for that key in recorded order;
    /// a live invocation pops the next one (FIFO), so repeated identical calls
    /// replay in their original order.
    cached: HashMap<(String, String), VecDeque<Result<serde_json::Value, String>>>,
    /// Sticky divergence guard. Once any invocation misses (its `(name, args)` key
    /// has no remaining cached result), the run is considered to have diverged
    /// from the journal: this is set and every subsequent invocation runs live,
    /// preventing a later same-key call from replaying a now-stale cached result.
    live_mode: bool,
    /// Monotonic counter assigning each invocation its recording index. This is
    /// only an entry index/order tag — matching no longer depends on it.
    next_seq: usize,
    /// Entries recorded for *this* execution, indexed by sequence number.
    /// `None` marks a reserved-but-unrecorded slot (in-flight or suspended).
    entries: Vec<Option<CallbackJournalEntry>>,
    /// Set if a callback suspended; only the first suspension is recorded. Once
    /// set, every subsequent invocation is rejected (`Decision::AlreadySuspended`)
    /// so no further callback runs after a suspension.
    suspended: Option<SuspendedCallback>,
    /// Initiation sequence of the suspending callback, if any. Concurrently
    /// dispatched siblings (e.g. an `asyncio.gather` batch) reserve sequence
    /// numbers around the suspending call and may complete and record *after* it;
    /// the built journal drops every entry at or after this sequence so it is a
    /// clean prefix that ends before the suspension point.
    suspend_seq: Option<usize>,
    /// How many invocations were served from the previous journal.
    replayed_count: u32,
}

/// The decision made for a single invocation while holding the state lock.
enum Decision {
    /// Served from the journal; the result is returned without a live call.
    Hit(Result<serde_json::Value, String>),
    /// Not in the journal (key absent or exhausted); invoke live and record at `seq`.
    Miss { seq: usize },
    /// A previous callback already suspended; reject this invocation without
    /// invoking the real callback or recording anything.
    ///
    /// This is the deterministic guarantee that no side-effecting callback runs
    /// after a suspension: because every invocation funnels through `decide`,
    /// any callback the guest dispatches *after* the suspension is rejected here.
    /// Fuel poisoning already halts the guest synchronously at the suspending
    /// callback's import, so in practice no further callback is dispatched; this
    /// gate is a belt-and-suspenders guard that also rejects any concurrent
    /// `asyncio.gather` sibling dispatched *after* the suspension is recorded. (A
    /// sibling that was already past `decide` completes and is journaled normally;
    /// `build_journal` then truncates it back out of the recorded prefix.)
    AlreadySuspended,
}

impl ReplayState {
    /// Create fresh replay state that replays from `previous`.
    ///
    /// The previous journal's entries are bucketed by `(name, args_json)` key in
    /// recorded order, so later invocations match regardless of the order they
    /// run in. Pass an empty journal (see [`CallbackJournal::new`]) to record a
    /// fresh journal without replaying anything.
    #[must_use]
    pub fn new(previous: CallbackJournal) -> Self {
        let mut cached: HashMap<(String, String), VecDeque<Result<serde_json::Value, String>>> =
            HashMap::new();
        for entry in previous.entries {
            cached
                .entry((entry.name, entry.args_json))
                .or_default()
                .push_back(entry.result);
        }
        Self {
            cached,
            live_mode: false,
            next_seq: 0,
            entries: Vec::new(),
            suspended: None,
            suspend_seq: None,
            replayed_count: 0,
        }
    }

    /// Decide how to handle an invocation by matching on `(name, args)`.
    ///
    /// This runs synchronously when the invocation is dispatched. While replay is
    /// active it pops the next cached result for the `(name, args_json)` key, so
    /// it is independent of the order callbacks are initiated or complete in (e.g.
    /// a concurrent `asyncio.gather` batch replays correctly regardless of
    /// scheduling). The first miss (a key with no remaining cached result) trips
    /// the sticky divergence guard, after which every invocation runs live so a
    /// later same-key call cannot replay a now-stale result.
    fn decide(&mut self, name: &str, args_hash: u64, args_json: &str) -> Decision {
        // Once any callback has suspended, reject every subsequent invocation
        // without invoking the real callback or recording anything. No seq is
        // reserved, so the rejected call leaves no hole in the journal.
        if self.suspended.is_some() {
            return Decision::AlreadySuspended;
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        self.ensure_slot(seq);

        // Once diverged, everything runs live.
        if self.live_mode {
            return Decision::Miss { seq };
        }

        // Match by name + canonical args, popping the next cached result for the key.
        let key = (name.to_string(), args_json.to_string());
        if let Some(queue) = self.cached.get_mut(&key)
            && let Some(result) = queue.pop_front()
        {
            self.replayed_count += 1;
            self.entries[seq] = Some(CallbackJournalEntry {
                index: u32::try_from(seq).unwrap_or(u32::MAX),
                name: name.to_string(),
                args_hash,
                args_json: args_json.to_string(),
                result: result.clone(),
            });
            return Decision::Hit(result);
        }

        // First miss for this key (absent or exhausted): the run has diverged from
        // the journal. Trip the sticky guard so this call and all later ones run
        // live, preventing replay of a stale cached result after the divergence.
        self.live_mode = true;
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

    /// Record that a callback suspended at initiation sequence `seq`. Only the
    /// first suspension is kept; the suspended call itself is *not* journaled (so
    /// it re-runs live on resume). The sequence is remembered so `build_journal`
    /// can truncate entries recorded by concurrent siblings after this point.
    fn record_suspend(&mut self, seq: usize, suspended: SuspendedCallback) {
        if self.suspended.is_none() {
            self.suspended = Some(suspended);
            self.suspend_seq = Some(seq);
        }
    }

    /// The callback that suspended this execution, if any.
    #[must_use]
    pub fn suspended(&self) -> Option<&SuspendedCallback> {
        self.suspended.as_ref()
    }

    /// Number of invocations served from the previous journal.
    #[must_use]
    pub fn replayed_count(&self) -> u32 {
        self.replayed_count
    }

    /// Build the journal recorded during this run for the given script.
    ///
    /// Reserved-but-unrecorded slots (e.g. in-flight callbacks) are dropped;
    /// recorded entries keep their dispatch-order positions. If a callback
    /// suspended, entries initiated at or after the suspending call are also
    /// dropped — a concurrently dispatched sibling may have completed and
    /// recorded around the suspension point, but the journal must be a clean
    /// prefix that ends before the suspension so a later run replays only what
    /// truly preceded it.
    #[must_use]
    pub fn build_journal(&self, code: impl Into<String>) -> CallbackJournal {
        let entries = self
            .entries
            .iter()
            .flatten()
            .filter(|entry| match self.suspend_seq {
                Some(seq) => (entry.index as usize) < seq,
                None => true,
            })
            .cloned()
            .collect();
        CallbackJournal {
            code: code.into(),
            entries,
        }
    }
}

/// Wraps a real callback with journal-aware replay logic.
///
/// All wrapped callbacks in a single execution share one [`ReplayState`], so an
/// invocation of any callback can match a cached result recorded for the same
/// `(name, args)` key, regardless of which wrapper records it.
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
        // returned. Matching is keyed on (name, args), so the result is the same
        // regardless of the order in which concurrent invocations are dispatched.
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
                            // Do not journal a suspended callback: only record
                            // that it suspended so the caller can act on it. The
                            // call re-runs live when execution resumes.
                            lock_state(&state).record_suspend(
                                seq,
                                SuspendedCallback {
                                    name,
                                    args_json,
                                    reason,
                                },
                            );
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
    fn outcome_classifies_ok_err() {
        assert!(matches!(
            CallbackOutcome::from_invoke(&Ok(json!(1))),
            CallbackOutcome::Ok(_)
        ));
        assert!(matches!(
            CallbackOutcome::from_invoke(&Err(CallbackError::ExecutionFailed("x".into()))),
            CallbackOutcome::Err(_)
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
    async fn first_miss_switches_to_live_mode() {
        // Divergence guard: the journal has keys [A, B]. A NEW key "C" is invoked
        // first (miss -> live), then "A" — which has a cached result — must STILL
        // run live, because the first miss made live-mode sticky.
        let a_args = json!({"id": "A"});
        let b_args = json!({"id": "B"});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![
                entry_ok(0, "a", &a_args, json!("cached-a")),
                entry_ok(1, "b", &b_args, json!("cached-b")),
            ],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));

        // New key "C": not in the journal -> miss -> trips the guard.
        let (c, c_calls) = programmable("c", Outcome::Ok(json!("live-c")));
        let c_wrapper = ReplayCallback::new(c, Arc::clone(&state));
        let rc = c_wrapper.invoke(json!({"id": "C"})).await.unwrap();
        assert_eq!(rc, json!("live-c"));
        assert_eq!(c_calls.load(Ordering::SeqCst), 1);

        // "a" has a cached result, but the guard is sticky, so it runs LIVE.
        let (a, a_calls) = programmable("a", Outcome::Ok(json!("live-a")));
        let a_wrapper = ReplayCallback::new(a, Arc::clone(&state));
        let ra = a_wrapper.invoke(a_args.clone()).await.unwrap();
        assert_eq!(
            ra,
            json!("live-a"),
            "a runs live after divergence, not replayed"
        );
        assert_eq!(a_calls.load(Ordering::SeqCst), 1, "a actually invoked");

        let st = lock_state(&state);
        assert_eq!(
            st.replayed_count(),
            0,
            "nothing replayed after the first miss"
        );
    }

    #[tokio::test]
    async fn write_miss_before_read_prevents_stale_replay() {
        // Concrete stale-replay-prevention shape: run 1 recorded only a read
        // (read_counter {} -> 5). Run 2 inserts a write (set_counter {value:10})
        // before the read. The write is a new key (miss -> live), so the guard
        // forces the subsequent read to run live (true value 10) instead of
        // replaying the stale cached 5.
        let read_args = json!({});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![entry_ok(0, "read_counter", &read_args, json!(5))],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));

        // The new write call misses and trips the guard.
        let (write, write_calls) = programmable("set_counter", Outcome::Ok(json!("ok")));
        let write_wrapper = ReplayCallback::new(write, Arc::clone(&state));
        let _ = write_wrapper.invoke(json!({"value": 10})).await.unwrap();
        assert_eq!(write_calls.load(Ordering::SeqCst), 1);

        // The read would otherwise pop the cached 5, but must run live now.
        let (read, read_calls) = programmable("read_counter", Outcome::Ok(json!(10)));
        let read_wrapper = ReplayCallback::new(read, Arc::clone(&state));
        let r = read_wrapper.invoke(read_args.clone()).await.unwrap();
        assert_eq!(
            r,
            json!(10),
            "read runs live, returning the true post-write value"
        );
        assert_eq!(
            read_calls.load(Ordering::SeqCst),
            1,
            "read not replayed from stale cache"
        );

        assert_eq!(lock_state(&state).replayed_count(), 0);
    }

    #[tokio::test]
    async fn matching_is_order_independent() {
        // Journal recorded A then B (distinct keys). Invoke the wrappers in the
        // REVERSED order (B then A): both must still replay from cache, proving
        // matching does not depend on invocation order (the concurrent-gather case).
        let a_args = json!({"id": "A"});
        let b_args = json!({"id": "B"});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![
                entry_ok(0, "a", &a_args, json!("cached-a")),
                entry_ok(1, "b", &b_args, json!("cached-b")),
            ],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));

        let (a, a_calls) = programmable("a", Outcome::Ok(json!("live-a")));
        let (b, b_calls) = programmable("b", Outcome::Ok(json!("live-b")));
        let a_wrapper = ReplayCallback::new(a, Arc::clone(&state));
        let b_wrapper = ReplayCallback::new(b, Arc::clone(&state));

        // Reversed: B first, then A.
        let rb = b_wrapper.invoke(b_args.clone()).await.unwrap();
        let ra = a_wrapper.invoke(a_args.clone()).await.unwrap();

        assert_eq!(rb, json!("cached-b"), "B replayed out of order");
        assert_eq!(ra, json!("cached-a"), "A replayed out of order");
        assert_eq!(b_calls.load(Ordering::SeqCst), 0, "B not invoked live");
        assert_eq!(a_calls.load(Ordering::SeqCst), 0, "A not invoked live");

        let st = lock_state(&state);
        assert_eq!(st.replayed_count(), 2);
    }

    #[tokio::test]
    async fn repeated_identical_calls_replay_fifo() {
        // Two identical-key entries with distinct results must replay in recorded
        // (FIFO) order across repeated invocations.
        let args = json!({"q": "x"});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![
                entry_ok(0, "fetch", &args, json!("first")),
                entry_ok(1, "fetch", &args, json!("second")),
            ],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));
        let (inner, calls) = programmable("fetch", Outcome::Ok(json!("live")));

        let w1 = ReplayCallback::new(Arc::clone(&inner) as Arc<dyn Callback>, Arc::clone(&state));
        let r1 = w1.invoke(args.clone()).await.unwrap();
        let w2 = ReplayCallback::new(inner as Arc<dyn Callback>, Arc::clone(&state));
        let r2 = w2.invoke(args.clone()).await.unwrap();

        assert_eq!(r1, json!("first"));
        assert_eq!(r2, json!("second"));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "neither invoked live");
        assert_eq!(lock_state(&state).replayed_count(), 2);
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

    // ---- suspension ---------------------------------------------------------

    #[test]
    fn outcome_classifies_suspend() {
        assert!(matches!(
            CallbackOutcome::from_invoke(&Err(CallbackError::Suspend("wait".into()))),
            CallbackOutcome::Suspend(_)
        ));
    }

    #[tokio::test]
    async fn suspend_records_metadata_without_journaling() {
        // The replay layer records the suspension metadata and does NOT journal
        // the suspended callback (the actual guest halt is the fuel-poison in the
        // host import, exercised by the integration tests).
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
        // (e.g. an in-flight gather sibling) must be rejected without invoking
        // the real callback — deterministically, the gate alongside fuel-poison.
        let state = Arc::new(Mutex::new(ReplayState::new(CallbackJournal::new("code"))));

        let (suspender, _) = programmable("approve", Outcome::Suspend("wait".into()));
        let suspend_wrapper = ReplayCallback::new(suspender, Arc::clone(&state));
        let _ = suspend_wrapper.invoke(json!({})).await;

        let (fetch, fetch_calls) = programmable("fetch", Outcome::Ok(json!("live")));
        let fetch_wrapper = ReplayCallback::new(fetch, Arc::clone(&state));
        let err = fetch_wrapper.invoke(json!({})).await.unwrap_err();
        assert!(matches!(err, CallbackError::Suspend(_)));
        assert_eq!(
            fetch_calls.load(Ordering::SeqCst),
            0,
            "callback dispatched after suspension must not run"
        );

        // Nothing new journaled by the rejected call.
        let st = lock_state(&state);
        assert!(st.build_journal("code").is_empty());
    }

    #[test]
    fn build_journal_truncates_entries_after_suspend_point() {
        // Three callbacks are dispatched concurrently, reserving seq 0, 1, 2. The
        // seq-0 and seq-2 calls complete and record; the seq-1 call suspends. The
        // built journal must be a clean prefix: only the entry initiated *before*
        // the suspending call (seq 0) is kept. The seq-2 sibling that happened to
        // complete around the suspension is dropped, so a later resume does not
        // replay/skip a callback that logically followed the suspension point.
        let mut st = ReplayState::new(CallbackJournal::new("code"));
        let args = json!({});
        let h = fnv1a_64(canonical_json(&args).as_bytes());

        assert!(matches!(st.decide("a", h, "{}"), Decision::Miss { seq: 0 }));
        assert!(matches!(st.decide("b", h, "{}"), Decision::Miss { seq: 1 }));
        assert!(matches!(st.decide("c", h, "{}"), Decision::Miss { seq: 2 }));

        st.record_live(entry_ok(0, "a", &args, json!("a-done")));
        st.record_live(entry_ok(2, "c", &args, json!("c-done")));
        st.record_suspend(
            1,
            SuspendedCallback {
                name: "b".into(),
                args_json: "{}".into(),
                reason: "wait".into(),
            },
        );

        let journal = st.build_journal("code");
        assert_eq!(
            journal.entries.len(),
            1,
            "only the pre-suspend prefix is kept"
        );
        assert_eq!(journal.entries[0].index, 0);
        assert_eq!(journal.entries[0].name, "a");
    }

    #[tokio::test]
    async fn resume_replays_prefix_and_reruns_suspended_call_live() {
        // Run 1 completed `fetch` (journaled) then `approve` suspended (not
        // journaled). On resume the journal has only `fetch`; keyed matching
        // replays it from cache while `approve` runs live again.
        let fetch_args = json!({"q": "x"});
        let previous = CallbackJournal {
            code: "code".into(),
            entries: vec![entry_ok(0, "fetch", &fetch_args, json!("cached"))],
        };
        let state = Arc::new(Mutex::new(ReplayState::new(previous)));

        // `fetch` replays from cache (not invoked live).
        let (fetch, fetch_calls) = programmable("fetch", Outcome::Ok(json!("LIVE")));
        let fetch_wrapper = ReplayCallback::new(fetch, Arc::clone(&state));
        let rf = fetch_wrapper.invoke(fetch_args.clone()).await.unwrap();
        assert_eq!(rf, json!("cached"), "fetch replayed from cache");
        assert_eq!(
            fetch_calls.load(Ordering::SeqCst),
            0,
            "fetch not re-invoked"
        );

        // `approve` is not in the journal -> runs live, and this time succeeds.
        let (approve, approve_calls) = programmable("approve", Outcome::Ok(json!("approved")));
        let approve_wrapper = ReplayCallback::new(approve, Arc::clone(&state));
        let ra = approve_wrapper.invoke(json!({})).await.unwrap();
        assert_eq!(ra, json!("approved"));
        assert_eq!(approve_calls.load(Ordering::SeqCst), 1, "approve runs live");

        let st = lock_state(&state);
        assert_eq!(st.replayed_count(), 1, "only fetch replayed");
        assert!(st.suspended().is_none(), "no suspension on the resume run");
    }
}
