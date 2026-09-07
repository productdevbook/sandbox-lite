//! Locks whose holder may panic.
//!
//! The release profile unwinds (issue #93), so a panic now travels out through whatever lock the
//! thread was holding and leaves it poisoned. `lock().unwrap()` on the next caller turns that one
//! failed request into a `500` for every tenant on the box for as long as the process lives — the
//! opposite of what the `catch_unwind` nets are for. Worse, two of the poisoned-lock callers are
//! `Drop` impls that run *during* an unwind (`Permit`, `Lead`), where a second panic is an abort.
//!
//! So the decision is taken here once rather than at seventy-odd call sites: every lock in this
//! daemon guards either a counter, a map of `Arc`s, or a `done` slot, and none of them holds an
//! invariant that spans an unlocked moment. The worst a panicking holder can leave behind is a
//! number that is one out or a map missing an entry — a wrong statistic or a recompile, against a
//! permanent outage. Taking the data back and carrying on is right for all of them, and the
//! `no_lock_poison_is_fatal` ratchet keeps the next lock from arriving with an `unwrap()`.
//!
//! Two locks are worth naming because the reasoning is not uniform:
//!
//! - `Gate::state` counts permits. `Permit::drop` runs while a panicking compile unwinds and must
//!   put its permit back; `unwrap()` there would abort instead.
//! - `Tenant::overlay` is held across the disk half of a write. A panic mid-`write_many` leaves the
//!   map matching what actually reached the directory, which is what a restart would read anyway.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

pub trait Held<T: ?Sized> {
    /// The guard, poisoned or not.
    fn held(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> Held<T> for Mutex<T> {
    fn held(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub trait Shared<T: ?Sized> {
    fn shared(&self) -> RwLockReadGuard<'_, T>;
    fn exclusive(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> Shared<T> for RwLock<T> {
    fn shared(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn exclusive(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

pub trait Waited {
    fn waited<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T>;
    /// The guard back, whether the wait ended on a notification or on the clock. Callers here all
    /// re-check the condition and their own deadline, so which of the two it was does not matter.
    fn waited_for<'a, T>(&self, guard: MutexGuard<'a, T>, timeout: Duration) -> MutexGuard<'a, T>;
}

impl Waited for Condvar {
    fn waited<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.wait(guard).unwrap_or_else(PoisonError::into_inner)
    }

    fn waited_for<'a, T>(&self, guard: MutexGuard<'a, T>, timeout: Duration) -> MutexGuard<'a, T> {
        self.wait_timeout(guard, timeout).unwrap_or_else(PoisonError::into_inner).0
    }
}

#[cfg(test)]
mod tests {
    use std::panic::AssertUnwindSafe;
    use std::sync::{Arc, Condvar, Mutex, RwLock};
    use std::time::Duration;

    use super::{Held, Shared, Waited};

    #[test]
    fn a_poisoned_mutex_still_hands_over_its_contents() {
        let m = Arc::new(Mutex::new(1u32));
        let holder = m.clone();
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut n = holder.held();
            *n = 2;
            panic!("while holding it");
        }));
        assert!(m.is_poisoned(), "the panic must have poisoned it, or this proves nothing");
        assert_eq!(*m.held(), 2);
    }

    #[test]
    fn a_poisoned_rwlock_still_reads_and_writes() {
        let l = Arc::new(RwLock::new(1u32));
        let holder = l.clone();
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _w = holder.exclusive();
            panic!("while holding it");
        }));
        assert!(l.is_poisoned());
        *l.exclusive() = 3;
        assert_eq!(*l.shared(), 3);
    }

    #[test]
    fn a_wait_on_a_poisoned_mutex_returns_rather_than_panicking() {
        let m = Arc::new(Mutex::new(false));
        let ready = Arc::new(Condvar::new());
        let holder = m.clone();
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _g = holder.held();
            panic!("while holding it");
        }));
        let guard = ready.waited_for(m.held(), Duration::from_millis(1));
        assert!(!*guard);
    }
}
