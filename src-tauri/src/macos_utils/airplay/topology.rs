//! Finding the display macOS created for us, and moving it out of the mirror set.
//!
//! Detection polls the display list rather than using
//! `CGDisplayRegisterReconfigurationCallback`: those callbacks need the main run loop, which
//! `ScreenExtend serve` never starts (registering on a worker thread succeeds and never fires).
//! Un-mirroring is the public CoreGraphics sequence behind "Use As Separate Display"
//! (`-[MPDisplayMgr stopMirroringForDisplay:]`), done directly instead of driving the menu.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCompleteDisplayConfiguration, CGConfigureDisplayMirrorOfDisplay,
    CGConfigureDisplayWithDisplayMode, CGConfigureOption, CGDisplayConfigRef,
    CGDisplayCopyAllDisplayModes, CGDisplayIsBuiltin, CGDisplayIsInMirrorSet,
    CGDisplayMirrorsDisplay, CGDisplayMode, CGDisplayModelNumber, CGDisplayPixelsHigh,
    CGDisplayPixelsWide, CGDisplayVendorNumber, CGError, CGGetOnlineDisplayList,
    CGRestorePermanentDisplayConfiguration,
};

use objc2_core_graphics::CGDisplayCopyDisplayMode;

use crate::macos_utils::streamer::display::active_displays;

/// `kCGNullDirectDisplay`.
const NULL_DISPLAY: u32 = 0;

/// Poll often: attach and reconfiguration both land in well under a second, and this interval
/// is paid on every join.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Snapshot of the active display set, taken before we ask macOS to connect.
pub struct Baseline(HashSet<u32>);

pub fn baseline() -> Baseline {
    Baseline(active_displays().into_iter().collect())
}

/// The vendor and model CoreGraphics reports for a display macOS created for an AirPlay
/// session: MediaToolbox hands WindowServer a fixed pair, `b"aapl"` / `b"airp"`
/// (`(1633775724, 1634300528)` on the live display).
const AIRPLAY_VENDOR: u32 = u32::from_be_bytes(*b"aapl");
const AIRPLAY_MODEL: u32 = u32::from_be_bytes(*b"airp");

/// Every display CoreGraphics knows about. The online list, not the active one: a display
/// attached as a mirror slave is excluded from `CGGetActiveDisplayList`, and we have to see it
/// to un-mirror it.
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
/// To mirror, macOS changes the built-in display's mode to match the AirPlay one, and
/// un-mirroring does not put it back, so we restore it ourselves.
///
/// Stored as `io_display_mode_id`: `CGDisplayMode` is neither `Send` nor `Sync`, and the id is
/// what tells "Default for display" from a scaled mode of the same logical size.
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
    /// Puts every remembered display except `skip` back the way it was. Returns how many were
    /// changed back.
    pub fn restore_except(&self, skip: u32) -> usize {
        self.restore_and_set(skip, None)
    }

    /// Restores every other display and, in the same commit, moves `target` onto `mode`. One
    /// commit because each display configuration blanks every attached screen for about a
    /// second.
    pub fn restore_and_set(&self, target: u32, mode: Option<(u32, u32)>) -> usize {
        let stale: Vec<(u32, i32)> = self
            .0
            .iter()
            .copied()
            .filter(|(id, want)| *id != target && current_mode_id(*id) != Some(*want))
            .collect();
        if stale.is_empty() && mode.is_none() {
            return 0;
        }

        let mut restored = 0;
        unsafe {
            let mut config = std::ptr::null_mut();
            if CGBeginDisplayConfiguration(&mut config) != CGError::Success {
                teprintln!("[airplay] could not begin a display configuration to restore modes");
                return 0;
            }
            if let Some((w, h)) = mode {
                if let Err(e) = queue_size(config, target, w, h) {
                    teprintln!("[airplay] could not queue {w}x{h} for display {target}: {e}");
                }
            }
            for (id, want) in &stale {
                match queue_mode_id(config, *id, *want) {
                    Ok(()) => restored += 1,
                    Err(e) => teprintln!("[airplay] display {id}: {e}"),
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

/// Queues a display's move to a specific published mode id.
///
/// # Safety
/// `config` must be a live configuration from `CGBeginDisplayConfiguration`.
unsafe fn queue_mode_id(config: CGDisplayConfigRef, id: u32, want: i32) -> Result<(), String> {
    unsafe {
        let modes = CGDisplayCopyAllDisplayModes(id, None)
            .ok_or_else(|| "CGDisplayCopyAllDisplayModes returned nil".to_string())?;
        for i in 0..modes.count() {
            let ptr = modes.value_at_index(i) as *const CGDisplayMode;
            if ptr.is_null() {
                continue;
            }
            let mode = &*ptr;
            if CGDisplayMode::io_display_mode_id(Some(mode)) != want {
                continue;
            }
            let err = CGConfigureDisplayWithDisplayMode(config, id, Some(mode), None);
            return if err == CGError::Success {
                Ok(())
            } else {
                Err(format!("could not queue a mode restore: CGError {}", err.0))
            };
        }
        Err("no longer offers the mode it had before the session".to_string())
    }
}

/// Queues a display's move to the published mode of a given size.
///
/// # Safety
/// `config` must be a live configuration from `CGBeginDisplayConfiguration`.
unsafe fn queue_size(
    config: CGDisplayConfigRef,
    id: u32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    unsafe {
        let modes = CGDisplayCopyAllDisplayModes(id, None)
            .ok_or_else(|| format!("CGDisplayCopyAllDisplayModes({id}) returned nil"))?;
        for i in 0..modes.count() {
            let ptr = modes.value_at_index(i) as *const CGDisplayMode;
            if ptr.is_null() {
                continue;
            }
            let mode = &*ptr;
            if CGDisplayMode::width(Some(mode)) as u32 != width
                || CGDisplayMode::height(Some(mode)) as u32 != height
            {
                continue;
            }
            let err = CGConfigureDisplayWithDisplayMode(config, id, Some(mode), None);
            return if err == CGError::Success {
                Ok(())
            } else {
                Err(format!(
                    "CGConfigureDisplayWithDisplayMode -> CGError {}",
                    err.0
                ))
            };
        }
        Err(format!("display {id} has no {width}x{height} mode"))
    }
}

/// The AirPlay display currently attached, if any. Found by identity, not a remembered id:
/// reconfiguring the topology can make macOS renumber displays.
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
/// Identified by vendor/model, not only by diffing the active list: macOS reuses the same
/// `CGDirectDisplayID` across sessions, so a new session can reattach an id that never went
/// away. The baseline only lets us prefer a freshly appeared one when there is a choice.
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
                    "macOS did not attach a display within the timeout: the AirPlay session was \
                     never accepted, or it was accepted and no display was created"
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
            // cancel rather than leave a half-built configuration behind
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

    // the reconfiguration is asynchronous: let it land before the caller captures
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

/// Every mode macOS published for a display, as `(width, height, refresh)`. CoreDisplay
/// synthesises a fixed ladder for an AirPlay display (no EDID), so this is the complete set of
/// reachable geometries.
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

/// The published mode closest to what was asked for, or `None` when the display is already
/// there.
///
/// macOS picks an AirPlay display's initial size from one bit (advertised height >= 1080), so
/// it starts at 1280x720 or 1920x1080, then publishes a ladder of modes that are selectable
/// like any other display's. Selection only: `ModeSnapshot::restore_and_set` queues the move.
pub fn closest_mode(id: u32, want: (u32, u32)) -> Option<(u32, u32)> {
    let current = geometry(id);
    if current == want {
        return None;
    }

    let modes = available_modes(id);
    if modes.is_empty() {
        return None;
    }

    // an exact match scores 0; otherwise aspect error counts double against area error
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

    let (w, h, _) = chosen?;
    if (w, h) == current {
        return None;
    }
    if (w, h) != want {
        tprintln!(
            "[airplay] {want:?} is not one of the modes macOS published for display {id}; \
             using the closest, {w}x{h}"
        );
    }
    Some((w, h))
}

/// Waits for a queued mode change to land and reports the geometry in effect. A display
/// configuration commits asynchronously.
pub fn settle_geometry(id: u32, want: (u32, u32), timeout: Duration) -> (u32, u32) {
    let deadline = Instant::now() + timeout;
    loop {
        let now = geometry(id);
        if now == want {
            return now;
        }
        if Instant::now() >= deadline {
            teprintln!(
                "[airplay] display {id} did not settle to {}x{} (still {}x{})",
                want.0,
                want.1,
                now.0,
                now.1
            );
            return now;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Switches a display to a published mode, as a permanent configuration.
///
/// Not `CGDisplaySetDisplayMode`: that scopes the change to the calling application, and macOS
/// then keeps the display alive until the application exits (measured: the AirPlay display
/// survived 48 s past its session and went only when the process died).
pub(crate) fn set_mode_permanently(id: u32, width: u32, height: u32) -> Result<(), String> {
    if geometry(id) == (width, height) {
        return Ok(());
    }
    unsafe {
        let mut config = std::ptr::null_mut();
        let err = CGBeginDisplayConfiguration(&mut config);
        if err != CGError::Success {
            return Err(format!("CGBeginDisplayConfiguration -> CGError {}", err.0));
        }
        if let Err(e) = queue_size(config, id, width, height) {
            // cancel rather than leave a half-built configuration behind
            let _ = CGCompleteDisplayConfiguration(config, CGConfigureOption::ForAppOnly);
            return Err(e);
        }
        let err = CGCompleteDisplayConfiguration(config, CGConfigureOption::Permanently);
        if err != CGError::Success {
            return Err(format!(
                "CGCompleteDisplayConfiguration -> CGError {}",
                err.0
            ));
        }
    }

    let now = settle_geometry(id, (width, height), MODE_SETTLE);
    if now == (width, height) {
        Ok(())
    } else {
        Err(format!(
            "display {id} did not settle to {width}x{height} (still {}x{})",
            now.0, now.1
        ))
    }
}

/// How long a committed display configuration gets to take effect.
pub const MODE_SETTLE: Duration = Duration::from_secs(2);

/// Hands every display configuration this process made back to the system.
///
/// Any `CGCompleteDisplayConfiguration` we perform associates the display with this process,
/// and macOS then keeps an AirPlay display alive until we exit (measured: 48 s and counting,
/// versus immediate removal untouched). This drops the association.
pub fn release_display_configuration() {
    unsafe { CGRestorePermanentDisplayConfiguration() };
}

/// `(vendor, model)` as CoreGraphics reports them, for identifying the display.
pub fn identity(id: u32) -> (u32, u32) {
    unsafe { (CGDisplayVendorNumber(id), CGDisplayModelNumber(id)) }
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

    /// CoreGraphics only reports displays to a process in a console session. Over ssh every
    /// list comes back empty.
    fn has_a_console_session() -> bool {
        !active_displays().is_empty()
    }

    #[test]
    fn baseline_of_the_live_system_is_not_empty() {
        if !has_a_console_session() {
            crate::test_support::skip_because(
                "airplay display baseline",
                "CoreGraphics reports no displays outside a console session (ssh?)",
            );
            return;
        }
        assert!(!baseline().0.is_empty());
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
    fn whatever_is_found_is_an_airplay_display_and_not_the_builtin() {
        // the search agrees with the predicate, and the built-in is never the answer: it is
        // what the host would otherwise start capturing and resizing
        let found = find_airplay_display();
        match found {
            Some(id) => {
                assert!(
                    !unsafe { CGDisplayIsBuiltin(id) },
                    "the built-in display was returned as an AirPlay receiver"
                );
                assert!(is_airplay_display(id));
            }
            None => {
                // nothing found, so nothing on this machine may satisfy the predicate either
                let matching: Vec<_> = active_displays()
                    .into_iter()
                    .filter(|id| is_airplay_display(*id))
                    .collect();
                assert!(
                    matching.is_empty(),
                    "find_airplay_display() returned None while {matching:?} match is_airplay_display()"
                );
            }
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
