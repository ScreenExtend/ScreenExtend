//! Virtual displays where `CGVirtualDisplay` does not exist (before 10.14.4).
//!
//! A minimal fake AirPlay receiver is advertised, the Mac's own AirPlay picker is driven at it,
//! and macOS creates the extended-desktop display, which is then captured like any other
//! `CGDirectDisplayID`. The mirroring stream is read and discarded (no decode, no decrypt): a
//! receiver that stops reading back-pressures the sender into resetting the session.
//!
//! This tier is below the app's 10.15 floor and is kept only as a safety net.

pub mod ax;
pub mod cf_sys;
pub mod dnssd;
pub mod fairplay;
pub mod info;
pub mod mirror;
pub mod receiver;
pub mod rtsp;
pub mod sender_prefs;
pub mod topology;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::streamer::session::{SharedVirtualDisplay, VirtualDisplayController};

use info::Geometry;
use receiver::{Phase, Receiver};

/// Minimal cancellation token (`tokio-util` is not a dependency).
#[derive(Clone, Default)]
pub struct Cancel {
    notify: Arc<Notify>,
    flag: Arc<AtomicBool>,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            self.notify.notified().await;
        }
    }
}

/// A runtime of its own: `remove_all_displays` runs on a bare `std::thread` at shutdown, where
/// there is no runtime context to borrow.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("se-airplay")
            .build()
            .expect("failed to build the AirPlay receiver runtime")
    })
}

/// Total budget for advertise, picker press, session and display attach.
///
/// Far above the server's 5 s `DISPLAY_ATTACH_TIMEOUT`: an AirPlay session is a
/// network handshake plus a WindowServer reconfiguration, not a synchronous call.
const SESSION_BUDGET: Duration = Duration::from_secs(40);
/// How long to let mDNSResponder settle after a *cold* publish. Only paid when
/// the receiver was not already warm (see [`prewarm`]).
///
/// Only the registration needs covering: `ax::connect_to` holds the menu open
/// for its whole `DEVICE_ROW_TIMEOUT` while the picker fills in device rows.
const DISCOVERY_SETTLE: Duration = Duration::from_millis(500);
/// How long the display gets to disappear after the session is dropped.
const REMOVAL_TIMEOUT: Duration = Duration::from_secs(8);
/// A second, much shorter window after handing the configuration back.
///
/// Short because a display that outlives its session is inert: the next
/// `create_display` adopts the same id.
const REMOVAL_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Wall-clock budget for tearing a session down.
///
/// One budget for the whole teardown, not one per sub-step: it drives the menu
/// bar on a thread with no runtime while the app is trying to quit.
const TEARDOWN_BUDGET: Duration = Duration::from_secs(10);

struct Session {
    name: String,
    requested: Geometry,
    /// The mode macOS chose on its own, before the display was moved onto the
    /// one the client asked for. Restored on teardown (see [`remove_display`]).
    granted: (u32, u32),
    /// The geometry the display actually settled on, in monitor coordinates.
    ///
    /// This tier *negotiates*: the client asks for a size, macOS publishes a fixed ladder and
    /// `closest_mode` picks from it. The server must use this value, not force a
    /// `CGDisplaySetDisplayMode` to a size that is not on the ladder.
    settled: (u32, u32),
    /// What every display was doing before the session, so the Mac's own screen
    /// can be put back after macOS mirrors and un-mirrors it.
    modes_before: topology::ModeSnapshot,
}

#[derive(Default)]
struct SessionRegistry {
    /// At most one entry (AirPlay allows one display session at a time); a map to mirror
    /// `virtual_display`'s registry.
    sessions: HashMap<u32, Session>,
}

/// The receiver, kept alive across sessions.
///
/// Standing one up costs a Bonjour publish plus the sender's rescan (several seconds), and
/// nothing about it is per-client: the advertised geometry is a constant because macOS keeps
/// only one bit of it.
fn warm_receiver() -> &'static Mutex<Option<Receiver>> {
    static WARM: OnceLock<Mutex<Option<Receiver>>> = OnceLock::new();
    WARM.get_or_init(|| Mutex::new(None))
}

/// The name macOS shows in the AirPlay picker, and the display's name.
fn receiver_name() -> String {
    "ScreenExtend".to_string()
}

/// Brings the receiver up if needed. `Ok(true)` means it was started cold and discovery still
/// has to settle.
fn ensure_receiver() -> Result<bool, String> {
    let mut slot = warm_receiver().lock().unwrap();
    if slot.is_some() {
        return Ok(false);
    }
    let geometry = Geometry::new(
        sender_prefs::ADVERTISED_WIDTH,
        sender_prefs::ADVERTISED_HEIGHT,
        sender_prefs::FIXED_REFRESH_HZ,
    );
    let receiver = runtime()
        .block_on(Receiver::start(&receiver_name(), geometry))
        .map_err(|e| format!("could not start the AirPlay receiver: {e}"))?;
    *slot = Some(receiver);
    Ok(true)
}

/// Starts advertising in the background so the first client does not pay for it.
/// Failure only logs: `create_display` retries.
fn prewarm() {
    std::thread::Builder::new()
        .name("se-airplay-warm".into())
        .spawn(|| match ensure_receiver() {
            Ok(true) => tprintln!(
                "[airplay] receiver advertising ahead of the first client, so joining does not \
                 have to wait for discovery"
            ),
            Ok(false) => {}
            Err(e) => teprintln!("[airplay] could not pre-start the receiver: {e}"),
        })
        .ok();
}

fn registry() -> &'static Mutex<SessionRegistry> {
    static REGISTRY: OnceLock<Mutex<SessionRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(SessionRegistry::default()))
}

/// The geometry an AirPlay display actually settled on, in monitor coordinates.
///
/// `None` for a display this backend did not create. Other backends are *commanded* and do not
/// know which mode they got, hence the trait's default `None`; this tier reads the ladder macOS
/// published, chooses from it and waits for the result.
pub fn negotiated_geometry(id: u32) -> Option<(u32, u32)> {
    registry().lock().ok()?.sessions.get(&id).map(|s| s.settled)
}

/// True when a display id was created by this backend.
pub fn owns_display(id: u32) -> bool {
    registry()
        .lock()
        .map(|r| r.sessions.contains_key(&id))
        .unwrap_or(false)
}

/// Whether this backend can be used at all: it needs an Accessibility grant (we drive the
/// picker) and a logged-in GUI session (there is no menu bar otherwise).
pub fn availability() -> Result<(), String> {
    if !ax::accessibility_trusted() {
        return Err(ax::ACCESSIBILITY_DENIED.to_string());
    }
    if !ax::gui_session_present() {
        return Err(ax::NO_GUI_SESSION.to_string());
    }
    Ok(())
}

pub struct AirPlayVirtualDisplay;

impl std::fmt::Debug for AirPlayVirtualDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = registry().lock().map(|r| r.sessions.len()).unwrap_or(0);
        f.debug_struct("AirPlayVirtualDisplay")
            .field("sessions", &count)
            .finish()
    }
}

impl AirPlayVirtualDisplay {
    pub fn new_shared() -> Option<SharedVirtualDisplay> {
        if let Err(e) = availability() {
            teprintln!("[airplay] virtual-display fallback unavailable: {e}");
        }
        registry().lock().unwrap().sessions.clear();
        // advertise now, not at the first join: discovery is most of what a join costs
        prewarm();
        Some(Arc::new(Self))
    }
}

impl VirtualDisplayController for AirPlayVirtualDisplay {
    fn create_display(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Result<u32, String> {
        availability()?;

        if !registry().lock().unwrap().sessions.is_empty() {
            return Err(
                "macOS can drive only one AirPlay display at a time, and ScreenExtend already has \
                 one open. On this macOS version a second device cannot get its own display."
                    .to_string(),
            );
        }

        let (width, height, refresh_rate, note) = sender_prefs::clamp(width, height, refresh_rate);
        if let Some(note) = note {
            tprintln!("[airplay] {note}");
        }

        // always advertise 1080-tall and pick the client's size from the ladder afterwards:
        // macOS keeps one bit of the advertised geometry (whether the height reached 1080),
        // and anything shorter loses the top of the mode ladder
        let started_cold = ensure_receiver()?;
        if started_cold {
            std::thread::sleep(DISCOVERY_SETTLE);
        }
        let receiver_name = receiver_name();
        // the advertised device is shared across sessions, so the client's label is unused
        let _ = &name;

        let baseline = topology::baseline();
        // mirroring changes the built-in display's mode and un-mirroring does not put it back
        let modes_before = topology::snapshot_modes();

        let started = Instant::now();
        // one budget for the whole join, picker press included
        let budget = started + SESSION_BUDGET;

        // clear the latched phase and mirror flags so `diagnose` describes this attempt
        if let Ok(slot) = warm_receiver().lock() {
            if let Some(receiver) = slot.as_ref() {
                receiver.begin_session();
            }
        }

        ax::connect_to(&receiver_name, budget)?;

        let attached = match topology::wait_for_airplay_display(
            &baseline,
            budget.saturating_duration_since(Instant::now()),
        ) {
            Ok(a) => a,
            Err(e) => {
                let diagnosis = warm_receiver()
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(diagnose))
                    .unwrap_or_default();
                let _ = ax::disconnect(Instant::now() + TEARDOWN_BUDGET);
                return Err(format!("{e} {diagnosis}"));
            }
        };

        // macOS often attaches an AirPlay display mirrored, and a mirror slave is not in the
        // active display list. The menu route is the fallback for when CoreGraphics reports
        // success and the display stays in the mirror set anyway.
        if topology::is_mirroring(attached.id) {
            if let Err(e) = topology::stop_mirroring(attached.id) {
                teprintln!("[airplay] CoreGraphics un-mirror failed ({e}); trying the menu");
                if let Err(e2) = ax::use_as_separate_display(budget) {
                    let _ = ax::disconnect(Instant::now() + TEARDOWN_BUDGET);
                    return Err(format!(
                        "the display attached as a mirror and could not be extended: {e}; \
                         the menu fallback also failed: {e2}"
                    ));
                }
            }
        }

        // One reconfiguration: every other display goes back the way it was (otherwise the
        // Mac's own screen stays at the client's resolution for the session) and ours moves
        // onto the mode the client asked for.
        let granted = topology::geometry(attached.id);
        let target = topology::closest_mode(attached.id, (width, height));
        modes_before.restore_and_set(attached.id, target);
        let (w, h) = match target {
            Some(t) => topology::settle_geometry(attached.id, t, topology::MODE_SETTLE),
            None => granted,
        };
        if (w, h) != (width, height) {
            tprintln!(
                "[airplay] macOS granted {w}x{h} for a requested {width}x{height}: AirPlay \
                 geometry is negotiated, not commanded"
            );
        }
        tprintln!(
            "[airplay] display {} attached as an extended desktop ({w}x{h}) in {:.1}s{}",
            attached.id,
            started.elapsed().as_secs_f64(),
            if started_cold {
                " (cold start: the receiver had to be advertised first)"
            } else {
                ""
            }
        );

        registry().lock().unwrap().sessions.insert(
            attached.id,
            Session {
                name: receiver_name,
                requested: Geometry::new(width, height, refresh_rate),
                granted,
                settled: (w, h),
                modes_before,
            },
        );

        Ok(attached.id)
    }

    /// The device name is the display id in decimal, but un-mirroring reconfigures the
    /// topology and macOS may renumber displays. Re-resolve by identity (vendor/model
    /// `aapl`/`airp`) and fall back to the remembered id.
    fn display_device_name(&self, id: u32) -> Option<String> {
        if let Some(current) = topology::find_airplay_display() {
            if current != id {
                tprintln!(
                    "[airplay] display renumbered {id} -> {current} during reconfiguration; \
                     capturing {current}"
                );
                if let Ok(mut reg) = registry().lock() {
                    if let Some(session) = reg.sessions.remove(&id) {
                        reg.sessions.insert(current, session);
                    }
                }
            }
            return Some(current.to_string());
        }
        crate::macos_utils::streamer::display::display_by_name(&id.to_string())
            .map(|id| id.to_string())
    }

    /// The mode `settle_geometry` picked from the ladder, so `start_session` skips the forced
    /// `set_display_mode` and its settle poll, which cannot succeed for a size off the ladder.
    fn created_geometry(&self, id: u32) -> Option<(u32, u32)> {
        negotiated_geometry(id)
    }

    /// macOS drives exactly one AirPlay display, so a second client cannot be
    /// given one while the first is connected.
    fn max_concurrent_displays(&self) -> Option<usize> {
        Some(1)
    }

    fn remove_display(&self, id: u32) {
        let session = registry().lock().unwrap().sessions.remove(&id);
        let Some(session) = session else {
            teprintln!("[airplay] remove_display({id}): not an AirPlay display");
            return;
        };
        let started = Instant::now();
        let budget = started + TEARDOWN_BUDGET;

        // Undo our changes, then hand the configuration back: any
        // `CGCompleteDisplayConfiguration` we performed associates the display with this
        // process, and macOS can then hold it open past the end of its session.
        if topology::geometry(id) != session.granted {
            let (w, h) = session.granted;
            if let Err(e) = topology::set_mode_permanently(id, w, h) {
                teprintln!("[airplay] could not restore display {id} to {w}x{h}: {e}");
            }
        }
        topology::release_display_configuration();

        // Order matters: end the session while our device is still advertised. Once the Bonjour
        // records are withdrawn there is no picker row left to press and macOS holds the
        // display open.
        if let Err(e) = ax::disconnect(budget) {
            teprintln!("[airplay] could not stop the AirPlay session from the menu: {e}");
        }

        // the receiver stays up and advertising, which keeps the next join fast
        let modes_before = session.modes_before;

        // removing the display can disturb the other displays the way attaching it did
        modes_before.restore_except(id);

        let first_window = REMOVAL_TIMEOUT.min(budget.saturating_duration_since(Instant::now()));
        if !topology::wait_for_removal(id, first_window) {
            // macOS is not always prompt about dropping the display. A lingering one is inert:
            // the next `create_display` adopts the same id.
            topology::release_display_configuration();
            if !topology::wait_for_removal(id, REMOVAL_SETTLE_TIMEOUT) {
                teprintln!(
                    "[airplay] display {id}'s session has ended but macOS has not dropped the \
                     display yet; it will go when ScreenExtend exits, and it does not block a \
                     new session"
                );
            }
        }
        tprintln!(
            "[airplay] display {id} torn down in {:.1}s",
            started.elapsed().as_secs_f64()
        );
    }

    fn remove_all_displays(&self) {
        let ids: Vec<u32> = registry()
            .lock()
            .map(|r| r.sessions.keys().copied().collect())
            .unwrap_or_default();
        for id in ids {
            self.remove_display(id);
        }
        // app shutdown: stop advertising too
        match warm_receiver().lock() {
            Ok(mut slot) => {
                slot.take();
            }
            // still drop the receiver, or the Bonjour records are never withdrawn
            Err(e) => {
                teprintln!(
                    "[airplay] the receiver slot is poisoned; withdrawing the advertisement anyway"
                );
                e.into_inner().take();
            }
        }
        ax::restore_menu_bar_option();
    }
}

/// Turns "no display appeared" into something a user or a bug report can act on.
fn diagnose(receiver: &Receiver) -> String {
    match receiver.phase() {
        Phase::Advertised => {
            "The AirPlay sender never contacted the receiver at all: the picker press did not \
             start a session."
                .to_string()
        }
        Phase::Probed => format!(
            "The sender fetched our device description ({} requests from {}) but never set up a \
             stream. The handshake was rejected: most likely it demanded legacy pairing.",
            receiver.requests(),
            receiver.peer().unwrap_or_else(|| "the sender".to_string()),
        ),
        Phase::Recording => {
            if receiver.mirror_connected() {
                "The session is live and macOS is streaming to us, but no new display appeared. \
                 macOS may have attached the session as a mirror of the built-in display."
                    .to_string()
            } else {
                "The session reached RECORD but the sender never opened the mirroring connection."
                    .to_string()
            }
        }
        Phase::Ended => "The sender ended the session before a display appeared.".to_string(),
    }
}

/// Did macOS change the display geometry since the last call?
///
/// The sender re-sends unencrypted SPS/PPS whenever it does, which `MirrorStats` latches. The
/// session watchdog polls this and bumps the reconfig epoch.
pub fn take_geometry_changed() -> bool {
    warm_receiver()
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|r| r.take_geometry_changed()))
        .unwrap_or(false)
}

/// Reports what the picker currently lists, for `ScreenExtend doctor`.
pub fn probe() -> Result<Vec<String>, String> {
    let snap = ax::snapshot(Instant::now() + SESSION_BUDGET)?;
    Ok(snap
        .device_names()
        .into_iter()
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_starts_unset_and_latches() {
        let c = Cancel::new();
        assert!(!c.is_cancelled());
        c.cancel();
        assert!(c.is_cancelled());
        // clones share the flag
        assert!(c.clone().is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_returns_immediately_once_set() {
        let c = Cancel::new();
        c.cancel();
        tokio::time::timeout(Duration::from_millis(500), c.cancelled())
            .await
            .expect("must not block once cancelled");
    }

    #[test]
    fn the_registry_starts_empty() {
        assert!(!owns_display(12345));
    }

    #[test]
    fn a_second_display_is_refused_before_anything_is_touched() {
        // Does not call create_display: that would advertise a real Bonjour service and drive
        // the user's menu bar. The registry check it guards runs first.
        let occupied = !registry().lock().unwrap().sessions.is_empty();
        assert!(
            !occupied,
            "a test run must not leave an AirPlay session in the registry"
        );
    }

    #[test]
    fn a_display_we_did_not_create_has_no_negotiated_geometry() {
        // `None` tells the server "this backend cannot say" and it forces the mode instead;
        // any other answer would skip that for a display we do not own.
        registry().lock().unwrap().sessions.clear();
        assert_eq!(negotiated_geometry(0xDEAD_BEEF), None);
    }

    #[test]
    fn removing_an_unknown_display_is_a_no_op() {
        AirPlayVirtualDisplay.remove_display(999_999);
        AirPlayVirtualDisplay.remove_all_displays();
    }

    /// End-to-end harness against the live machine. Ignored by default: it advertises a real
    /// Bonjour service, drives the user's menu bar and reconfigures displays.
    ///
    /// ```sh
    /// cargo test --lib macos_utils::airplay::tests::live_end_to_end -- --ignored --nocapture
    /// ```
    ///
    /// It reports rather than asserts: the useful output is which step failed and what the
    /// picker contained.
    #[test]
    #[ignore = "drives real system UI; run manually"]
    fn live_end_to_end() {
        crate::logbus::set_verbose(true);
        println!("accessibility trusted: {}", ax::accessibility_trusted());
        println!(
            "exact geometry pref: {}",
            sender_prefs::exact_geometry_enabled()
        );
        // no `probe()` here: it opens and closes the menu `create_display` is about to drive

        // SE_AIRPLAY_TEST_SIZE=1440x900 exercises a different rung of the mode ladder
        let (want_w, want_h) = crate::envcfg::var("SE_AIRPLAY_TEST_SIZE")
            .ok()
            .and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
            })
            .unwrap_or((1920u32, 1080u32));
        println!("requesting {want_w}x{want_h}");

        let before_names = crate::macos_utils::streamer::pipeline::monitor_device_names();
        println!("before: {before_names:?}");

        // the real construction path, so the pre-warm runs as it does at app start
        let vd = AirPlayVirtualDisplay::new_shared().expect("controller");
        std::thread::sleep(Duration::from_secs(3));
        match vd.create_display("ScreenExtend Live Test".into(), want_w, want_h, 60) {
            Ok(id) => {
                println!("created display {id} at {:?}", topology::geometry(id));
                // the correlation the server performs: a mismatch here would be the host
                // capturing its own screen
                println!(
                    "  capture target = {:?} (create_display returned {id})",
                    vd.display_device_name(id)
                );
                println!("mirroring: {}", topology::is_mirroring(id));
                println!("modes offered: {:?}", topology::available_modes(id));
                println!("identity: {:?}", topology::identity(id));
                std::thread::sleep(Duration::from_secs(5));
                println!("after 5s: {:?}", topology::geometry(id));

                vd.remove_display(id);
                let gone = !crate::macos_utils::streamer::display::active_displays().contains(&id);
                println!("removed cleanly: {gone}");

                // can a second session start even if macOS is slow to drop the old display?
                match vd.create_display("ScreenExtend Live Test".into(), want_w, want_h, 60) {
                    Ok(id2) => {
                        println!(
                            "second session ok: id={id2} at {:?}",
                            topology::geometry(id2)
                        );
                        vd.remove_display(id2);
                        println!(
                            "second removed cleanly: {}",
                            !crate::macos_utils::streamer::display::active_displays()
                                .contains(&id2)
                        );
                    }
                    Err(e) => println!("second session FAILED: {e}"),
                }
            }
            Err(e) => println!("create_display failed: {e}"),
        }
        vd.remove_all_displays();
    }
}
