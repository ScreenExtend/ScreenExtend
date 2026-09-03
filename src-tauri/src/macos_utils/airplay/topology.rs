//! Finding the display macOS created for us, and moving it out of the mirror set.
//!
//! Two jobs, both pure CoreGraphics and both public API:
//!
//! * **Detection** — diff `CGGetActiveDisplayList()` across the AirPlay connect
//!   and pick the display that appeared. Polling rather than
//!   `CGDisplayRegisterReconfigurationCallback`, because those callbacks are
//!   delivered on the process's main run loop and `ScreenExtend serve` parks its
//!   main thread on a channel receive without ever starting one. Registering on
//!   a worker thread succeeds and then never fires.
//! * **Extend** — macOS attaches an AirPlay display in *mirror* mode. The
//!   "Use As Separate Display" menu item in `Displays.menu` is a one-line
//!   trampoline into `-[MPDisplayMgr stopMirroringForDisplay:]`, which is
//!   `CGBeginDisplayConfiguration` + `CGConfigureDisplayMirrorOfDisplay(cfg, id,
//!   kCGNullDirectDisplay)` + `CGCompleteDisplayConfiguration(cfg,
//!   kCGConfigurePermanently)`. That is all public, so we do it directly instead
//!   of driving the menu.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCompleteDisplayConfiguration, CGConfigureDisplayMirrorOfDisplay,
    CGConfigureDisplayWithDisplayMode, CGConfigureOption, CGDisplayCopyAllDisplayModes,
    CGDisplayIsBuiltin, CGDisplayIsInMirrorSet, CGDisplayIsOnline, CGDisplayMirrorsDisplay,
    CGDisplayMode, CGDisplayModelNumber, CGDisplayPixelsHigh, CGDisplayPixelsWide,
    CGDisplayVendorNumber, CGError, CGGetOnlineDisplayList, CGRestorePermanentDisplayConfiguration,
};

use objc2_core_graphics::CGDisplayCopyDisplayMode;

use crate::macos_utils::streamer::display::active_displays;

/// `kCGNullDirectDisplay`.
const NULL_DISPLAY: u32 = 0;

/// Attach and reconfiguration both land in well under a second, so poll
/// often — this interval is paid on every join.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Snapshot of the active display set, taken before we ask macOS to connect.
pub struct Baseline(HashSet<u32>);

pub fn baseline() -> Baseline {
    Baseline(active_displays().into_iter().collect())
}

impl Baseline {
    fn newcomers(&self) -> Vec<u32> {
        active_displays()
            .into_iter()
            .filter(|id| !self.0.contains(id))
            .collect()
    }
}

/// The vendor and model CoreGraphics reports for a display macOS created for an
/// AirPlay session.
///
/// MediaToolbox hands WindowServer a fixed `kCGSVirtualDisplayVendorID` /
/// `kCGSVirtualDisplayModelID` pair when it brings an AirPlay display online, so
/// these two numbers identify one exactly. Confirmed on the live display:
/// `(1633775724, 1634300528)` = `b"aapl"`, `b"airp"`.
const AIRPLAY_VENDOR: u32 = u32::from_be_bytes(*b"aapl");
const AIRPLAY_MODEL: u32 = u32::from_be_bytes(*b"airp");

/// Every display CoreGraphics knows about, including ones that are not drawable.
///
/// Deliberately the *online* list, not the active one. When macOS attaches an
/// AirPlay session as a **mirror**, the AirPlay display is online but not
/// active — mirror slaves are excluded from `CGGetActiveDisplayList` — so a
/// search of the active list finds nothing and the session looks like it failed.
/// We need to see it in order to un-mirror it, after which it becomes active.
fn online_displays() -> Vec<u32> {
    const MAX_DISPLAYS: usize = 16;
    let mut ids = [0u32; MAX_DISPLAYS];
    let mut count: u32 = 0;
    let err = unsafe { CGGetOnlineDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        teprintln!("[airplay] CGGetOnlineDisplayList failed: CGError {}", err.0);
        return Vec::new();
    }
    ids[..count as usize].to_vec()
}

/// True when macOS created this display for an AirPlay session.
pub fn is_airplay_display(id: u32) -> bool {
    identity(id) == (AIRPLAY_VENDOR, AIRPLAY_MODEL)
}

/// The exact mode every display was in before we touched the topology.
///
/// Attaching an AirPlay display starts in **mirror** mode, and to mirror, macOS
/// changes the built-in display's mode to match the AirPlay one. Un-mirroring
/// does *not* put it back — the Mac's own screen is left at the client's
/// resolution, showing as "Scaled" in Displays preferences until someone picks
/// "Default for display" by hand. So we remember what every display was doing
/// and restore it ourselves.
///
/// The whole `CGDisplayMode` is kept, not just its size: at one logical size
/// there can be both a native and a scaled mode, and "Default for display" is a
/// specific one of them. `io_display_mode_id` is what tells them apart.
/// Which mode each display was in before we touched the topology.
///
/// Stored as `io_display_mode_id`, not the `CGDisplayMode` object: the object is
/// neither `Send` nor `Sync` so it cannot live in the session registry, and the
/// id is the stable handle anyway — it is what distinguishes "Default for
/// display" from a scaled mode of the same logical size, which is exactly the
/// distinction that has to survive.
#[derive(Debug, Default)]
pub struct ModeSnapshot(Vec<(u32, i32)>);

fn current_mode_id(id: u32) -> Option<i32> {
    let mode = unsafe { CGDisplayCopyDisplayMode(id) }?;
    Some(unsafe { CGDisplayMode::io_display_mode_id(Some(&mode)) })
}

pub fn snapshot_modes() -> ModeSnapshot {
    ModeSnapshot(
        online_displays()
            .into_iter()
            .filter_map(|id| current_mode_id(id).map(|m| (id, m)))
            .collect(),
    )
}

impl ModeSnapshot {
    /// Puts every remembered display back the way it was, skipping `skip` — the
    /// display we deliberately reconfigured.
    ///
    /// Returns how many were actually changed back.
    pub fn restore_except(&self, skip: u32) -> usize {
        let stale: Vec<(u32, i32)> = self
            .0
            .iter()
            .copied()
            .filter(|(id, want)| *id != skip && current_mode_id(*id) != Some(*want))
            .collect();
        if stale.is_empty() {
            return 0;
        }

        let mut restored = 0;
        unsafe {
            let mut config = std::ptr::null_mut();
            if CGBeginDisplayConfiguration(&mut config) != CGError::Success {
                teprintln!("[airplay] could not begin a display configuration to restore modes");
                return 0;
            }
            for (id, want) in &stale {
                let Some(modes) = CGDisplayCopyAllDisplayModes(*id, None) else {
                    continue;
                };
                let mut found = false;
                for i in 0..modes.count() {
                    let ptr = modes.value_at_index(i) as *const CGDisplayMode;
                    if ptr.is_null() {
                        continue;
                    }
                    let mode = &*ptr;
                    if CGDisplayMode::io_display_mode_id(Some(mode)) != *want {
                        continue;
                    }
                    let err = CGConfigureDisplayWithDisplayMode(config, *id, Some(mode), None);
                    if err == CGError::Success {
                        found = true;
                        restored += 1;
                    } else {
                        teprintln!(
                            "[airplay] could not queue a mode restore for display {id}: CGError {}",
                            err.0
                        );
                    }
                    break;
                }
                if !found {
                    teprintln!(
                        "[airplay] display {id} no longer offers the mode it had before the session"
                    );
                }
            }
            let err = CGCompleteDisplayConfiguration(config, CGConfigureOption::Permanently);
            if err != CGError::Success {
                teprintln!(
                    "[airplay] restoring display modes failed: CGError {}",
                    err.0
                );
                return 0;
            }
        }

        if restored > 0 {
            tprintln!(
                "[airplay] restored {restored} display(s) to the mode they had before the session"
            );
        }
        restored
    }
}

/// The AirPlay display currently attached, if any.
///
/// Identity rather than a remembered id: reconfiguring the display topology can
/// make macOS renumber displays, so an id captured a moment ago is not
/// guaranteed to still name the same screen.
pub fn find_airplay_display() -> Option<u32> {
    online_displays()
        .into_iter()
        .find(|&id| !unsafe { CGDisplayIsBuiltin(id) } && is_airplay_display(id))
}

/// A display macOS attached for our session, with the geometry it chose.
#[derive(Clone, Copy, Debug)]
pub struct Attached {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

/// Waits for macOS to attach an AirPlay display.
///
/// Identified by vendor/model rather than purely by diffing the active list,
/// because macOS reuses the same `CGDirectDisplayID` across sessions: if a
/// previous display has not finished going away, a genuinely new session
/// reattaches *that* id and a diff would see nothing appear. A display that was
/// already present is still ours to use — the baseline only lets us prefer a
/// freshly-appeared one when there is a choice.
pub fn wait_for_airplay_display(base: &Baseline, timeout: Duration) -> Result<Attached, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut fallback: Option<Attached> = None;
        for id in online_displays() {
            if unsafe { CGDisplayIsBuiltin(id) } || !is_airplay_display(id) {
                continue;
            }
            let (w, h) = geometry(id);
            if w == 0 || h == 0 {
                continue;
            }
            let found = Attached {
                id,
                width: w,
                height: h,
            };
            if !base.0.contains(&id) {
                return Ok(found);
            }
            fallback = Some(found);
        }
        if Instant::now() >= deadline {
            return match fallback {
                Some(a) => Ok(a),
                None => Err(
                    "macOS did not attach a display within the timeout — the AirPlay session was                      never accepted, or it was accepted and no display was created"
                        .to_string(),
                ),
            };
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Waits for a display to disappear from the active list.
pub fn wait_for_removal(id: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !active_displays().contains(&id) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// True when the display is mirroring another one, i.e. macOS attached it as a
/// mirror rather than as an extended desktop.
pub fn is_mirroring(id: u32) -> bool {
    unsafe { CGDisplayIsInMirrorSet(id) && CGDisplayMirrorsDisplay(id) != NULL_DISPLAY }
}

/// The programmatic equivalent of picking "Use As Separate Display".
pub fn stop_mirroring(id: u32) -> Result<(), String> {
    if !is_mirroring(id) {
        return Ok(());
    }

    unsafe {
        let mut config = std::ptr::null_mut();
        let err = CGBeginDisplayConfiguration(&mut config);
        if err != CGError::Success {
            return Err(format!("CGBeginDisplayConfiguration -> CGError {}", err.0));
        }

        let err = CGConfigureDisplayMirrorOfDisplay(config, id, NULL_DISPLAY);
        if err != CGError::Success {
            // Cancel rather than leaving a half-built configuration behind.
            let _ = CGCompleteDisplayConfiguration(config, CGConfigureOption::ForAppOnly);
            return Err(format!(
                "CGConfigureDisplayMirrorOfDisplay({id}, kCGNullDirectDisplay) -> CGError {}",
                err.0
            ));
        }

        let err = CGCompleteDisplayConfiguration(config, CGConfigureOption::Permanently);
        if err != CGError::Success {
            return Err(format!(
                "CGCompleteDisplayConfiguration(kCGConfigurePermanently) -> CGError {}",
                err.0
            ));
        }
    }

    // The reconfiguration is asynchronous; let it land before the caller starts
    // capturing.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !is_mirroring(id) {
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(format!(
        "display {id} was still mirroring 3s after CGConfigureDisplayMirrorOfDisplay succeeded"
    ))
}

/// Every mode macOS published for a display, as `(width, height, refresh)`.
///
/// An AirPlay display gets a fixed ladder synthesised by CoreDisplay rather
/// than anything derived from an EDID, so this is the complete set of
/// geometries reachable with `CGDisplaySetDisplayMode` once it exists.
pub fn available_modes(id: u32) -> Vec<(u32, u32, u32)> {
    let mut out = Vec::new();
    unsafe {
        let Some(modes) = CGDisplayCopyAllDisplayModes(id, None) else {
            return out;
        };
        for i in 0..modes.count() {
            let ptr = modes.value_at_index(i) as *const CGDisplayMode;
            if ptr.is_null() {
                continue;
            }
            let m = &*ptr;
            out.push((
                CGDisplayMode::width(Some(m)) as u32,
                CGDisplayMode::height(Some(m)) as u32,
                CGDisplayMode::refresh_rate(Some(m)).round() as u32,
            ));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Moves the display onto the published mode closest to what was asked for.
///
/// macOS decides an AirPlay display's *initial* size from a single bit — whether
/// the advertised height reached 1080 — so a request for anything else lands on
/// 1280x720 or 1920x1080. But it then publishes a real ladder of modes, and
/// those are selectable like any other display's. This is what turns "the size
/// macOS felt like" into "the size the client asked for", whenever the request
/// is on the ladder.
///
/// Returns the geometry actually in effect.
pub fn best_effort_mode(id: u32, want: (u32, u32)) -> (u32, u32) {
    let current = geometry(id);
    if current == want {
        return current;
    }

    let modes = available_modes(id);
    if modes.is_empty() {
        return current;
    }

    // Exact match first; otherwise the mode closest in area that does not
    // distort the aspect ratio more than the alternatives.
    let target_aspect = want.0 as f64 / want.1.max(1) as f64;
    let target_area = want.0 as f64 * want.1 as f64;
    let chosen = modes
        .iter()
        .filter(|(w, h, _)| *w > 1 && *h > 1)
        .min_by(|a, b| {
            let score = |(w, h, _): &(u32, u32, u32)| {
                let aspect = *w as f64 / (*h).max(1) as f64;
                let area = *w as f64 * *h as f64;
                let aspect_err = (aspect / target_aspect).ln().abs();
                let area_err = (area / target_area).ln().abs();
                aspect_err * 2.0 + area_err
            };
            score(a)
                .partial_cmp(&score(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied();

    let Some((w, h, _)) = chosen else {
        return current;
    };
    if (w, h) == current {
        return current;
    }

    match set_mode_permanently(id, w, h) {
        Ok(()) => {
            let now = geometry(id);
            if (w, h) == want {
                tprintln!("[airplay] display {id} moved to the requested {w}x{h}");
            } else {
                tprintln!(
                    "[airplay] {want:?} is not one of the modes macOS published for display {id};                      using the closest, {w}x{h}"
                );
            }
            now
        }
        Err(e) => {
            teprintln!("[airplay] could not move display {id} to {w}x{h}: {e}");
            current
        }
    }
}

/// Switches a display to a published mode, as a *permanent* configuration.
///
/// Deliberately not `CGDisplaySetDisplayMode`. That call scopes the change to
/// the calling application, and macOS then keeps the display alive until that
/// application exits — measured here: with `CGDisplaySetDisplayMode` the AirPlay
/// display survived 48 s past the end of its session and only vanished when the
/// process died, while without any mode change it disappeared immediately.
/// Going through a display configuration committed with `kCGConfigurePermanently`
/// gets the same geometry without the process association.
pub(crate) fn set_mode_permanently(id: u32, width: u32, height: u32) -> Result<(), String> {
    unsafe {
        let modes = CGDisplayCopyAllDisplayModes(id, None)
            .ok_or_else(|| format!("CGDisplayCopyAllDisplayModes({id}) returned nil"))?;
        let mut chosen: Option<&CGDisplayMode> = None;
        for i in 0..modes.count() {
            let ptr = modes.value_at_index(i) as *const CGDisplayMode;
            if ptr.is_null() {
                continue;
            }
            let m = &*ptr;
            if CGDisplayMode::width(Some(m)) as u32 == width
                && CGDisplayMode::height(Some(m)) as u32 == height
            {
                chosen = Some(m);
                break;
            }
        }
        let mode = chosen.ok_or_else(|| format!("display {id} has no {width}x{height} mode"))?;

        let mut config = std::ptr::null_mut();
        let err = CGBeginDisplayConfiguration(&mut config);
        if err != CGError::Success {
            return Err(format!("CGBeginDisplayConfiguration -> CGError {}", err.0));
        }
        let err = CGConfigureDisplayWithDisplayMode(config, id, Some(mode), None);
        if err != CGError::Success {
            let _ = CGCompleteDisplayConfiguration(config, CGConfigureOption::ForAppOnly);
            return Err(format!(
                "CGConfigureDisplayWithDisplayMode({id}, {width}x{height}) -> CGError {}",
                err.0
            ));
        }
        let err = CGCompleteDisplayConfiguration(config, CGConfigureOption::Permanently);
        if err != CGError::Success {
            return Err(format!(
                "CGCompleteDisplayConfiguration -> CGError {}",
                err.0
            ));
        }
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if geometry(id) == (width, height) {
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(format!(
        "display {id} did not settle to {width}x{height} (still {:?})",
        geometry(id)
    ))
}

/// Hands every display configuration this process made back to the system.
///
/// Any `CGCompleteDisplayConfiguration` we perform — the un-mirror, or a mode
/// change — associates the affected display with this process, and macOS then
/// keeps an AirPlay display alive until we exit rather than removing it when its
/// session ends. Measured: 48 s and counting, versus immediate removal when we
/// never touched the configuration. This is the documented way to drop that
/// association, and it is what makes teardown prompt.
pub fn release_display_configuration() {
    unsafe { CGRestorePermanentDisplayConfiguration() };
}

/// `(vendor, model)` as CoreGraphics reports them, for identifying the display.
pub fn identity(id: u32) -> (u32, u32) {
    unsafe { (CGDisplayVendorNumber(id), CGDisplayModelNumber(id)) }
}

/// Whether CoreGraphics still considers the display connected.
pub fn is_online(id: u32) -> bool {
    unsafe { CGDisplayIsOnline(id) }
}

/// Geometry the display settled on, once it is no longer mirroring.
pub fn geometry(id: u32) -> (u32, u32) {
    unsafe {
        (
            CGDisplayPixelsWide(id) as u32,
            CGDisplayPixelsHigh(id) as u32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_of_the_live_system_is_not_empty() {
        assert!(
            !active_displays().is_empty(),
            "a Mac always has at least one active display"
        );
    }

    #[test]
    fn no_airplay_display_appears_without_a_connect() {
        let base = baseline();
        let err = wait_for_airplay_display(&base, Duration::from_millis(300))
            .expect_err("no display should attach on its own");
        assert!(err.contains("did not attach"), "{err}");
    }

    #[test]
    fn the_airplay_fourccs_are_what_coregraphics_reports() {
        assert_eq!(AIRPLAY_VENDOR, 1_633_775_724);
        assert_eq!(AIRPLAY_MODEL, 1_634_300_528);
    }

    #[test]
    fn no_airplay_display_is_found_when_none_is_attached() {
        // The built-in must never be returned here: it is what the host would
        // otherwise start capturing and resizing.
        if let Some(id) = find_airplay_display() {
            assert!(!unsafe { CGDisplayIsBuiltin(id) });
            assert!(is_airplay_display(id));
        }
    }

    #[test]
    fn the_builtin_display_is_not_mistaken_for_an_airplay_one() {
        for id in active_displays() {
            if unsafe { CGDisplayIsBuiltin(id) } {
                assert!(!is_airplay_display(id));
            }
        }
    }

    #[test]
    fn the_main_display_is_not_reported_as_mirroring_itself() {
        for id in active_displays() {
            if unsafe { CGDisplayMirrorsDisplay(id) } == NULL_DISPLAY {
                assert!(!is_mirroring(id));
            }
        }
    }
}
