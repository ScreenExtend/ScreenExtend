use crossbeam_channel::{Sender, bounded, select, unbounded};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

pub mod protocol;

use self::protocol::InputEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayRect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
pub struct Stats {
    pub fast_received: AtomicU64,
    pub fast_dropped: AtomicU64,
}

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod backend;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod backend;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
#[path = "generic.rs"]
mod backend;

pub mod scancode;

pub fn boost_current_thread() {
    backend::boost_thread();
}

pub fn tune_process() {
    backend::tune_process();
}

pub const FAST_CAP: usize = 512;

#[derive(Clone)]
pub struct InjectorTx {
    pub fast: Sender<InputEvent>,
    pub reliable: Sender<InputEvent>,
    pub stats: Arc<Stats>,
}

impl InjectorTx {
    #[inline]
    pub fn route(&self, ev: InputEvent, hot: bool) {
        if hot {
            match self.fast.try_send(ev) {
                Ok(()) => { self.stats.fast_received.fetch_add(1, Ordering::Relaxed); }
                Err(_) => { self.stats.fast_dropped.fetch_add(1, Ordering::Relaxed); }
            }
        } else {
            let _ = self.reliable.send(ev);
        }
    }

    #[inline]
    pub fn fast_queue_depth(&self) -> usize {
        self.fast.len()
    }

    pub fn release_all(&self) {
        use self::protocol::Lifecycle;
        let _ = self.reliable.send(InputEvent::Lifecycle(Lifecycle::Focus(false)));
    }
}

pub fn spawn(device_name: Option<String>) -> (InjectorTx, JoinHandle<()>) {
    let (fast_tx, fast_rx) = bounded::<InputEvent>(FAST_CAP);
    let (rel_tx, rel_rx) = unbounded::<InputEvent>();
    let stats = Arc::new(Stats::default());

    let join = thread::Builder::new()
        .name("injector".into())
        .spawn(move || {
            backend::boost_thread();
            let mut inj = backend::Injector::new(device_name);
            log::info!("injector thread up ({})", backend::NAME);
            loop {
                select! {
                    recv(rel_rx) -> m => match m {
                        Ok(ev) => inj.dispatch(&ev),
                        Err(_) => break,
                    },
                    recv(fast_rx) -> m => match m {
                        Ok(ev) => inj.dispatch(&ev),
                        Err(_) => break,
                    },
                }
            }
            log::info!("injector shutting down: releasing all held input");
            inj.release_all();
        })
        .expect("spawn injector thread");

    (InjectorTx { fast: fast_tx, reliable: rel_tx, stats }, join)
}
