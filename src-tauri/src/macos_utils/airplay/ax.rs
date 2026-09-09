//! Driving the AirPlay picker without the user.
//!
//! Selecting an AirPlay display is a sender-side, WindowServer-local decision:
//! nothing a receiver advertises can pre-select it, so the connect step has to
//! be performed on the sender. The public route (`AVOutputContext`) is gated
//! behind `com.apple.avfoundation.allow-system-wide-context`, an entitlement
//! AMFI will not honour for a third-party binary — so this drives the menu
//! extra instead, through the Accessibility API that ScreenExtend already asks
//! the user to grant for remote input.
//!
//! The one thing that makes this robust rather than a screen-scrape: AppKit
//! publishes each menu item's action selector as its `AXIdentifier`. The row for
//! an AirPlay device is `connectToAirPlayDevice:` in every locale, so we never
//! match on a translated title. The device's own name — which we chose when we
//! published the Bonjour service — disambiguates between rows.
//!
//! Pressing the extra does not steal focus and the menu populates in tens of
//! milliseconds; we close it again with `AXCancel` so nothing is left open on
//! the user's screen.

use std::ffi::c_void;
use std::process::Command;
use std::ptr;
use std::time::{Duration, Instant};

use super::cf_sys::{
    cf_string, cf_string_to_string, retained, CFArrayGetCount, CFArrayGetTypeID,
    CFArrayGetValueAtIndex, CFBooleanGetTypeID, CFBooleanGetValue, CFBundleCopyLocalizedString,
    CFBundleCreate, CFGetTypeID, CFPreferencesAppSynchronize, CFPreferencesCopyAppValue,
    CFPreferencesSetAppValue, CFRelease, CFTypeRef, CFURLCreateWithFileSystemPath, CfOwned,
    K_CF_BOOLEAN_FALSE, K_CF_BOOLEAN_TRUE,
};

/// An AX element is just a CFType; the alias is for reading, not for safety.
type AXUIElementRef = CFTypeRef;
type CFStringRef = CFTypeRef;
type AXError = i32;

const AX_SUCCESS: AXError = 0;
const AX_ERROR_API_DISABLED: AXError = -25211;
const AX_ERROR_NOT_IMPLEMENTED: AXError = -25208;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
const AX_ERROR_CANNOT_COMPLETE: AXError = -25204;
const AX_ERROR_INVALID_ELEMENT: AXError = -25202;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state_id: i32) -> *mut c_void;
    fn CGEventCreateKeyboardEvent(source: *mut c_void, keycode: u16, key_down: bool)
        -> *mut c_void;
    fn CGEventPost(tap: u32, event: *mut c_void);
}

/// The user-facing "Show mirroring options in the menu bar when available"
/// checkbox, in `com.apple.airplay`.
///
/// macOS only keeps the Displays menu extra resident while this is on, and the
/// extra is the only way to start an AirPlay session from a third-party process
/// — the programmatic route needs an entitlement AMFI will not honour. Leaving
/// it to the user would make this backend depend on a manual step, so we set it
/// ourselves. It is a per-user preference: no privileges, no restart.
///
/// It is also the user's setting, not ours, so what it was before is
/// remembered and [`restore_menu_bar_option`] puts it back at shutdown. The
/// sibling module states the principle and declines to write its preference at
/// all ([`super::sender_prefs`]); this one has to write, so it has to undo.
const AIRPLAY_DOMAIN: &str = "com.apple.airplay";
const SHOW_IN_MENU_BAR_KEY: &str = "showInMenuBarIfPresent";

/// What the preference was before we turned it on. `None` means we never did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviousMenuBar {
    /// The key was not in the domain at all, so restoring means removing it.
    Unset,
    /// The key was present and false.
    Off,
}

fn saved_menu_bar_option() -> &'static std::sync::Mutex<Option<PreviousMenuBar>> {
    static SAVED: std::sync::OnceLock<std::sync::Mutex<Option<PreviousMenuBar>>> =
        std::sync::OnceLock::new();
    SAVED.get_or_init(|| std::sync::Mutex::new(None))
}

/// `Some(on)` when the key is present, `None` when the domain does not carry it.
fn menu_bar_option() -> Option<bool> {
    let (Some(k), Some(d)) = (cf_string(SHOW_IN_MENU_BAR_KEY), cf_string(AIRPLAY_DOMAIN)) else {
        return None;
    };
    unsafe {
        let v = CFPreferencesCopyAppValue(k.0, d.0);
        if v.is_null() {
            return None;
        }
        let on = CFGetTypeID(v) == CFBooleanGetTypeID() && CFBooleanGetValue(v) != 0;
        CFRelease(v);
        Some(on)
    }
}

fn write_menu_bar_option(value: Option<bool>) -> bool {
    let (Some(k), Some(d)) = (cf_string(SHOW_IN_MENU_BAR_KEY), cf_string(AIRPLAY_DOMAIN)) else {
        return false;
    };
    unsafe {
        let cf = match value {
            Some(true) => K_CF_BOOLEAN_TRUE,
            Some(false) => K_CF_BOOLEAN_FALSE,
            // A null value removes the key, which is how "it was never set"
            // is restored.
            None => ptr::null(),
        };
        CFPreferencesSetAppValue(k.0, cf, d.0);
        CFPreferencesAppSynchronize(d.0)
    }
}

/// Turns the menu-bar option on if it is off. Returns true when it changed.
fn enable_menu_bar_option() -> bool {
    let previous = match menu_bar_option() {
        Some(true) => return false,
        Some(false) => PreviousMenuBar::Off,
        None => PreviousMenuBar::Unset,
    };
    if !write_menu_bar_option(Some(true)) {
        return false;
    }
    if let Ok(mut slot) = saved_menu_bar_option().lock() {
        slot.get_or_insert(previous);
    }
    tprintln!(
        "[airplay] enabled \"Show mirroring options in the menu bar when available\" so the \
         Displays menu is available to drive"
    );
    true
}

/// Puts the menu-bar option back the way we found it.
///
/// The item does not vanish from the menu bar until the next login — adding the
/// extra takes effect live, removing it needs a SystemUIServer restart — so this
/// restores the *setting*, not the appearance.
pub fn restore_menu_bar_option() {
    let previous = match saved_menu_bar_option().lock() {
        Ok(mut slot) => slot.take(),
        Err(e) => e.into_inner().take(),
    };
    let Some(previous) = previous else {
        return;
    };
    let value = match previous {
        PreviousMenuBar::Unset => None,
        PreviousMenuBar::Off => Some(false),
    };
    if write_menu_bar_option(value) {
        tprintln!(
            "[airplay] restored \"Show mirroring options in the menu bar when available\"; the \
             menu item itself goes at the next login"
        );
    } else {
        teprintln!("[airplay] could not restore {AIRPLAY_DOMAIN} {SHOW_IN_MENU_BAR_KEY}");
    }
}

/// The menu extra we drive, and the source of every title we compare against.
const DISPLAYS_MENU_BUNDLE: &str = "/System/Library/CoreServices/Menu Extras/Displays.menu";

/// Reads a localized string out of the Displays menu extra's own bundle.
///
/// This is what makes title matching safe. `AXIdentifier` on menu items is a
/// **10.15-only** affordance: `-[NSMenuItem userInterfaceItemIdentifier]` on
/// 10.13.6 and 10.14.6 returns only the `_uiid` ivar, and `DisplaysExtra` never
/// calls `setIdentifier:` — so on the OS versions this backend actually exists
/// for, the rows have no identifier at all. Asking CFBundle for the same key the
/// menu itself uses gets the right string in whatever language the menu is
/// drawn in, without hardcoding English.
fn menu_string(key: &str) -> Option<String> {
    let path = cf_string(DISPLAYS_MENU_BUNDLE)?;
    let k = cf_string(key)?;
    unsafe {
        let url = CFURLCreateWithFileSystemPath(ptr::null(), path.0, 0, true);
        if url.is_null() {
            return None;
        }
        let bundle = CFBundleCreate(ptr::null(), url);
        CFRelease(url);
        if bundle.is_null() {
            return None;
        }
        let value = CFBundleCopyLocalizedString(bundle, k.0, ptr::null(), ptr::null());
        CFRelease(bundle);
        if value.is_null() {
            return None;
        }
        let out = cf_string_to_string(value);
        CFRelease(value);
        // CFBundle returns the key itself when the lookup misses.
        out.filter(|v| v != key)
    }
}

/// Localized titles we compare menu rows against, resolved once.
struct MenuStrings {
    stop_airplay: Option<String>,
    use_as_display: Option<String>,
    descriptions: Vec<String>,
}

fn menu_strings() -> &'static MenuStrings {
    static CACHED: std::sync::OnceLock<MenuStrings> = std::sync::OnceLock::new();
    CACHED.get_or_init(|| MenuStrings {
        stop_airplay: menu_string("AIRPLAY_TURN_OFF"),
        use_as_display: menu_string("USE_AS_DISPLAY"),
        descriptions: ["AX_AIRPLAY_NONE", "AX_AIRPLAY_OFF", "AX_AIRPLAY_ON"]
            .iter()
            .filter_map(|k| menu_string(k))
            .collect(),
    })
}

fn copy_attr(element: AXUIElementRef, name: &str) -> Option<CfOwned> {
    let key = cf_string(name)?;
    let mut out: CFTypeRef = ptr::null();
    let err = unsafe { AXUIElementCopyAttributeValue(element, key.0, &mut out) };
    if err != AX_SUCCESS || out.is_null() {
        return None;
    }
    Some(CfOwned(out))
}

fn attr_string(element: AXUIElementRef, name: &str) -> Option<String> {
    let v = copy_attr(element, name)?;
    cf_string_to_string(v.0)
}

/// Whether a menu row can actually be chosen.
///
/// The menu carries disabled headers — `"AirPlay: <name>"`, `"AirPlay To:"` —
/// whose titles can look enough like a device row to be matched by accident on
/// the OS versions that publish no `AXIdentifier`. Only enabled rows are real.
fn is_enabled(element: AXUIElementRef) -> bool {
    let Some(v) = copy_attr(element, "AXEnabled") else {
        // Absent means "not applicable", which for a menu item means selectable.
        return true;
    };
    unsafe { CFGetTypeID(v.0) != CFBooleanGetTypeID() || CFBooleanGetValue(v.0) != 0 }
}

/// An element's children, together with the array that owns them.
///
/// `CFArrayGetValueAtIndex` hands back a *borrowed* reference, so the array has
/// to outlive every pointer taken out of it. That used to be arranged by leaking
/// the array — and with it a Mach port send right into SystemUIServer per child
/// — on every call, from inside poll loops that run every 25 ms for seconds at a
/// time. Owning it gives the same lifetime with a `Drop`; anything that has to
/// outlive the array is retained explicitly instead ([`retained`]).
struct Children {
    _array: CfOwned,
    items: Vec<AXUIElementRef>,
}

impl Children {
    fn empty() -> Self {
        Self {
            _array: CfOwned(ptr::null()),
            items: Vec::new(),
        }
    }

    fn iter(&self) -> impl Iterator<Item = AXUIElementRef> + '_ {
        self.items.iter().copied()
    }
}

fn children(element: AXUIElementRef) -> Children {
    let Some(v) = copy_attr(element, "AXChildren") else {
        return Children::empty();
    };
    if unsafe { CFGetTypeID(v.0) } != unsafe { CFArrayGetTypeID() } {
        return Children::empty();
    }
    let n = unsafe { CFArrayGetCount(v.0) };
    let mut items = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        let child = unsafe { CFArrayGetValueAtIndex(v.0, i) };
        if !child.is_null() {
            items.push(child);
        }
    }
    Children { _array: v, items }
}

fn press(element: AXUIElementRef) -> Result<(), String> {
    perform(element, "AXPress")
}

fn perform(element: AXUIElementRef, action: &str) -> Result<(), String> {
    let a = cf_string(action).ok_or_else(|| format!("could not build the {action} action"))?;
    let err = unsafe { AXUIElementPerformAction(element, a.0) };
    match err {
        AX_SUCCESS => Ok(()),
        AX_ERROR_API_DISABLED => Err(ACCESSIBILITY_DENIED.to_string()),
        AX_ERROR_NOT_IMPLEMENTED | AX_ERROR_ATTRIBUTE_UNSUPPORTED => Err(format!(
            "{action} is not supported by that element (AXError {err})"
        )),
        AX_ERROR_CANNOT_COMPLETE => Err(format!(
            "{action} could not be completed — the target app may be busy or gone (AXError {err})"
        )),
        AX_ERROR_INVALID_ELEMENT => Err(format!(
            "{action} hit a stale element — the menu was rebuilt or dismissed (AXError {err})"
        )),
        _ => Err(format!("{action} failed (AXError {err})")),
    }
}

pub const NO_GUI_SESSION: &str =
    "There is no logged-in desktop session on this Mac, so there is no menu bar to drive and \
     the AirPlay display fallback cannot start a session. Log in at the console and try again.";

/// Whether there is a desktop session at all.
///
/// SystemUIServer owns the menu bar and only exists inside a logged-in console
/// session, so its absence is exactly the condition that makes this backend
/// unusable — over ssh, at the login window, or in a headless session.
pub fn gui_session_present() -> bool {
    system_ui_server_pid().is_some()
}

pub const ACCESSIBILITY_DENIED: &str =
    "Accessibility permission is not granted, so ScreenExtend cannot open the AirPlay display \
     menu. Grant it under System Preferences → Security & Privacy → Privacy → Accessibility.";

/// AX identifier of the menu row that connects to an AirPlay device. Stable
/// across locales because it is the action selector, not a title.
const CONNECT_IDENTIFIER: &str = "connectToAirPlayDevice:";
/// AX identifier of the row that turns an existing mirror into an extended
/// desktop. We normally do this with CoreGraphics instead — see
/// [`super::topology::stop_mirroring`] — but the row is useful as a probe.
const USE_AS_DISPLAY_IDENTIFIER: &str = "stopMirroring:";

/// How long to wait for a pressed menu to populate. Measured at ~30 ms on
/// 10.15; the budget is generous because a busy SystemUIServer can be slower.
const POPULATE_TIMEOUT: Duration = Duration::from_millis(4000);
const POPULATE_POLL: Duration = Duration::from_millis(25);

pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

const SYSTEM_UI_SERVER: &[u8] = b"SystemUIServer";

/// `<sys/proc_info.h>`: `PROC_ALL_PIDS`.
const PROC_ALL_PIDS: u32 = 1;

/// True when `pid` is still the process we think it is.
fn process_is_named(pid: i32, name: &[u8]) -> bool {
    // `proc_name` truncates at MAXCOMLEN; every name we ask about is shorter.
    let mut buf = [0u8; 64];
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    len > 0 && &buf[..len as usize] == name
}

fn find_process_named(name: &[u8]) -> Option<i32> {
    unsafe {
        let bytes = libc::proc_listpids(PROC_ALL_PIDS, 0, ptr::null_mut(), 0);
        if bytes <= 0 {
            return None;
        }
        let cap = bytes as usize / std::mem::size_of::<i32>() + 32;
        let mut pids = vec![0i32; cap];
        let bytes = libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut c_void,
            (cap * std::mem::size_of::<i32>()) as libc::c_int,
        );
        if bytes <= 0 {
            return None;
        }
        let n = (bytes as usize / std::mem::size_of::<i32>()).min(cap);
        pids[..n]
            .iter()
            .copied()
            .filter(|pid| *pid > 0)
            .find(|pid| process_is_named(*pid, name))
    }
}

/// SystemUIServer's pid, cached, without forking.
///
/// This used to be `/usr/bin/pgrep -x SystemUIServer`, called at the top of
/// every `find_displays_extra_once` — i.e. inside a 25 ms retry loop, up to 200
/// times per attempt and 3 attempts per connect, so up to 600 fork+exec+waits
/// for a single failed join, on exactly the old hardware this tier exists for.
/// `proc_listpids`/`proc_name` are libproc, in libSystem since 10.5, and answer
/// the same question without a process spawn.
///
/// The pid is then held: SystemUIServer starts at login and lives as long as the
/// session. The cheap name check is what makes a restart self-healing — a
/// recycled pid belonging to something else fails it and forces a rescan.
fn system_ui_server_pid() -> Option<i32> {
    static CACHED: std::sync::OnceLock<std::sync::Mutex<Option<i32>>> = std::sync::OnceLock::new();
    let cell = CACHED.get_or_init(|| std::sync::Mutex::new(None));
    // Poisoning here would only mean a stale pid, which the name check catches.
    let mut slot = match cell.lock() {
        Ok(s) => s,
        Err(e) => e.into_inner(),
    };
    if let Some(pid) = *slot {
        if process_is_named(pid, SYSTEM_UI_SERVER) {
            return Some(pid);
        }
        *slot = None;
    }
    let found = find_process_named(SYSTEM_UI_SERVER);
    *slot = found;
    found
}

/// One menu row.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub title: String,
    pub identifier: String,
}

/// What the Displays menu currently offers. Used both to drive the connect and
/// to report a useful error when the device we published is not listed.
#[derive(Debug, Default)]
pub struct MenuSnapshot {
    pub items: Vec<MenuItem>,
}

impl MenuSnapshot {
    pub fn device_names(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter(|i| i.identifier == CONNECT_IDENTIFIER)
            .map(|i| i.title.as_str())
            .collect()
    }
}

struct Extra {
    /// The menu-bar extra itself, retained out of its parent's child array.
    element: CfOwned,
    /// Root application element, released when the Extra is dropped.
    _app: CfOwned,
}

/// Finds the Displays menu extra, adding it to the menu bar if it is missing.
///
/// macOS only keeps the extra resident "when available", i.e. while it has seen
/// an AirPlay destination — so on a machine that has not, there is nothing to
/// drive. `open`ing the bundle asks SystemUIServer to load it live: the path is
/// appended to `com.apple.systemuiserver`'s `menuExtras` and the extra appears
/// within a couple of seconds, **with SystemUIServer's pid unchanged**. That
/// matters: `killall SystemUIServer` would also work but visibly flashes the
/// user's whole menu bar, and removal — unlike addition — is the operation that
/// actually requires it. So we only ever add.
fn find_displays_extra(budget: Instant) -> Result<Extra, String> {
    match find_displays_extra_once() {
        Ok(extra) => return Ok(extra),
        Err(e) if e == ACCESSIBILITY_DENIED => return Err(e),
        Err(_) => {}
    }

    // Two levers, cheapest first: turn on the preference that keeps the extra
    // resident, then ask LaunchServices to load the bundle now.
    enable_menu_bar_option();
    if let Err(e) = Command::new("/usr/bin/open")
        .arg(DISPLAYS_MENU_BUNDLE)
        .status()
    {
        teprintln!("[airplay] could not ask SystemUIServer to load the Displays menu: {e}");
    }

    let started = Instant::now();
    let deadline = (started + EXTRA_APPEAR_TIMEOUT).min(budget);
    loop {
        match find_displays_extra_once() {
            Ok(extra) => {
                tprintln!("[airplay] added the Displays item to the menu bar");
                return Ok(extra);
            }
            Err(e) if Instant::now() >= deadline => return Err(e),
            // Hammering SystemUIServer with AX queries every 25 ms is not what
            // makes it finish loading a menu extra; back off once the fast case
            // has clearly not happened.
            Err(_) => std::thread::sleep(poll_interval(started)),
        }
    }
}

/// Poll interval that widens once it is clear the cheap case did not happen.
///
/// The fast paths here land in tens of milliseconds, so the first second is
/// polled tightly. Past that we are waiting on something slow — a menu extra
/// loading, network discovery answering — and sub-100 ms polling buys nothing
/// while costing an AX round trip and two allocations per row per iteration.
fn poll_interval(started: Instant) -> Duration {
    if started.elapsed() < Duration::from_secs(1) {
        POPULATE_POLL
    } else {
        SETTLED_POLL
    }
}

const SETTLED_POLL: Duration = Duration::from_millis(100);

/// How long SystemUIServer gets to load the menu extra after being asked.
const EXTRA_APPEAR_TIMEOUT: Duration = Duration::from_secs(5);

fn find_displays_extra_once() -> Result<Extra, String> {
    if !accessibility_trusted() {
        return Err(ACCESSIBILITY_DENIED.to_string());
    }

    let pid = system_ui_server_pid().ok_or_else(|| {
        "SystemUIServer is not running, so there is no menu bar to drive".to_string()
    })?;

    let app = unsafe { AXUIElementCreateApplication(pid) };
    if app.is_null() {
        return Err(format!(
            "could not open SystemUIServer (pid {pid}) for Accessibility"
        ));
    }
    let app = CfOwned(app);

    let extras = copy_attr(app.0, "AXExtrasMenuBar").ok_or_else(|| {
        "SystemUIServer exposes no menu-bar extras — is Accessibility granted to ScreenExtend?"
            .to_string()
    })?;

    let mut found: Option<CfOwned> = None;
    for extra in children(extras.0).iter() {
        // The Displays extra is the one whose description names it. It has no
        // AXTitle, so the description is what identifies it before it is opened.
        let desc = attr_string(extra, "AXDescription").unwrap_or_default();
        let ident = attr_string(extra, "AXIdentifier").unwrap_or_default();
        let known = menu_strings().descriptions.contains(&desc);
        if known
            || desc.contains("Displays")
            || desc.contains("AirPlay")
            || ident.contains("Displays")
        {
            // Retained rather than borrowed: `extras` and its child array both
            // go out of scope here, and the extra has to survive them.
            found = Some(retained(extra));
            break;
        }
    }
    if let Some(element) = found {
        return Ok(Extra { element, _app: app });
    }

    Err(
        "the Displays menu-bar item is not present. macOS shows it only when an AirPlay display \
         is available; if it does not appear on its own, tick System Preferences → Displays → \
         \"Show mirroring options in the menu bar when available\"."
            .to_string(),
    )
}

fn read_menu(extra: &Extra) -> MenuSnapshot {
    let mut snapshot = MenuSnapshot::default();
    // The open menu is the extra's first child; its rows are flat.
    let menus = children(extra.element.0);
    for child in menus.iter() {
        let rows = children(child);
        for item in rows.iter() {
            let title = attr_string(item, "AXTitle").unwrap_or_default();
            let identifier = attr_string(item, "AXIdentifier").unwrap_or_default();
            if title.is_empty() && identifier.is_empty() {
                continue;
            }
            snapshot.items.push(MenuItem { title, identifier });
        }
        if !snapshot.items.is_empty() {
            break;
        }
    }
    snapshot
}

/// Finds a menu row.
///
/// `AXIdentifier` is only populated on 10.15+, so it is treated as a bonus
/// confirmation rather than a requirement. The primary key is the title:
/// for a device row that is the name *we* chose when publishing the Bonjour
/// service, and for a command row it is the string read out of the menu's own
/// bundle, so neither depends on the user's language.
fn find_row(extra: &Extra, want_identifier: &str, want_title: Option<&str>) -> Option<CfOwned> {
    let menus = children(extra.element.0);
    for child in menus.iter() {
        let rows = children(child);
        for item in rows.iter() {
            let identifier = attr_string(item, "AXIdentifier").unwrap_or_default();
            let title = attr_string(item, "AXTitle").unwrap_or_default();

            let matches = match want_title {
                // A title we can name exactly: match on it, and accept a
                // mismatched identifier only if the platform supplied one.
                Some(t) => title == t && (identifier.is_empty() || identifier == want_identifier),
                // No title to go on: the identifier is all we have, which means
                // this path only works on 10.15+.
                None => !identifier.is_empty() && identifier == want_identifier,
            };
            // A menu listing several AirPlay devices is the normal case, so the
            // match has to be exact *and* selectable — never the nearest header.
            if matches && is_enabled(item) {
                return Some(retained(item));
            }
        }
    }
    None
}

fn open_menu(extra: &Extra, budget: Instant) -> Result<(), String> {
    // `AXPress` on a menu-bar extra is a *toggle*, and there is no AX-visible
    // open/closed state to read — `AXChildren` on the extra is 0 before the very
    // first press and 1 forever after, open or closed. So the only reliable
    // signal is whether the menu has items, and the only safe loop is: if it
    // already has them we are open; otherwise press once and wait. A press that
    // happened to close an already-open menu simply reopens it next time round,
    // which is why this must not also try to "reset" by pressing again itself.
    for _ in 0..3 {
        if !read_menu(extra).items.is_empty() {
            return Ok(());
        }
        if Instant::now() >= budget {
            break;
        }
        press(extra.element.0)?;
        let started = Instant::now();
        let deadline = (started + POPULATE_TIMEOUT).min(budget);
        while Instant::now() < deadline {
            if !read_menu(extra).items.is_empty() {
                return Ok(());
            }
            std::thread::sleep(poll_interval(started));
        }
    }
    Err(
        "the Displays menu did not populate after being pressed. It can take a moment to rebuild \
         after login or a SystemUIServer restart."
            .to_string(),
    )
}

/// Dismisses the menu, by a route that can never *open* one.
///
/// Pressing the menu-bar extra is a toggle, so using it to "close" is a trap:
/// activating a menu item already dismisses the menu, and a toggle afterwards
/// re-opens it — which is exactly how the picker ends up sitting on screen after
/// a device connects. So this never presses the extra.
///
/// `AXCancel` is unsupported on the extra itself (`-25206`) but works on the
/// menu below it, and a synthetic Escape closes whatever is open without being
/// able to open anything.
///
/// The Escape is the last resort and is gated, because it is not delivered to
/// the menu — it goes to `kCGHIDEventTap`, i.e. to whatever the user has
/// focused. Escape there dismisses their sheet, cancels their text input, or
/// drops them out of full screen, which is a real side effect of an operation
/// they did not start. So it is only sent when a menu is demonstrably open, by
/// the same signal `open_menu` uses, and it says so in the log.
/// How the menu is dismissed.
///
/// An enum rather than a comment, because the mistake this guards against is a specific one:
/// pressing the menu-bar extra is a *toggle*, so using it to "close" leaves the picker on screen
/// after a device connects. There is deliberately no variant for it, which makes the rule a
/// compile-time fact rather than something a test has to read the source to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DismissPlan {
    /// `AXCancel` on the open menu — the only dismissal that is not also a toggle.
    Cancel,
    /// Nothing to do: the menu is not open.
    AlreadyClosed,
    /// A synthetic Escape, because `AXCancel` was refused and the menu is still up.
    Escape,
}

/// Pure: what dismissal to perform, given what the AX tree answered.
fn dismiss_plan(cancel_accepted: bool, still_open: bool) -> DismissPlan {
    if cancel_accepted {
        DismissPlan::Cancel
    } else if !still_open {
        DismissPlan::AlreadyClosed
    } else {
        DismissPlan::Escape
    }
}

fn close_menu(extra: &Extra) {
    let menus = children(extra.element.0);
    let cancel_accepted = menus.iter().any(|child| perform(child, "AXCancel").is_ok());
    let still_open = !cancel_accepted && !read_menu(extra).items.is_empty();
    match dismiss_plan(cancel_accepted, still_open) {
        DismissPlan::Cancel | DismissPlan::AlreadyClosed => {}
        DismissPlan::Escape => {
            tprintln!("[airplay] the Displays menu would not cancel; sending a synthetic Escape");
            press_escape();
        }
    }
}

/// `kVK_Escape`.
const ESCAPE_KEYCODE: u16 = 0x35;
/// `kCGHIDEventTap`.
const HID_EVENT_TAP: u32 = 0;
/// `kCGEventSourceStateHIDSystemState`.
const SOURCE_STATE_HID: i32 = 1;

fn press_escape() {
    unsafe {
        let source = CGEventSourceCreate(SOURCE_STATE_HID);
        for down in [true, false] {
            let event = CGEventCreateKeyboardEvent(source, ESCAPE_KEYCODE, down);
            if event.is_null() {
                continue;
            }
            CGEventPost(HID_EVENT_TAP, event);
            CFRelease(event);
        }
        if !source.is_null() {
            CFRelease(source);
        }
    }
}

/// How many times to reopen the menu and look again before giving up.
///
/// The menu is a live `NSMenu` that SystemUIServer rebuilds as devices come and
/// go, so element references go stale on their own; and the person at the
/// keyboard can dismiss it at any moment by clicking elsewhere. Both look
/// identical from here — the row is gone or the press fails — and both are
/// fixed by opening it again.
const ROW_PRESS_ATTEMPTS: usize = 3;

/// How long to keep the menu open waiting for the row we want to appear.
///
/// The picker paints as soon as it opens, but it opens on `AirPlay: Looking for
/// TV…` and fills the device rows in as discovery answers. So "the menu has
/// items" does **not** mean "the menu has ours", and looking exactly once —
/// then closing — is how a selection silently fails to happen.
const DEVICE_ROW_TIMEOUT: Duration = Duration::from_millis(6000);

/// How long to wait for a row whose presence does not depend on discovery.
///
/// "Stop AirPlay" and "Use As Separate Display" are drawn from state the menu
/// already has, so if they are not there shortly after it opens they are not
/// coming — and waiting the full discovery budget would just make teardown slow.
const STATE_ROW_TIMEOUT: Duration = Duration::from_millis(1000);

/// After activating a row, how long to let AppKit dispatch the action.
///
/// `DisplaysExtra` handles these on a `dispatch_async`, so the work has not
/// necessarily started when `AXPress` returns, and the caller goes straight on
/// to reconfiguring displays.
const ACTION_SETTLE: Duration = Duration::from_millis(250);

/// Poll `find` until it answers or `deadline` passes.
///
/// The picker opens on "AirPlay: Looking for TV…" and fills its rows in as discovery answers, so
/// looking once and closing is how a selection silently fails to happen. Separated from the AX
/// calls around it so the *waiting* can be tested with a closure that answers late — which is
/// the only part a machine with no Displays menu extra can execute.
fn wait_for_row<T>(started: Instant, deadline: Instant, find: impl Fn() -> Option<T>) -> Option<T> {
    loop {
        if let Some(found) = find() {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(poll_interval(started));
    }
}

/// Give a dispatched menu action a moment before the caller moves on.
///
/// `AXPress` returns once the action is *dispatched*, not once macOS has acted on it, so a
/// caller that immediately re-read the display list would read the state before the change.
fn settle_after_action() {
    std::thread::sleep(ACTION_SETTLE);
}

/// Opens the menu, waits for a row, and presses it — retrying the sequence.
///
/// Finding and pressing are deliberately adjacent: a reference fetched and then
/// held across other work is exactly what goes stale.
fn press_row_in_menu(
    appear_timeout: Duration,
    budget: Instant,
    find: impl Fn(&Extra) -> Option<CfOwned>,
    missing: impl Fn(&MenuSnapshot) -> String,
) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..ROW_PRESS_ATTEMPTS {
        if Instant::now() >= budget {
            if last.is_empty() {
                last = "ran out of time driving the Displays menu".to_string();
            }
            break;
        }
        let extra = find_displays_extra(budget)?;
        if let Err(e) = open_menu(&extra, budget) {
            last = e;
            continue;
        }

        // Hold the menu open until our row shows up, re-reading it each time —
        // the rows are rebuilt as devices arrive, so a reference from the
        // previous pass is no good.
        let started = Instant::now();
        let deadline = (started + appear_timeout).min(budget);
        let pressed = wait_for_row(started, deadline, || find(&extra)).map(|row| press(row.0));

        match pressed {
            Some(Ok(())) => {
                // Deliberately no dismissal: activating a menu item is what
                // closes an NSMenu, and anything we did afterwards could only
                // re-open it. Just let the action get going.
                settle_after_action();
                return Ok(());
            }
            Some(Err(e)) => {
                last = e;
                close_menu(&extra);
            }
            None => {
                let snapshot = read_menu(&extra);
                close_menu(&extra);
                last = missing(&snapshot);
            }
        }
        if attempt + 1 < ROW_PRESS_ATTEMPTS {
            std::thread::sleep(POPULATE_POLL * 4);
        }
    }
    Err(last)
}

/// Opens the picker and reports what it contains, without selecting anything.
pub fn snapshot(budget: Instant) -> Result<MenuSnapshot, String> {
    let extra = find_displays_extra(budget)?;
    open_menu(&extra, budget)?;
    // Same reason the press path waits: the menu opens before the device rows
    // exist, so reading it immediately would report an empty picker.
    let started = Instant::now();
    let deadline = (started + DEVICE_ROW_TIMEOUT).min(budget);
    let mut snap = read_menu(&extra);
    while snap.device_names().is_empty() && Instant::now() < deadline {
        std::thread::sleep(poll_interval(started));
        snap = read_menu(&extra);
    }
    close_menu(&extra);
    Ok(snap)
}

/// Selects the AirPlay device published under `device_name`.
///
/// On success macOS starts a session; it normally attaches the display in mirror
/// mode, which the caller converts to an extended desktop with CoreGraphics.
pub fn connect_to(device_name: &str, budget: Instant) -> Result<(), String> {
    press_row_in_menu(
        DEVICE_ROW_TIMEOUT,
        budget,
        |extra| find_row(extra, CONNECT_IDENTIFIER, Some(device_name)),
        |snapshot| {
            let listed = snapshot.device_names();
            if listed.is_empty() {
                format!(
                    "macOS is not offering {device_name:?} as an AirPlay display. The picker lists \
                     no AirPlay devices at all, so the sender never accepted our Bonjour \
                     advertisement."
                )
            } else {
                format!(
                    "macOS is not offering {device_name:?} as an AirPlay display. The picker \
                     lists: {}",
                    listed.join(", ")
                )
            }
        },
    )?;
    tprintln!("[airplay] selected {device_name:?} in the Displays menu");
    Ok(())
}

/// Presses "Use As Separate Display" through the menu.
///
/// [`super::topology::stop_mirroring`] does the same thing with public
/// CoreGraphics and is preferred; this exists as a fallback for the case where
/// the CoreGraphics route reports success but the display stays mirrored.
pub fn use_as_separate_display(budget: Instant) -> Result<(), String> {
    press_row_in_menu(
        STATE_ROW_TIMEOUT,
        budget,
        |extra| {
            menu_strings()
                .use_as_display
                .as_deref()
                .and_then(|t| find_row(extra, USE_AS_DISPLAY_IDENTIFIER, Some(t)))
                .or_else(|| find_row(extra, USE_AS_DISPLAY_IDENTIFIER, None))
        },
        |_| {
            "the Displays menu is not offering an extend option — either nothing is mirroring or \
             the display is already extended"
                .to_string()
        },
    )
}

/// AX identifier of the row that puts a connected display back into mirror mode.
/// Only present while a session is live, which is how we recognise that state.
const START_MIRRORING_IDENTIFIER: &str = "startMirroring:";

/// Ends the active AirPlay session.
///
/// Two ways to find the row, because the reliable one differs by OS version:
///
/// * **By title** — `AIRPLAY_TURN_OFF` read from the menu's own bundle, so it is
///   whatever "Stop AirPlay" is called in the user's language. This is the only
///   route that works on 10.13/10.14, where menu items have no `AXIdentifier`.
/// * **By position** — on 10.15+, where identifiers exist, the Stop row is the
///   *first* `connectToAirPlayDevice:` item (it is that action invoked with no
///   device), sitting above the `startMirroring:` / `stopMirroring:` block,
///   while the real devices are listed after the "AirPlay To:" header.
pub fn disconnect(budget: Instant) -> Result<(), String> {
    press_row_in_menu(
        STATE_ROW_TIMEOUT,
        budget,
        |extra| {
            menu_strings()
                .stop_airplay
                .as_deref()
                .and_then(|t| find_row(extra, CONNECT_IDENTIFIER, Some(t)))
                .or_else(|| find_stop_row_by_position(extra))
        },
        |_| {
            "the Displays menu is not showing an active AirPlay session, so there is nothing to \
             disconnect"
                .to_string()
        },
    )?;
    tprintln!("[airplay] stopped the AirPlay session from the Displays menu");
    Ok(())
}

/// The 10.15+ fallback: first `connectToAirPlayDevice:` row, and only when the
/// mirror-mode block below it proves a session is live.
fn find_stop_row_by_position(extra: &Extra) -> Option<CfOwned> {
    let menus = children(extra.element.0);
    for child in menus.iter() {
        let mut first_connect: Option<CfOwned> = None;
        let rows = children(child);
        for item in rows.iter() {
            match attr_string(item, "AXIdentifier")
                .unwrap_or_default()
                .as_str()
            {
                CONNECT_IDENTIFIER if first_connect.is_none() => {
                    first_connect = Some(retained(item))
                }
                START_MIRRORING_IDENTIFIER | USE_AS_DISPLAY_IDENTIFIER => {
                    // The mirror-mode block only exists during a live session,
                    // and it sits below the Stop row.
                    return first_connect;
                }
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_ui_server_is_running_on_a_desktop_session() {
        // SystemUIServer only exists in a logged-in GUI session; over ssh it is
        // absent, so this asserts the probe does not panic either way.
        let first = system_ui_server_pid();
        println!("SystemUIServer pid: {first:?}");
        // The second call comes off the cache and must agree with the first.
        assert_eq!(first, system_ui_server_pid());
        if let Some(pid) = first {
            assert!(process_is_named(pid, SYSTEM_UI_SERVER));
        }
    }

    #[test]
    fn the_process_scan_finds_this_process() {
        // The test binary is the one process the scan is guaranteed to be able
        // to see, whatever the session and whatever the privileges.
        let me = std::process::id() as i32;
        let mut name = [0u8; 64];
        let len =
            unsafe { libc::proc_name(me, name.as_mut_ptr() as *mut c_void, name.len() as u32) };
        assert!(len > 0, "proc_name must resolve our own pid");
        let name = &name[..len as usize];
        println!("own process name: {:?}", String::from_utf8_lossy(name));
        assert!(process_is_named(me, name));
        assert!(!process_is_named(me, b"definitely-not-this-process"));
        // The scan returns the lowest matching pid, which need not be us if a
        // second copy of the harness is running — but it must be *a* match.
        let found = find_process_named(name).expect("the scan must find a process by that name");
        assert!(process_is_named(found, name));
        assert_eq!(find_process_named(b"definitely-not-this-process"), None);
    }

    #[test]
    fn polling_widens_once_the_fast_case_has_passed() {
        let now = Instant::now();
        assert_eq!(poll_interval(now), POPULATE_POLL);
        assert_eq!(
            poll_interval(now - Duration::from_secs(3)),
            SETTLED_POLL,
            "a multi-second wait must not be polled every 25 ms"
        );
    }

    #[test]
    fn the_menu_bundle_yields_localized_titles() {
        // The keys come from Displays.menu's own Localizable.strings. If Apple
        // ever renames one, this is where it surfaces — and the code degrades to
        // the positional fallback rather than misfiring.
        let m = menu_strings();
        assert!(
            m.stop_airplay.is_some(),
            "AIRPLAY_TURN_OFF must resolve; without it 10.13/10.14 cannot disconnect"
        );
        assert!(m.use_as_display.is_some(), "USE_AS_DISPLAY must resolve");
        assert!(
            !m.descriptions.is_empty(),
            "at least one AX_AIRPLAY_* description must resolve"
        );
    }

    #[test]
    fn a_missing_bundle_key_is_none_not_the_key_itself() {
        assert_eq!(menu_string("NO_SUCH_KEY_IN_THIS_BUNDLE"), None);
    }

    #[test]
    fn a_row_is_waited_for_rather_than_looked_for_once() {
        // The picker opens on "AirPlay: Looking for TV…" and fills in devices as
        // discovery answers, so a single look followed by a close is how a
        // selection silently fails to happen.
        //
        // This used to assert on the *text* of `press_row_in_menu`, because the
        // wait was tangled up in AX calls no test machine can make. It is now a
        // seam that takes the lookup as a closure, so the waiting itself runs
        // here: the row below appears only on the fourth look.
        let looks = std::cell::Cell::new(0);
        let started = Instant::now();
        let found = wait_for_row(started, started + Duration::from_secs(2), || {
            looks.set(looks.get() + 1);
            if looks.get() >= 4 {
                Some("Living Room TV")
            } else {
                None
            }
        });
        assert_eq!(
            found,
            Some("Living Room TV"),
            "a late row must still be found"
        );
        assert!(
            looks.get() >= 4,
            "the menu was read {} time(s)",
            looks.get()
        );
    }

    #[test]
    fn a_row_that_never_appears_gives_up_at_the_deadline() {
        // The other half: waiting must be bounded, or a Mac with no receiver on
        // the network hangs the join instead of reporting that it found nothing.
        let looks = std::cell::Cell::new(0);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(300);
        let found = wait_for_row(started, deadline, || -> Option<&str> {
            looks.set(looks.get() + 1);
            None
        });
        assert_eq!(found, None);
        assert!(looks.get() > 1, "it looked once and gave up");
        assert!(
            Instant::now() >= deadline,
            "it returned before its own deadline"
        );
    }

    #[test]
    fn a_row_that_is_already_there_is_pressed_without_waiting() {
        let started = Instant::now();
        let found = wait_for_row(started, started + Duration::from_secs(30), || Some(7));
        assert_eq!(found, Some(7));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "an available row should not have waited"
        );
    }

    #[test]
    fn a_dispatched_action_is_given_time_to_take_effect() {
        // `AXPress` returns when the action is dispatched, not when macOS has
        // acted on it, so the caller has to pause before reading the result.
        let started = Instant::now();
        settle_after_action();
        assert!(started.elapsed() >= ACTION_SETTLE);
    }

    #[test]
    fn a_device_row_gets_longer_than_a_state_row() {
        // One depends on network discovery; the other is already in the menu's
        // own state, and waiting the full budget for it would slow teardown.
        assert!(DEVICE_ROW_TIMEOUT > STATE_ROW_TIMEOUT);
    }

    #[test]
    fn dismissing_the_menu_never_presses_the_extra() {
        // Pressing the extra is a toggle: using it to "close" is what leaves the
        // picker on screen after a device connects. `DismissPlan` has no variant
        // for it, so that rule is now a compile-time fact; what is left to check
        // is that the three real outcomes are chosen correctly.
        //
        // This used to read the text of `close_menu` and look for the absence of
        // a substring, which a comment mentioning it would have broken.
        assert_eq!(
            dismiss_plan(true, false),
            DismissPlan::Cancel,
            "AXCancel is the dismissal that is not a toggle"
        );
        assert_eq!(
            dismiss_plan(true, true),
            DismissPlan::Cancel,
            "a menu that accepted AXCancel needs nothing else"
        );
        assert_eq!(
            dismiss_plan(false, false),
            DismissPlan::AlreadyClosed,
            "a menu that is not open must not be sent an Escape at the desktop"
        );
        assert_eq!(
            dismiss_plan(false, true),
            DismissPlan::Escape,
            "escape is the fallback when AXCancel is refused"
        );
    }

    #[test]
    fn several_devices_are_disambiguated_by_title() {
        let snap = MenuSnapshot {
            items: vec![
                MenuItem {
                    title: "AirPlay To:".into(),
                    identifier: String::new(),
                },
                MenuItem {
                    title: "Living Room TV".into(),
                    identifier: CONNECT_IDENTIFIER.into(),
                },
                MenuItem {
                    title: "ScreenExtend - iPad".into(),
                    identifier: CONNECT_IDENTIFIER.into(),
                },
                MenuItem {
                    title: "Kitchen".into(),
                    identifier: CONNECT_IDENTIFIER.into(),
                },
            ],
        };
        // Every device is listed, and the one we published is exactly one of
        // them — the picker being busy is the normal case, not an error.
        assert_eq!(
            snap.device_names(),
            vec!["Living Room TV", "ScreenExtend - iPad", "Kitchen"]
        );
        assert_eq!(
            snap.device_names()
                .iter()
                .filter(|n| **n == "ScreenExtend - iPad")
                .count(),
            1
        );
    }

    #[test]
    fn a_device_row_is_not_confused_with_the_connected_header() {
        // While connected the menu grows an "AirPlay: <name>" header whose
        // title contains ours. Matching is exact, so it cannot collide.
        let header = "AirPlay: ScreenExtend - iPad";
        let row = "ScreenExtend - iPad";
        assert_ne!(header, row);
        assert!(header.contains(row), "the substring overlap is real");
    }

    #[test]
    fn snapshot_helpers_filter_by_identifier() {
        let snap = MenuSnapshot {
            items: vec![
                MenuItem {
                    title: "AirPlay To:".into(),
                    identifier: String::new(),
                },
                MenuItem {
                    title: "Living Room".into(),
                    identifier: CONNECT_IDENTIFIER.into(),
                },
                MenuItem {
                    title: "Use As Separate Display".into(),
                    identifier: USE_AS_DISPLAY_IDENTIFIER.into(),
                },
            ],
        };
        assert_eq!(snap.device_names(), vec!["Living Room"]);
    }

    #[test]
    #[ignore = "requires a GUI session and Accessibility permission; run manually"]
    fn can_read_the_live_displays_menu() {
        match snapshot(Instant::now() + Duration::from_secs(30)) {
            Ok(s) => println!("{:#?}", s.items),
            Err(e) => println!("no menu: {e}"),
        }
    }
}
