//! Pre-instantiated ("warm") stores for stateless execution.
//!
//! Instantiating the runtime component costs far more than running a short
//! script on it. Wasmtime has to build a `VMFuncRef` for every entry of the
//! dynamically linked modules' function tables (their element segments are
//! offset by an imported `__table_base`, so they cannot be initialised
//! lazily), and dropping the store afterwards resets the linear-memory slot.
//! For `pass` that is roughly 80% of the wall time.
//!
//! [`WarmPool`] keeps instantiated stores ready so [`crate::Sandbox::execute`]
//! can pick one up immediately. The used store is handed to a background task
//! that drops it and instantiates a replacement, moving both costs off the
//! request path. Isolation is unchanged: every execution still runs on an
//! instance that has never run user code.
//!
//! The pool is process-global and keyed by component, so short-lived
//! `Sandbox` values (the `SandboxFactory` pattern) share it. It is only active
//! on a multi-threaded Tokio runtime, where the background work actually runs
//! in parallel; on a current-thread runtime it would just be added to the next
//! request's latency.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::runtime::{Handle, RuntimeFlavor};
use wasmtime::Store;

use crate::wasm::{ExecutorState, Sandbox, SandboxPre};

/// An instantiated store that has not run user code yet.
pub(crate) struct WarmInstance {
    pub(crate) store: Store<ExecutorState>,
    pub(crate) bindings: Sandbox,
}

#[derive(Default)]
struct Slot {
    ready: Vec<WarmInstance>,
    /// Replacements a background task has committed to instantiate.
    pending: usize,
}

/// Process-global pool of warm instances, one slot per component.
pub(crate) struct WarmPool {
    /// Instances to keep ready per component; `0` disables the pool.
    target: usize,
    slots: Mutex<HashMap<usize, Slot>>,
}

impl WarmPool {
    /// Default number of ready instances per component.
    ///
    /// One is enough to hide instantiation from a caller that executes
    /// serially, which is the `SandboxFactory` render pattern; concurrent
    /// callers should raise `ERYX_WARM_INSTANCES` towards their concurrency.
    const DEFAULT_TARGET: usize = 1;

    pub(crate) fn global() -> &'static Self {
        static POOL: OnceLock<WarmPool> = OnceLock::new();
        POOL.get_or_init(|| {
            let target = std::env::var("ERYX_WARM_INSTANCES")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(Self::DEFAULT_TARGET);
            WarmPool {
                target,
                slots: Mutex::new(HashMap::new()),
            }
        })
    }

    /// Whether this call site can use the pool.
    fn active(&self) -> bool {
        self.target > 0
            && Handle::try_current()
                .is_ok_and(|handle| handle.runtime_flavor() == RuntimeFlavor::MultiThread)
    }

    /// Take a ready instance for `pre` whose instantiation fits in `memory_limit`.
    ///
    /// A limit below the snapshot's baseline has to fail the way a fresh
    /// instantiation does, so such executions are left to the cold path.
    pub(crate) fn take(
        &self,
        pre: &SandboxPre<ExecutorState>,
        memory_limit: Option<u64>,
    ) -> Option<WarmInstance> {
        if !self.active() {
            return None;
        }
        let mut slots = self.slots.lock().ok()?;
        let ready = &mut slots.get_mut(&key(pre))?.ready;
        let baseline = ready
            .last()?
            .store
            .data()
            .memory_tracker
            .peak_memory_bytes();
        if memory_limit.is_some_and(|limit| baseline > limit) {
            return None;
        }
        ready.pop()
    }

    /// Number of ready instances for `pre`.
    pub(crate) fn ready(&self, pre: &SandboxPre<ExecutorState>) -> usize {
        self.slots
            .lock()
            .ok()
            .and_then(|slots| slots.get(&key(pre)).map(|slot| slot.ready.len()))
            .unwrap_or(0)
    }

    /// Dispose of a store that has run user code and top the pool back up.
    ///
    /// Both happen on a background task when the pool is active; otherwise
    /// the store is dropped here.
    pub(crate) fn retire(
        &'static self,
        pre: &SandboxPre<ExecutorState>,
        mut store: Store<ExecutorState>,
    ) {
        if !self.active() {
            drop(store);
            return;
        }

        // The caller's handler tasks run until every sender of their channel
        // is gone, so release those now rather than when the background task
        // gets around to dropping the store.
        store.data_mut().disconnect();

        let key = key(pre);
        let replenish = self.reserve(key);
        let pre = pre.clone();
        tokio::spawn(async move {
            // Dropping first returns the memory slot to wasmtime's pool, so the
            // replacement below can reuse it while it is still cache-hot.
            drop(store);
            if !replenish {
                return;
            }
            match instantiate_warm(&pre).await {
                Ok(instance) => self.fulfil(key, Some(instance)),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to pre-instantiate a warm instance");
                    self.fulfil(key, None);
                }
            }
        });
    }

    /// Commit to instantiating one more instance for `key` if the slot is
    /// below target, counting instances already in flight.
    fn reserve(&self, key: usize) -> bool {
        let Ok(mut slots) = self.slots.lock() else {
            return false;
        };
        let slot = slots.entry(key).or_default();
        if slot.ready.len() + slot.pending >= self.target {
            return false;
        }
        slot.pending += 1;
        true
    }

    fn fulfil(&self, key: usize, instance: Option<WarmInstance>) {
        let Ok(mut slots) = self.slots.lock() else {
            return;
        };
        let slot = slots.entry(key).or_default();
        slot.pending = slot.pending.saturating_sub(1);
        if let Some(instance) = instance {
            slot.ready.push(instance);
        }
    }
}

/// Identify the component behind `pre`.
///
/// The compiled image's address is unique for as long as the component is
/// loaded, and a warm store keeps its component loaded.
fn key(pre: &SandboxPre<ExecutorState>) -> usize {
    pre.instance_pre().component().image_range().start as usize
}

async fn instantiate_warm(pre: &SandboxPre<ExecutorState>) -> Result<WarmInstance, crate::Error> {
    let (store, bindings) =
        crate::wasm::instantiate_store(pre, ExecutorState::placeholder(), u64::MAX).await?;
    Ok(WarmInstance { store, bindings })
}
