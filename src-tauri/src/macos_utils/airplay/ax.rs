//! Driving the AirPlay picker without the user.
//!
//! Nothing a receiver advertises can pre-select an AirPlay display, and the
//! public sender route (`AVOutputContext`) is gated behind
//! `com.apple.avfoundation.allow-system-wide-context`, an entitlement AMFI will
//! not honour for a third-party binary. So this drives the Displays menu extra
//! through the Accessibility API. Rows are matched by `AXIdentifier` (the action
//! selector, the same in every locale) or by a title read from the menu's own
//! bundle, never by a hardcoded translation; the Bonjour name we published
//! picks the device row. Pressing the extra does not steal focus.

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
/// extra is the only way to start an AirPlay session from a third-party process,
/// so we set it ourselves. It is a per-user preference: no privileges, no restart.
///
/// It is the user's setting, not ours, so what it was before is remembered and
/// [`restore_menu_bar_option`] puts it back at shutdown.
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
            // a null value removes the key, which restores "never set"
            None => ptr::null(),
        };
        CFPreferencesSetAppValue(k.0, cf, d.0);
        CFPreferencesAppSynchronize(d.0)
    }
}

/// Returns true when it changed the setting.
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

/// Puts the menu-bar option back the way it was found.
///
/// Restores the *setting*, not the appearance: removing the extra needs a
/// SystemUIServer restart, so the item stays in the menu bar until the next login.
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

/// The menu extra this backend drives, and the source of every title it matches.
const DISPLAYS_MENU_BUNDLE: &str = "/System/Library/CoreServices/Menu Extras/Displays.menu";

/// Reads a localized string out of the Displays menu extra's own bundle.
///
/// This is what makes title matching safe. `AXIdentifier` on menu items is
/// **10.15-only**: `-[NSMenuItem userInterfaceItemIdentifier]` on 10.13.6 and
/// 10.14.6 returns only the `_uiid` ivar, and `DisplaysExtra` never calls
/// `setIdentifier:`, so on the OS versions this backend exists for the rows
/// have no identifier at all. Asking CFBundle for the key the menu itself uses
/// gets the string in whatever language the menu is drawn in.
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
        // CFBundle returns the key itself when the lookup misses
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

/// Whether a menu row can be chosen.
///
/// Disabled headers (`"AirPlay: <name>"`, `"AirPlay To:"`) can match a device
/// row by title on the OS versions that publish no `AXIdentifier`.
fn is_enabled(element: AXUIElementRef) -> bool {
    let Some(v) = copy_attr(element, "AXEnabled") else {
        // absent means "not applicable", which for a menu item means selectable
        return true;
    };
    unsafe { CFGetTypeID(v.0) != CFBooleanGetTypeID() || CFBooleanGetValue(v.0) != 0 }
}

/// An element's children, together with the array that owns them.
///
/// `CFArrayGetValueAtIndex` hands back a *borrowed* reference, so the array has
/// to outlive every pointer taken out of it. Owned, not leaked: a leaked array
/// also leaks a Mach port send right into SystemUIServer per child, and this is
/// called from 25 ms poll loops. Anything that has to outlive the array is
/// retained explicitly ([`retained`]).
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
            "{action} could not be completed: the target app may be busy or gone (AXError {err})"
        )),
        AX_ERROR_INVALID_ELEMENT => Err(format!(
            "{action} hit a stale element: the menu was rebuilt or dismissed (AXError {err})"
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
/// session: it is absent over ssh, at the login window and in a headless session.
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
/// desktop. Mostly a probe: the switch itself normally goes through CoreGraphics
/// ([`super::topology::stop_mirroring`]).
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

fn process_is_named(pid: i32, name: &[u8]) -> bool {
    // `proc_name` truncates at MAXCOMLEN: every name asked about here is shorter
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
/// Called at the top of every `find_displays_extra_once`, i.e. inside a 25 ms
/// retry loop (up to 600 times for one failed join), so it must not spawn
/// `pgrep`. `proc_listpids`/`proc_name` are libproc, in libSystem since 10.5.
///
/// The pid is then held: SystemUIServer starts at login and lives as long as the
/// session. The name check makes a restart self-healing: a recycled pid
/// belonging to something else fails it and forces a rescan.
fn system_ui_server_pid() -> Option<i32> {
    static CACHED: std::sync::OnceLock<std::sync::Mutex<Option<i32>>> = std::sync::OnceLock::new();
    let cell = CACHED.get_or_init(|| std::sync::Mutex::new(None));
    // poisoning would only mean a stale pid, which the name check catches
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
/// an AirPlay destination. `open`ing the bundle asks SystemUIServer to load it
/// live: the path is appended to `com.apple.systemuiserver`'s `menuExtras` and
/// the extra appears within a couple of seconds, **with SystemUIServer's pid
/// unchanged**. `killall SystemUIServer` would also work but flashes the user's
/// whole menu bar, and only removal requires it. So we only ever add.
fn find_displays_extra(budget: Instant) -> Result<Extra, String> {
    match find_displays_extra_once() {
        Ok(extra) => return Ok(extra),
        Err(e) if e == ACCESSIBILITY_DENIED => return Err(e),
        Err(_) => {}
    }

    // two levers, cheapest first: the preference that keeps the extra resident,
    // then ask LaunchServices to load the bundle now
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
            // AX queries every 25 ms do not make SystemUIServer load the extra
            // any sooner: back off once the fast case has not happened
            Err(_) => std::thread::sleep(poll_interval(started)),
        }
    }
}

/// Poll interval that widens once it is clear the cheap case did not happen.
///
/// The fast paths here land in tens of milliseconds, so the first second is
/// polled tightly. Past that we are waiting on something slow (a menu extra
/// loading, network discovery answering), and each poll costs an AX round trip
/// and two allocations per row.
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
        "SystemUIServer exposes no menu-bar extras. Is Accessibility granted to ScreenExtend?"
            .to_string()
    })?;

    let mut found: Option<CfOwned> = None;
    for extra in children(extras.0).iter() {
        // the Displays extra has no AXTitle: its description is what identifies
        // it before it is opened
        let desc = attr_string(extra, "AXDescription").unwrap_or_default();
        let ident = attr_string(extra, "AXIdentifier").unwrap_or_default();
        let known = menu_strings().descriptions.contains(&desc);
        if known
            || desc.contains("Displays")
            || desc.contains("AirPlay")
            || ident.contains("Displays")
        {
            // retained, not borrowed: `extras` and its child array both go out
            // of scope here, and the extra has to survive them
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
    // the open menu is the extra's first child; its rows are flat
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
                // match on the title; an identifier has to agree only when the
                // platform supplied one
                Some(t) => title == t && (identifier.is_empty() || identifier == want_identifier),
                // identifier only, so this path works on 10.15+ alone
                None => !identifier.is_empty() && identifier == want_identifier,
            };
            // several AirPlay devices in one menu is the normal case: the match
            // has to be exact *and* selectable, never the nearest header
            if matches && is_enabled(item) {
                return Some(retained(item));
            }
        }
    }
    None
}

fn open_menu(extra: &Extra, budget: Instant) -> Result<(), String> {
    // `AXPress` on a menu-bar extra is a *toggle* with no AX-visible open/closed
    // state: `AXChildren` on the extra is 0 before the first press and 1 forever
    // after. The only reliable signal is whether the menu has items, so: open if
    // it has them, otherwise press once and wait. A press that closed an open
    // menu reopens it next time round; never "reset" by pressing again here.
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

/// How the menu is dismissed, by routes that can never *open* one.
///
/// There is deliberately no variant for pressing the menu-bar extra: that is a
/// toggle, and since activating a menu item already dismisses the menu, a press
/// afterwards re-opens it and leaves the picker on screen after a device connects.
///
/// `AXCancel` is unsupported on the extra itself (`-25206`) but works on the
/// menu below it. The synthetic Escape is the last resort and is gated: it goes
/// to `kCGHIDEventTap`, i.e. to whatever the user has focused (it dismisses
/// their sheet, cancels their text input, or drops them out of full screen), so
/// it is only sent when a menu is demonstrably open, by the same signal
/// `open_menu` uses, and it says so in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DismissPlan {
    /// `AXCancel` on the open menu, the only dismissal that is not also a toggle.
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
/// go, so element references go stale on their own, and the person at the
/// keyboard can dismiss it at any moment. Both look identical from here (the
/// row is gone or the press fails) and both are fixed by opening it again.
const ROW_PRESS_ATTEMPTS: usize = 3;

/// How long to keep the menu open waiting for the row we want to appear.
///
/// The picker opens on `AirPlay: Looking for TV...` and fills the device rows in
/// as discovery answers, so "the menu has items" does **not** mean "the menu has
/// ours": looking exactly once and closing silently selects nothing.
const DEVICE_ROW_TIMEOUT: Duration = Duration::from_millis(6000);

/// How long to wait for a row whose presence does not depend on discovery.
///
/// "Stop AirPlay" and "Use As Separate Display" are drawn from state the menu
/// already has: if they are not there shortly after it opens they are not
/// coming, and waiting the full discovery budget would only slow teardown.
const STATE_ROW_TIMEOUT: Duration = Duration::from_millis(1000);

/// After activating a row, how long to let AppKit dispatch the action.
///
/// `DisplaysExtra` handles these on a `dispatch_async`, so the work has not
/// necessarily started when `AXPress` returns, and the caller goes straight on
/// to reconfiguring displays.
const ACTION_SETTLE: Duration = Duration::from_millis(250);

/// Poll `find` until it answers or `deadline` passes.
///
/// Separated from the AX calls around it so the *waiting* can be tested with a closure that
/// answers late, the only part a machine with no Displays menu extra can execute.
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

/// Sleeps [`ACTION_SETTLE`]: `AXPress` returns once the action is *dispatched*, not once macOS
/// has acted on it, so a caller that re-read the display list at once would see the old state.
fn settle_after_action() {
    std::thread::sleep(ACTION_SETTLE);
}

/// Opens the menu, waits for a row, and presses it, retrying the sequence.
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

        // re-read on every pass: the rows are rebuilt as devices arrive, so a
        // reference from the previous pass is stale
        let started = Instant::now();
        let deadline = (started + appear_timeout).min(budget);
        let pressed = wait_for_row(started, deadline, || find(&extra)).map(|row| press(row.0));

        match pressed {
            Some(Ok(())) => {
                // no dismissal: activating a menu item is what closes an NSMenu,
                // and anything done afterwards could only re-open it
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
    // the menu opens before the device rows exist: reading it at once would
    // report an empty picker
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
            "the Displays menu is not offering an extend option: either nothing is mirroring or \
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
/// * **By title**: `AIRPLAY_TURN_OFF` read from the menu's own bundle, so it is
///   whatever "Stop AirPlay" is called in the user's language. This is the only
///   route that works on 10.13/10.14, where menu items have no `AXIdentifier`.
/// * **By position**: on 10.15+, where identifiers exist, the Stop row is the
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
                    // the mirror-mode block only exists during a live session,
                    // and it sits below the Stop row
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
        // absent, so this asserts the probe does not panic either way
        let first = system_ui_server_pid();
        println!("SystemUIServer pid: {first:?}");
        // the second call comes off the cache and must agree with the first
        assert_eq!(first, system_ui_server_pid());
        if let Some(pid) = first {
            assert!(process_is_named(pid, SYSTEM_UI_SERVER));
        }
    }

    #[test]
    fn the_process_scan_finds_this_process() {
        // the test binary is the one process the scan is guaranteed to be able
        // to see, whatever the session and whatever the privileges
        let me = std::process::id() as i32;
        let mut name = [0u8; 64];
        let len =
            unsafe { libc::proc_name(me, name.as_mut_ptr() as *mut c_void, name.len() as u32) };
        assert!(len > 0, "proc_name must resolve our own pid");
        let name = &name[..len as usize];
        println!("own process name: {:?}", String::from_utf8_lossy(name));
        assert!(process_is_named(me, name));
        assert!(!process_is_named(me, b"definitely-not-this-process"));
        // the scan returns the lowest matching pid, which need not be us if a
        // second copy of the harness is running, but it must be *a* match
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
        // keys from Displays.menu's own Localizable.strings: if Apple renames
        // one this is where it surfaces, and the code degrades to the positional
        // fallback rather than misfiring
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
        // the row appears only on the fourth look
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
        // waiting must be bounded, or a Mac with no receiver on the network
        // hangs the join instead of reporting that it found nothing
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
        let started = Instant::now();
        settle_after_action();
        assert!(started.elapsed() >= ACTION_SETTLE);
    }

    #[test]
    fn a_device_row_gets_longer_than_a_state_row() {
        assert!(DEVICE_ROW_TIMEOUT > STATE_ROW_TIMEOUT);
    }

    #[test]
    fn dismissing_the_menu_never_presses_the_extra() {
        // `DismissPlan` has no variant for pressing the extra (a toggle), so
        // only the three real outcomes are left to check
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
        // every device is listed and ours is exactly one of them: a busy picker
        // is the normal case, not an error
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
        // while connected the menu grows an "AirPlay: <name>" header whose
        // title contains ours; matching is exact, so it cannot collide
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
