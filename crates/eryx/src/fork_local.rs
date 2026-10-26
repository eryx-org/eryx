//! Process-global state that a forked child starts afresh.

use std::sync::OnceLock;

/// Nested forks that each get their own value. Deeper descendants share the
/// last one, so their state must still check the pid itself.
const MAX_FORK_DEPTH: usize = 8;

/// A lazily created value per process in a chain of forks.
///
/// A forked child inherits the parent's memory but only the thread that
/// called `fork`, so a lock another parent thread held at that moment stays
/// held in the child forever. [`get`](Self::get) picks the value by pid
/// before any lock is taken and creates a fresh one for a new process,
/// without touching what the child inherited. Inherited values are never
/// dropped: their teardown (threads, wasmtime stores) belongs to the parent.
///
/// The one window left is a fork during the parent's very first `get`, while
/// its own value is being created.
pub(crate) struct ForkLocal<T> {
    slots: [OnceLock<(u32, T)>; MAX_FORK_DEPTH],
    init: fn() -> T,
}

impl<T> ForkLocal<T> {
    pub(crate) const fn new(init: fn() -> T) -> Self {
        Self {
            slots: [const { OnceLock::new() }; MAX_FORK_DEPTH],
            init,
        }
    }

    /// This process's value.
    pub(crate) fn get(&self) -> &T {
        self.get_for(std::process::id())
    }

    fn get_for(&self, pid: u32) -> &T {
        let [ancestors @ .., last] = &self.slots;
        for slot in ancestors {
            let (owner, value) = slot.get_or_init(|| (pid, (self.init)()));
            if *owner == pid {
                return value;
            }
        }
        &last.get_or_init(|| (pid, (self.init)())).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A child gets a fresh lock even when its parent's is held forever.
    #[test]
    fn a_new_pid_gets_a_fresh_value_without_locking_the_old_one() {
        static STATE: ForkLocal<Mutex<u32>> = ForkLocal::new(|| Mutex::new(0));

        let parent = STATE.get_for(100);
        *parent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = 7;
        // What a fork sees when another thread was inside the lock.
        std::mem::forget(parent.lock());

        let child = STATE.get_for(101);
        assert!(!std::ptr::eq(parent, child));
        assert!(
            matches!(child.try_lock().as_deref(), Ok(0)),
            "child lock is free"
        );

        assert!(std::ptr::eq(STATE.get_for(101), child));
        assert!(std::ptr::eq(STATE.get_for(100), parent));
    }

    /// Past the depth limit, descendants share the last value.
    #[test]
    fn deep_descendants_share_the_last_value() {
        static STATE: ForkLocal<u32> = ForkLocal::new(|| 0);
        let pids = 0..=u32::try_from(MAX_FORK_DEPTH).unwrap_or(u32::MAX);
        let values: Vec<_> = pids.map(|pid| STATE.get_for(pid) as *const u32).collect();
        assert_eq!(values[MAX_FORK_DEPTH - 1], values[MAX_FORK_DEPTH]);
        assert_ne!(values[0], values[1]);
    }
}
