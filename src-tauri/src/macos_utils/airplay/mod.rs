//! Virtual displays on macOS 10.13–10.14, where `CGVirtualDisplay` does not exist.
//!
//! The private `CGVirtualDisplay*` classes that
//! [`super::virtual_display`] uses arrived with 10.15. Below that there is no
//! supported way for a userspace process to add a display — so this module
//! borrows one from AirPlay: it stands up a minimal fake AirPlay receiver on the
//! loopback-visible Bonjour namespace, drives the Mac's own AirPlay picker at
//! it, and lets **macOS** create the extended-desktop display. Once the display
//! exists it is an ordinary `CGDirectDisplayID` and the existing
//! CGDisplayStream + WebRTC pipeline captures it exactly as before.
//!
//! What this deliberately does not do: decode H.264, derive FairPlay keys,
//! decrypt anything, or touch VideoToolbox. The mirroring stream macOS sends us
//! is framed and dropped on the floor ([`mirror`]); the only reason we read it
//! at all is that a receiver which stops reading back-pressures the sender into
//! resetting the session.
//!
//! ## Shape
//!
//! | module | job |
//! |---|---|
//! | [`dnssd`] | Bonjour advertisement over a hand-written `dns_sd.h` FFI |
//! | [`rtsp`] | the mixed RTSP/1.0 + HTTP/1.1 control socket |
//! | [`info`] | the `GET /info` device description, where geometry is requested |
//! | [`fairplay`] | the static `/fp-setup` tables |
//! | [`receiver`] | the session state machine |
//! | [`mirror`] | accept-and-discard for the video stream |
//! | [`sender_prefs`] | the one macOS preference that makes the geometry stick |
//! | [`ax`] | pressing the picker on the user's behalf |
//! | [`topology`] | finding the new display and taking it out of the mirror set |

pub mod ax;
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

/// Minimal cancellation token. `tokio-util` is not a dependency and this is the
/// only piece of it we would use.
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

/// The receiver owns a runtime of its own rather than borrowing the streamer's.
///
/// `create_display` is called from a `spawn_blocking` on the streamer runtime,
/// but `remove_all_displays` is called from a bare `std::thread::spawn` during
/// shutdown, where there is no runtime context at all. One private runtime keeps
/// both paths identical.
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

/// Total budget for advertise → picker press → session → display attach.
///
/// Much longer than the 5 s `DISPLAY_ATTACH_TIMEOUT` the server uses for
/// `CGVirtualDisplay`, because an AirPlay session is a real network handshake
/// plus a WindowServer reconfiguration, not a synchronous API call.
const SESSION_BUDGET: Duration = Duration::from_secs(40);
/// How long to wait after a *cold* publish before the picker can plausibly list
/// us. Only paid when the receiver was not already warm — see [`prewarm`].
const DISCOVERY_SETTLE: Duration = Duration::from_millis(6000);
/// How long the display gets to disappear after we drop the session.
const REMOVAL_TIMEOUT: Duration = Duration::from_secs(8);

struct Session {
    name: String,
    requested: Geometry,
    /// The mode macOS chose on its own, before we moved the display onto the
    /// one the client asked for. Restored on teardown — see [`remove_display`].
    granted: (u32, u32),
    /// What every display was doing before the session, so the Mac's own screen
    /// can be put back after macOS mirrors and un-mirrors it.
    modes_before: topology::ModeSnapshot,
}

#[derive(Default)]
struct SessionRegistry {
    /// AirPlay allows exactly one display session at a time, so this holds at
    /// most one entry — but it is a map to mirror `virtual_display`'s registry
    /// and to keep `remove_display(id)` honest.
    sessions: HashMap<u32, Session>,
}

/// The receiver, kept alive across sessions.
///
/// Standing one up costs a Bonjour publish plus however long the sender takes to
/// rescan — several seconds, and by far the largest part of what a join used to
/// spend. Nothing about it is per-client: the advertised geometry is a constant
/// (see [`sender_prefs::ADVERTISED_WIDTH`]) because macOS keeps only one bit of
/// it, and the client's real size is selected from the mode ladder afterwards.
/// So it is started once, as early as possible, and simply left running.
fn warm_receiver() -> &'static Mutex<Option<Receiver>> {
    static WARM: OnceLock<Mutex<Option<Receiver>>> = OnceLock::new();
    WARM.get_or_init(|| Mutex::new(None))
}

/// The name macOS shows for us in the AirPlay picker, and the display's name.
fn receiver_name() -> String {
    "ScreenExtend".to_string()
}

/// Brings the receiver up if it is not already, and reports whether it had to be
/// started (in which case discovery still needs a moment to settle).
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
///
/// Called as soon as the backend is chosen. Failure is not fatal — `create_display`
/// retries — so this only logs.
fn prewarm() {
    std::thread::Builder::new()
        .name("se-airplay-warm".into())
        .spawn(|| match ensure_receiver() {
            Ok(true) => tprintln!(
                "[airplay] receiver advertising ahead of the first client, so joining does not                  have to wait for discovery"
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

/// True when a display id was created by this backend.
pub fn owns_display(id: u32) -> bool {
    registry()
        .lock()
        .map(|r| r.sessions.contains_key(&id))
        .unwrap_or(false)
}

/// Whether this backend can be used at all on the running system.
///
/// Two hard requirements: an Accessibility grant (we drive the picker), and a
/// logged-in GUI session (there is no menu bar otherwise).
pub fn availability() -> Result<(), String> {
    if !ax::accessibility_trusted() {
        return Err(ax::ACCESSIBILITY_DENIED.to_string());
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
        // Advertise now rather than when the first client arrives: discovery is
        // most of what a join costs, and it can happen while the user is still
        // looking at the QR code.
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

        // What we advertise is not what we ask for.
        //
        // macOS reduces the whole advertised geometry to one bit — whether the
        // height reached 1080 — and that bit decides which mode ladder the
        // display gets. Advertising the client's real size would, for anything
        // shorter than 1080, cost us the top of the ladder. So we always
        // advertise 1080-tall and then select the client's geometry out of the
        // ladder afterwards (`topology::best_effort_mode`).
        // The receiver is normally already advertising (see `prewarm`), which is
        // what keeps a join short. Only a cold start pays for discovery.
        let started_cold = ensure_receiver()?;
        if started_cold {
            std::thread::sleep(DISCOVERY_SETTLE);
        }
        let receiver_name = receiver_name();
        // `name` is the client's label; the advertised device is shared across
        // sessions, so it does not carry the client's name.
        let _ = &name;

        let baseline = topology::baseline();
        // Mirroring changes the built-in display's mode to match ours, and
        // un-mirroring does not put it back. Remember what every display was
        // doing so the Mac's own screen can be restored afterwards.
        let modes_before = topology::snapshot_modes();

        let started = Instant::now();

        ax::connect_to(&receiver_name)?;

        let attached = match topology::wait_for_airplay_display(
            &baseline,
            SESSION_BUDGET.saturating_sub(started.elapsed()),
        ) {
            Ok(a) => a,
            Err(e) => {
                let diagnosis = warm_receiver()
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(diagnose))
                    .unwrap_or_default();
                let _ = ax::disconnect();
                return Err(format!("{e} {diagnosis}"));
            }
        };

        // macOS often attaches an AirPlay display mirrored — and a mirror slave
        // is not in the active display list at all, so until this runs the rest
        // of ScreenExtend cannot see it. Convert it to an extended desktop; the
        // menu route is only a fallback for when CoreGraphics reports success
        // and the display stays in the mirror set anyway.
        if topology::is_mirroring(attached.id) {
            if let Err(e) = topology::stop_mirroring(attached.id) {
                teprintln!("[airplay] CoreGraphics un-mirror failed ({e}); trying the menu");
                if let Err(e2) = ax::use_as_separate_display() {
                    let _ = ax::disconnect();
                    return Err(format!(
                        "the display attached as a mirror and could not be extended: {e}; \
                         the menu fallback also failed: {e2}"
                    ));
                }
            }
        }

        // Put every *other* display back the way it was. Without this the Mac's
        // own screen is left at the client's resolution — showing as "Scaled"
        // in Displays preferences — for the rest of the session.
        modes_before.restore_except(attached.id);

        // macOS picks the initial size itself, from a single bit — whether the
        // advertised height reached 1080. The ladder it publishes afterwards is
        // where the client's actual geometry comes back.
        let granted = topology::geometry(attached.id);
        let (w, h) = topology::best_effort_mode(attached.id, (width, height));
        if (w, h) != (width, height) {
            tprintln!(
                "[airplay] macOS granted {w}x{h} for a requested {width}x{height} — AirPlay \
                 geometry is negotiated, not commanded"
            );
        }
        tprintln!(
            "[airplay] display {} attached as an extended desktop ({w}x{h}) in {:.1}s{}",
            attached.id,
            started.elapsed().as_secs_f64(),
            if started_cold {
                " (cold start — the receiver had to be advertised first)"
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
                modes_before,
            },
        );

        Ok(attached.id)
    }

    /// On macOS the monitor "device name" is the display id in decimal, so
    /// there is nothing to correlate — but the id we handed back can be stale
    /// by now: taking a display out of a mirror set reconfigures the topology,
    /// and macOS may renumber displays while it does. So re-resolve by identity
    /// (`CGDisplayVendorNumber`/`ModelNumber` = `aapl`/`airp`) and only fall
    /// back to the remembered id.
    fn display_device_name(&self, id: u32) -> Option<String> {
        if let Some(current) = topology::find_airplay_display() {
            if current != id {
                tprintln!(
                    "[airplay] display renumbered {id} -> {current} during reconfiguration;                      capturing {current}"
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

    /// macOS drives exactly one AirPlay display, so a second client cannot be
    /// given one while the first is connected.
    fn max_concurrent_displays(&self) -> Option<usize> {
        Some(1)
    }

    fn remove_display(&self, id: u32) {
        let session = registry().lock().unwrap().sessions.remove(&id);
        let Some(session) = session else {
            teprintln!("[airplay] remove_display({id}) — not an AirPlay display");
            return;
        };

        // Undo everything we changed about the display, then hand the whole
        // configuration back to the system. Any `CGCompleteDisplayConfiguration`
        // we performed — the un-mirror, or a mode change — associates the
        // display with this process, and macOS can then hold it open past the
        // end of its session.
        if topology::geometry(id) != session.granted {
            let (w, h) = session.granted;
            if let Err(e) = topology::set_mode_permanently(id, w, h) {
                teprintln!("[airplay] could not restore display {id} to {w}x{h}: {e}");
            }
        }
        topology::release_display_configuration();

        // Order matters. Ask macOS to end the session *first*, while our device
        // is still advertised and therefore still in the picker: once the
        // Bonjour records are withdrawn there is no row left to press, and
        // macOS holds the display open on a receiver it can no longer see.
        if let Err(e) = ax::disconnect() {
            teprintln!("[airplay] could not stop the AirPlay session from the menu: {e}");
        }

        // The receiver deliberately stays up and advertising: the AirPlay
        // *session* is what ends, and leaving the device published is what makes
        // the next join fast. It is torn down in `remove_all_displays`.
        let modes_before = session.modes_before;

        // Removing the display can disturb the other displays the same way
        // attaching it did, so put them back once more now that it is gone.
        modes_before.restore_except(id);

        if !topology::wait_for_removal(id, REMOVAL_TIMEOUT) {
            // macOS is not always prompt about dropping the display object once
            // its session is over. Hand the configuration back once more and give
            // it a second window; if it still lingers it is inert — the session
            // is gone, and the next `create_display` will adopt the same id
            // rather than waiting for a new one (see
            // `topology::wait_for_airplay_display`).
            topology::release_display_configuration();
            if !topology::wait_for_removal(id, REMOVAL_TIMEOUT) {
                teprintln!(
                    "[airplay] display {id}'s session has ended but macOS has not dropped the                      display yet; it will go when ScreenExtend exits, and it does not block a                      new session"
                );
            }
        }
    }

    fn remove_all_displays(&self) {
        let ids: Vec<u32> = registry()
            .lock()
            .map(|r| r.sessions.keys().copied().collect())
            .unwrap_or_default();
        for id in ids {
            self.remove_display(id);
        }
        // Now stop advertising too — this is app shutdown, not a client leaving.
        if let Ok(mut slot) = warm_receiver().lock() {
            slot.take();
        }
        sender_prefs::restore();
    }
}

/// Turns "no display appeared" into something a user or a bug report can act on.
fn diagnose(receiver: &Receiver) -> String {
    match receiver.phase() {
        Phase::Advertised => {
            "The AirPlay sender never contacted the receiver at all — the picker press did not \
             start a session."
                .to_string()
        }
        Phase::Probed => format!(
            "The sender fetched our device description ({} requests from {}) but never set up a \
             stream. The handshake was rejected — most likely it demanded legacy pairing.",
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

/// Reports what the picker currently lists, for `ScreenExtend doctor`.
pub fn probe() -> Result<Vec<String>, String> {
    let snap = ax::snapshot()?;
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
        // Clones observe the same state.
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
        // Deliberately does not call create_display: that would advertise a
        // real Bonjour service and drive the user's menu bar from a unit test.
        // The guard it exercises is the registry check, which runs first.
        let occupied = !registry().lock().unwrap().sessions.is_empty();
        assert!(
            !occupied,
            "a test run must not leave an AirPlay session in the registry"
        );
    }

    #[test]
    fn removing_an_unknown_display_is_a_no_op() {
        AirPlayVirtualDisplay.remove_display(999_999);
        AirPlayVirtualDisplay.remove_all_displays();
    }

    /// End-to-end harness against the live machine.
    ///
    /// Ignored by default because it advertises a real Bonjour service, drives
    /// the user's menu bar and reconfigures displays. Run it deliberately:
    ///
    /// ```sh
    /// cargo test --lib macos_utils::airplay::tests::live_end_to_end -- --ignored --nocapture
    /// ```
    ///
    /// It reports rather than asserts: on a Mac where the picker never lists us
    /// the interesting output is *which* step failed and what the picker did
    /// contain, not a red test.
    #[test]
    #[ignore = "drives real system UI; run manually"]
    fn live_end_to_end() {
        crate::logbus::set_verbose(true);
        println!("accessibility trusted: {}", ax::accessibility_trusted());
        println!(
            "exact geometry pref: {}",
            sender_prefs::exact_geometry_enabled()
        );
        // Deliberately no `probe()` here: it opens and closes the same menu
        // `create_display` is about to drive, which is not how the app behaves.

        // Override with SE_AIRPLAY_TEST_SIZE=1440x900 to exercise a different
        // rung of the mode ladder.
        let (want_w, want_h) = std::env::var("SE_AIRPLAY_TEST_SIZE")
            .ok()
            .and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
            })
            .unwrap_or((1920u32, 1080u32));
        println!("requesting {want_w}x{want_h}");

        let before_names = crate::macos_utils::streamer::pipeline::monitor_device_names();
        println!("before: {before_names:?}");

        // Go through the real construction path so the pre-warm runs, the way it
        // does when the app starts.
        let vd = AirPlayVirtualDisplay::new_shared().expect("controller");
        std::thread::sleep(Duration::from_secs(3));
        match vd.create_display("ScreenExtend Live Test".into(), want_w, want_h, 60) {
            Ok(id) => {
                println!("created display {id} at {:?}", topology::geometry(id));
                // The same correlation server.rs performs, so a mismatch shows
                // up here rather than as the host capturing its own screen.
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

                // The question that matters for the product: even if macOS is
                // slow to drop the old display, can we start a second session?
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
