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
//! `Sandbox` values (the `SandboxFactory` pattern) share it. It holds
//! instances for at most [`WarmPool::MAX_COMPONENTS`] components, evicting
//! the least recently used, because a warm store keeps its component loaded
//! and executors built with `from_file`/`from_binary` each load their own. It
//! is only active
//! on a multi-threaded Tokio runtime, where the background work actually runs
//! in parallel; on a current-thread runtime it would just be added to the next
//! request's latency.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use tokio::runtime::{Handle, RuntimeFlavor};
use wasmtime::Store;

use crate::wasm::{ExecutorState, Sandbox, SandboxPre};

/// An instantiated store that has not run user code yet.
pub(crate) struct WarmInstance {
    pub(crate) store: Store<ExecutorState>,
    pub(crate) bindings: Sandbox,
}

struct Slot {
    ready: Vec<WarmInstance>,
    /// Replacements a background task has committed to instantiate.
    pending: usize,
    last_used: Instant,
}

impl Slot {
    fn new() -> Self {
        Slot {
            ready: Vec::new(),
            pending: 0,
            last_used: Instant::now(),
        }
    }
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

    /// Components to keep warm instances for.
    ///
    /// Usually there is one, the embedded runtime; the bound only matters
    /// when executors keep loading their own copy of a component.
    const MAX_COMPONENTS: usize = 4;

    pub(crate) fn global() -> &'static Self {
        static POOL: OnceLock<WarmPool> = OnceLock::new();
        POOL.get_or_init(|| {
            let target = match std::env::var("ERYX_WARM_INSTANCES") {
                Ok(v) => v.trim().parse().unwrap_or_else(|_| {
                    tracing::warn!(
                        value = %v,
                        default = Self::DEFAULT_TARGET,
                        "ignoring invalid ERYX_WARM_INSTANCES, expected a non-negative integer"
                    );
                    Self::DEFAULT_TARGET
                }),
                Err(_) => Self::DEFAULT_TARGET,
            };
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
        let slot = slots.get_mut(&key(pre))?;
        slot.last_used = Instant::now();
        let ready = &mut slot.ready;
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

    /// Dispose of a store that has run user code (if the caller has not
    /// already) and top the pool back up.
    ///
    /// Both happen on Tokio's blocking pool when the pool is active, so the
    /// slot reset and the CPU-bound instantiation (each around a millisecond)
    /// do not hold up the async workers serving other requests. Otherwise
    /// the store is dropped here.
    pub(crate) fn retire(
        &'static self,
        pre: &SandboxPre<ExecutorState>,
        mut store: Option<Store<ExecutorState>>,
    ) {
        if !self.active() {
            drop(store);
            return;
        }

        // The caller's handler tasks run until every sender of their channel
        // is gone, so release those now rather than when the background task
        // gets around to dropping the store.
        if let Some(store) = &mut store {
            store.data_mut().disconnect();
        }

        let (reservation, evicted) = self.reserve(key(pre));
        let pre = pre.clone();
        let handle = Handle::current();
        tokio::task::spawn_blocking(move || {
            // Dropping first returns the memory slot to wasmtime's pool, so the
            // replacement below can reuse it while it is still cache-hot.
            drop(store);
            drop(evicted);
            let Some(reservation) = reservation else {
                return;
            };
            match handle.block_on(instantiate_warm(&pre)) {
                Ok(instance) => reservation.fulfil(instance),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to pre-instantiate a warm instance");
                }
            }
        });
    }

    /// Commit to instantiating one more instance for `key` if the slot is
    /// below target, counting instances already in flight.
    ///
    /// Making room for a new component evicts the least recently used idle
    /// one; its instances are returned so the caller can drop them off the
    /// request path.
    fn reserve(&'static self, key: usize) -> (Option<Reservation>, Vec<WarmInstance>) {
        let Ok(mut slots) = self.slots.lock() else {
            return (None, Vec::new());
        };
        let mut evicted = Vec::new();
        if !slots.contains_key(&key) && slots.len() >= Self::MAX_COMPONENTS {
            let oldest = slots
                .iter()
                .filter(|(_, slot)| slot.pending == 0)
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(key, _)| *key);
            match oldest.and_then(|oldest| slots.remove(&oldest)) {
                Some(slot) => evicted = slot.ready,
                // Every slot is being refilled; try again on a later retire.
                None => return (None, evicted),
            }
        }
        let slot = slots.entry(key).or_insert_with(Slot::new);
        slot.last_used = Instant::now();
        if slot.ready.len() + slot.pending >= self.target {
            return (None, evicted);
        }
        slot.pending += 1;
        (Some(Reservation { pool: self, key }), evicted)
    }
}

/// A committed replacement for one slot.
///
/// Dropping it without [`fulfil`](Self::fulfil) (instantiation failed, or the
/// task was cancelled by runtime shutdown before it finished) releases the
/// commitment, so the slot can be refilled later.
struct Reservation {
    pool: &'static WarmPool,
    key: usize,
}

impl Reservation {
    fn fulfil(self, instance: WarmInstance) {
        if let Ok(mut slots) = self.pool.slots.lock()
            && let Some(slot) = slots.get_mut(&self.key)
        {
            slot.ready.push(instance);
        }
        // `Drop` releases the pending count.
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Ok(mut slots) = self.pool.slots.lock()
            && let Some(slot) = slots.get_mut(&self.key)
        {
            slot.pending = slot.pending.saturating_sub(1);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A replacement task that never runs to completion (its runtime shut
    /// down before polling it, or instantiation failed) must not leave the
    /// slot thinking one is still on its way.
    #[test]
    fn a_dropped_reservation_lets_the_slot_be_refilled() {
        let pool: &'static WarmPool = Box::leak(Box::new(WarmPool {
            target: 1,
            slots: Mutex::new(HashMap::new()),
        }));

        let (reservation, _) = pool.reserve(7);
        assert!(reservation.is_some());
        assert!(
            pool.reserve(7).0.is_none(),
            "the slot is already being refilled"
        );

        drop(reservation);
        assert!(pool.reserve(7).0.is_some());
    }
}
