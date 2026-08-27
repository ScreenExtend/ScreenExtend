use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::macos_utils::audio::legacy::routing::{
    recover, DefaultChange, DefaultDevicePort, Router, RoutingState,
};

const OURS: &str = "app.screenextend.desktop.audio.device";

struct FakePort {
    current: RefCell<Option<String>>,
    fallback: Option<String>,
}

impl FakePort {
    fn new(initial: &str) -> Self {
        Self {
            current: RefCell::new(Some(initial.to_string())),
            fallback: None,
        }
    }
    fn with_fallback(initial: &str, fallback: &str) -> Self {
        Self {
            current: RefCell::new(Some(initial.to_string())),
            fallback: Some(fallback.to_string()),
        }
    }
    fn get(&self) -> Option<String> {
        self.current.borrow().clone()
    }
}

impl DefaultDevicePort for FakePort {
    fn current_default_uid(&self) -> Option<String> {
        self.current.borrow().clone()
    }
    fn set_default_uid(&self, uid: &str) -> bool {
        *self.current.borrow_mut() = Some(uid.to_string());
        true
    }
    fn fallback_output_uid(&self) -> Option<String> {
        self.fallback.clone()
    }
}

fn temp_state_path() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("se_routing_test_{}_{}.json", std::process::id(), n))
}

#[test]
fn activate_saves_previous_and_takes_over() {
    let path = temp_state_path();
    let port = FakePort::new("built-in-speakers");
    let mut router = Router::new(port, OURS.to_string(), path.clone());
    router.activate(1000).unwrap();

    let state = RoutingState::load(&path);
    assert_eq!(state.saved_uid, "built-in-speakers");
    assert!(state.changed);
    assert_eq!(state.timestamp, 1000);
    assert_eq!(router.saved_uid(), Some("built-in-speakers"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn restore_puts_the_saved_device_back_and_clears_flag() {
    let path = temp_state_path();
    let port = FakePort::new("headphones");
    let mut router = Router::new(port, OURS.to_string(), path.clone());
    router.activate(5).unwrap();
    router.restore();

    // persisted flag cleared; no longer active
    assert!(!RoutingState::load(&path).changed);
    assert!(!router.is_active());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn activate_never_saves_ourselves_as_the_device_to_restore() {
    let path = temp_state_path();
    let port = FakePort::with_fallback(OURS, "built-in-speakers");
    let mut router = Router::new(port, OURS.to_string(), path.clone());
    router.activate(1).unwrap();
    assert_ne!(router.saved_uid(), Some(OURS)); // restoring to ourselves would be a silent-device trap
    assert_eq!(router.saved_uid(), Some("built-in-speakers"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn user_switching_output_is_respected_not_fought() {
    let path = temp_state_path();
    let port = FakePort::new("built-in");
    let mut router = Router::new(port, OURS.to_string(), path.clone());
    router.activate(1).unwrap();

    router.set_default_for_test("external-dac");
    let decision = router.on_default_changed();
    assert_eq!(
        decision,
        DefaultChange::UserSwitchedAway {
            new_uid: "external-dac".to_string()
        }
    );
    assert!(!router.is_active());
    assert!(!RoutingState::load(&path).changed);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn default_change_back_to_us_is_ignored() {
    let path = temp_state_path();
    let port = FakePort::new("built-in");
    let mut router = Router::new(port, OURS.to_string(), path.clone());
    router.activate(1).unwrap();
    assert_eq!(router.on_default_changed(), DefaultChange::Ignore);
    assert!(router.is_active());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn crash_recovery_restores_when_we_died_holding_the_default() {
    let path = temp_state_path();
    RoutingState {
        saved_uid: "built-in-speakers".to_string(),
        changed: true,
        timestamp: 42,
    }
    .save(&path);
    let port = FakePort::new(OURS);

    let recovered = recover(&port, OURS, &path);
    assert!(recovered);
    assert_eq!(port.get().as_deref(), Some("built-in-speakers")); // restored
    assert!(!RoutingState::load(&path).changed); // flag cleared
    let _ = std::fs::remove_file(&path);
}

#[test]
fn crash_recovery_no_op_when_flag_unset() {
    let path = temp_state_path();
    RoutingState::default().save(&path); // changed = false
    let port = FakePort::new("something-else");
    assert!(!recover(&port, OURS, &path));
    assert_eq!(port.get().as_deref(), Some("something-else")); // untouched
    let _ = std::fs::remove_file(&path);
}

#[test]
fn crash_recovery_no_op_when_default_is_not_us() {
    let path = temp_state_path();
    RoutingState {
        saved_uid: "built-in".to_string(),
        changed: true,
        timestamp: 1,
    }
    .save(&path);
    let port = FakePort::new("external-dac");
    let recovered = recover(&port, OURS, &path);
    assert!(!recovered);
    // must not stomp the user's current choice
    assert_eq!(port.get().as_deref(), Some("external-dac"));
    let _ = std::fs::remove_file(&path);
}
