//! One way to take a `std::sync::Mutex` in the streamer core, and it is not `.unwrap()`.
//!
//! Every shared-state lock in the transport guards the same handful of `Arc<Mutex<…>>` values,
//! and those `Arc`s are shared across every network adapter's server and the cloud relay thread.
//! So one panic that unwinds while any of those guards is live poisons the lock for the *whole
//! host*, not for one session: after that, every `/whep`, `/reconfig`, `/leave` and every UI
//! command touching that state panics on `unwrap()`. axum catches panics per task, so the visible
//! symptom is not a crash but every request returning 500 and the device list going inert, with
//! nothing in the log that explains it.
//!
//! Recovering the guard is the right call here: the guarded data is plain maps, counters and
//! epochs. A torn update to one of those is far less harmful than a host that has to be
//! restarted. What must not happen is the recovery being silent, so the first one is logged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

static REPORTED: AtomicBool = AtomicBool::new(false);

#[cold]
fn report_poison() {
    if !REPORTED.swap(true, Ordering::Relaxed) {
        teprintln!(
            "[lock] recovered a poisoned mutex — a thread panicked while holding streamer state. \
             The host keeps serving, but something above this panicked and is worth finding."
        );
    }
}

/// How many times a poisoned lock has been recovered, for the telemetry surface and for tests.
static RECOVERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn poison_recoveries() -> u64 {
    RECOVERED.load(Ordering::Relaxed)
}

pub trait LockExt<T> {
    /// Take the lock, recovering the guard if a previous holder panicked.
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    #[inline]
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        match self.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                RECOVERED.fetch_add(1, Ordering::Relaxed);
                report_poison();
                poisoned.into_inner()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_poisoned_lock_is_recovered_rather_than_propagated() {
        let m = Arc::new(Mutex::new(vec![1u32, 2, 3]));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let mut g = m2.lock().unwrap();
            g.push(4);
            panic!("holder panicked");
        })
        .join();
        assert!(m.lock().is_err(), "the lock really is poisoned");

        let before = poison_recoveries();
        let g = m.lock_recover();
        assert_eq!(&*g, &[1, 2, 3, 4]);
        drop(g);
        assert!(poison_recoveries() > before);

        // And it stays usable: the host must not need a restart.
        m.lock_recover().push(5);
        assert_eq!(m.lock_recover().len(), 5);
    }

    #[test]
    fn a_healthy_lock_costs_nothing_extra() {
        let m = Mutex::new(7u32);
        assert_eq!(*m.lock_recover(), 7);
    }
}
