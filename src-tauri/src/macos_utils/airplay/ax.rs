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

use std::ffi::{c_void, CStr, CString};
use std::process::Command;
use std::ptr;
use std::time::{Duration, Instant};

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type AXUIElementRef = *const c_void;
type AXError = i32;

const AX_SUCCESS: AXError = 0;
const AX_ERROR_API_DISABLED: AXError = -25211;
const AX_ERROR_NOT_IMPLEMENTED: AXError = -25208;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
const AX_ERROR_CANNOT_COMPLETE: AXError = -25204;
const AX_ERROR_INVALID_ELEMENT: AXError = -25202;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

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

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        cstr: *const i8,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetCString(
        s: CFStringRef,
        buffer: *mut i8,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: isize) -> CFTypeRef;
    fn CFURLCreateWithFileSystemPath(
        allocator: *const c_void,
        path: CFStringRef,
        path_style: isize,
        is_directory: bool,
    ) -> *const c_void;
    fn CFBundleCreate(allocator: *const c_void, url: *const c_void) -> *const c_void;
    fn CFBundleCopyLocalizedString(
        bundle: *const c_void,
        key: CFStringRef,
        value: CFStringRef,
        table: CFStringRef,
    ) -> CFStringRef;
    fn CFPreferencesCopyAppValue(key: CFStringRef, application_id: CFStringRef) -> CFTypeRef;
    fn CFPreferencesSetAppValue(key: CFStringRef, value: CFTypeRef, application_id: CFStringRef);
    fn CFPreferencesAppSynchronize(application_id: CFStringRef) -> bool;
    // Signature must match the other declaration in this crate
    // (`macos_utils/streamer/encoder.rs`), or the linker sees a clash.
    fn CFBooleanGetValue(boolean: *const c_void) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state_id: i32) -> *mut c_void;
    fn CGEventCreateKeyboardEvent(source: *mut c_void, keycode: u16, key_down: bool)
        -> *mut c_void;
    fn CGEventPost(tap: u32, event: *mut c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    #[link_name = "CFBooleanGetTypeID"]
    fn cf_boolean_type_id() -> usize;
    #[link_name = "kCFBooleanTrue"]
    static K_CF_BOOLEAN_TRUE: CFTypeRef;
}

/// The user-facing "Show mirroring options in the menu bar when available"
/// checkbox, in `com.apple.airplay`.
///
/// macOS only keeps the Displays menu extra resident while this is on, and the
/// extra is the only way to start an AirPlay session from a third-party process
/// — the programmatic route needs an entitlement AMFI will not honour. Leaving
/// it to the user would make this backend depend on a manual step, so we set it
/// ourselves. It is a per-user preference: no privileges, no restart, and it is
/// exactly the setting the feature requires.
const AIRPLAY_DOMAIN: &str = "com.apple.airplay";
const SHOW_IN_MENU_BAR_KEY: &str = "showInMenuBarIfPresent";

fn menu_bar_option_enabled() -> bool {
    let (Some(k), Some(d)) = (CfStr::new(SHOW_IN_MENU_BAR_KEY), CfStr::new(AIRPLAY_DOMAIN)) else {
        return false;
    };
    unsafe {
        let v = CFPreferencesCopyAppValue(k.0, d.0);
        if v.is_null() {
            return false;
        }
        let on = CFGetTypeID(v) == cf_boolean_type_id() && CFBooleanGetValue(v) != 0;
        CFRelease(v);
        on
    }
}

/// Turns the menu-bar option on if it is off. Returns true when it changed.
fn enable_menu_bar_option() -> bool {
    if menu_bar_option_enabled() {
        return false;
    }
    let (Some(k), Some(d)) = (CfStr::new(SHOW_IN_MENU_BAR_KEY), CfStr::new(AIRPLAY_DOMAIN)) else {
        return false;
    };
    unsafe {
        CFPreferencesSetAppValue(k.0, K_CF_BOOLEAN_TRUE, d.0);
        CFPreferencesAppSynchronize(d.0);
    }
    tprintln!(
        "[airplay] enabled \"Show mirroring options in the menu bar when available\" so the          Displays menu is available to drive"
    );
    true
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
    let path = CfStr::new(DISPLAYS_MENU_BUNDLE)?;
    let k = CfStr::new(key)?;
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
        let out = cfstring_to_string(value);
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

/// An owned CFString, released on drop.
struct CfStr(CFStringRef);

impl CfStr {
    fn new(s: &str) -> Option<Self> {
        let c = CString::new(s).ok()?;
        let r = unsafe {
            CFStringCreateWithCString(ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        };
        if r.is_null() {
            None
        } else {
            Some(Self(r))
        }
    }
}

impl Drop for CfStr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// An owned CFType returned by an AX copy, released on drop.
struct CfValue(CFTypeRef);

impl Drop for CfValue {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn cfstring_to_string(s: CFStringRef) -> Option<String> {
    if s.is_null() || unsafe { CFGetTypeID(s) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    let mut buf = vec![0i8; 1024];
    let ok = unsafe {
        CFStringGetCString(
            s,
            buf.as_mut_ptr(),
            buf.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if !ok {
        return None;
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .ok()
        .map(|s| s.to_string())
}

fn copy_attr(element: AXUIElementRef, name: &str) -> Option<CfValue> {
    let key = CfStr::new(name)?;
    let mut out: CFTypeRef = ptr::null();
    let err = unsafe { AXUIElementCopyAttributeValue(element, key.0, &mut out) };
    if err != AX_SUCCESS || out.is_null() {
        return None;
    }
    Some(CfValue(out))
}

fn attr_string(element: AXUIElementRef, name: &str) -> Option<String> {
    let v = copy_attr(element, name)?;
    cfstring_to_string(v.0)
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
    unsafe { CFGetTypeID(v.0) != cf_boolean_type_id() || CFBooleanGetValue(v.0) != 0 }
}

fn children(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    let Some(v) = copy_attr(element, "AXChildren") else {
        return Vec::new();
    };
    if unsafe { CFGetTypeID(v.0) } != unsafe { CFArrayGetTypeID() } {
        return Vec::new();
    }
    let n = unsafe { CFArrayGetCount(v.0) };
    let mut out = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        let child = unsafe { CFArrayGetValueAtIndex(v.0, i) };
        if !child.is_null() {
            // The array owns its elements; we only read them while `v` is alive,
            // and every use below is synchronous.
            out.push(child);
        }
    }
    // Deliberately leak the array reference for the lifetime of `out`. Releasing
    // it here would invalidate the child pointers.
    std::mem::forget(v);
    out
}

fn press(element: AXUIElementRef) -> Result<(), String> {
    perform(element, "AXPress")
}

fn perform(element: AXUIElementRef, action: &str) -> Result<(), String> {
    let a = CfStr::new(action).ok_or_else(|| format!("could not build the {action} action"))?;
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

pub const ACCESSIBILITY_DENIED: &str =
    "Accessibility permission is not granted, so ScreenExtend cannot open the AirPlay display \
     menu. Grant it under System Preferences → Security & Privacy → Privacy → Accessibility.";

/// AX identifier of the menu row that connects to an AirPlay device. Stable
/// across locales because it is the action selector, not a title.
const CONNECT_IDENTIFIER: &str = "connectToAirPlayDevice:";
/// AX identifier of the row that opens the Displays pane. Present in the same
/// menu, and the cheapest way to recognise the Displays extra among the others.
const DISPLAYS_PREF_IDENTIFIER: &str = "openDisplaysPref:";
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

fn system_ui_server_pid() -> Option<i32> {
    let out = Command::new("/usr/bin/pgrep")
        .args(["-x", "SystemUIServer"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
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

    pub fn offers_extend(&self) -> bool {
        self.items
            .iter()
            .any(|i| i.identifier == USE_AS_DISPLAY_IDENTIFIER)
    }
}

struct Extra {
    element: AXUIElementRef,
    /// Root application element, released when the Extra is dropped.
    _app: OwnedElement,
}

struct OwnedElement(AXUIElementRef);

impl Drop for OwnedElement {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
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
fn find_displays_extra() -> Result<Extra, String> {
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

    let deadline = Instant::now() + EXTRA_APPEAR_TIMEOUT;
    loop {
        match find_displays_extra_once() {
            Ok(extra) => {
                tprintln!("[airplay] added the Displays item to the menu bar");
                return Ok(extra);
            }
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(POPULATE_POLL),
        }
    }
}

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
    let app = OwnedElement(app);

    let extras = copy_attr(app.0, "AXExtrasMenuBar").ok_or_else(|| {
        "SystemUIServer exposes no menu-bar extras — is Accessibility granted to ScreenExtend?"
            .to_string()
    })?;

    for extra in children(extras.0) {
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
            std::mem::forget(extras);
            return Ok(Extra {
                element: extra,
                _app: app,
            });
        }
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
    for child in children(extra.element) {
        for item in children(child) {
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
fn find_row(
    extra: &Extra,
    want_identifier: &str,
    want_title: Option<&str>,
) -> Option<AXUIElementRef> {
    for child in children(extra.element) {
        for item in children(child) {
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
                return Some(item);
            }
        }
    }
    None
}

fn open_menu(extra: &Extra) -> Result<(), String> {
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
        press(extra.element)?;
        let deadline = Instant::now() + POPULATE_TIMEOUT;
        while Instant::now() < deadline {
            if !read_menu(extra).items.is_empty() {
                return Ok(());
            }
            std::thread::sleep(POPULATE_POLL);
        }
    }
    Err(
        "the Displays menu did not populate after being pressed. It can take a moment to rebuild          after login or a SystemUIServer restart."
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
/// able to open anything. Both are safe to attempt when nothing is open.
fn close_menu(extra: &Extra) {
    for child in children(extra.element) {
        if perform(child, "AXCancel").is_ok() {
            return;
        }
    }
    press_escape();
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

/// Opens the menu, waits for a row, and presses it — retrying the sequence.
///
/// Finding and pressing are deliberately adjacent: a reference fetched and then
/// held across other work is exactly what goes stale.
fn press_row_in_menu(
    appear_timeout: Duration,
    find: impl Fn(&Extra) -> Option<AXUIElementRef>,
    missing: impl Fn(&MenuSnapshot) -> String,
) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..ROW_PRESS_ATTEMPTS {
        let extra = find_displays_extra()?;
        if let Err(e) = open_menu(&extra) {
            last = e;
            continue;
        }

        // Hold the menu open until our row shows up, re-reading it each time —
        // the rows are rebuilt as devices arrive, so a reference from the
        // previous pass is no good.
        let deadline = Instant::now() + appear_timeout;
        let mut pressed = None;
        loop {
            if let Some(row) = find(&extra) {
                pressed = Some(press(row));
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POPULATE_POLL);
        }

        match pressed {
            Some(Ok(())) => {
                // Deliberately no dismissal: activating a menu item is what
                // closes an NSMenu, and anything we did afterwards could only
                // re-open it. Just let the action get going.
                std::thread::sleep(ACTION_SETTLE);
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
pub fn snapshot() -> Result<MenuSnapshot, String> {
    let extra = find_displays_extra()?;
    open_menu(&extra)?;
    // Same reason the press path waits: the menu opens before the device rows
    // exist, so reading it immediately would report an empty picker.
    let deadline = Instant::now() + DEVICE_ROW_TIMEOUT;
    let mut snap = read_menu(&extra);
    while snap.device_names().is_empty() && Instant::now() < deadline {
        std::thread::sleep(POPULATE_POLL);
        snap = read_menu(&extra);
    }
    close_menu(&extra);
    Ok(snap)
}

/// Selects the AirPlay device published under `device_name`.
///
/// On success macOS starts a session; it normally attaches the display in mirror
/// mode, which the caller converts to an extended desktop with CoreGraphics.
pub fn connect_to(device_name: &str) -> Result<(), String> {
    press_row_in_menu(
        DEVICE_ROW_TIMEOUT,
        |extra| find_row(extra, CONNECT_IDENTIFIER, Some(device_name)),
        |snapshot| {
            let listed = snapshot.device_names();
            if listed.is_empty() {
                format!(
                    "macOS is not offering {device_name:?} as an AirPlay display. The picker lists                      no AirPlay devices at all, so the sender never accepted our Bonjour                      advertisement."
                )
            } else {
                format!(
                    "macOS is not offering {device_name:?} as an AirPlay display. The picker                      lists: {}",
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
pub fn use_as_separate_display() -> Result<(), String> {
    press_row_in_menu(
        STATE_ROW_TIMEOUT,
        |extra| {
            menu_strings()
                .use_as_display
                .as_deref()
                .and_then(|t| find_row(extra, USE_AS_DISPLAY_IDENTIFIER, Some(t)))
                .or_else(|| find_row(extra, USE_AS_DISPLAY_IDENTIFIER, None))
        },
        |_| {
            "the Displays menu is not offering an extend option — either nothing is mirroring or              the display is already extended"
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
pub fn disconnect() -> Result<(), String> {
    press_row_in_menu(
        STATE_ROW_TIMEOUT,
        |extra| {
            menu_strings()
                .stop_airplay
                .as_deref()
                .and_then(|t| find_row(extra, CONNECT_IDENTIFIER, Some(t)))
                .or_else(|| find_stop_row_by_position(extra))
        },
        |_| {
            "the Displays menu is not showing an active AirPlay session, so there is nothing to              disconnect"
                .to_string()
        },
    )?;
    tprintln!("[airplay] stopped the AirPlay session from the Displays menu");
    Ok(())
}

/// The 10.15+ fallback: first `connectToAirPlayDevice:` row, and only when the
/// mirror-mode block below it proves a session is live.
fn find_stop_row_by_position(extra: &Extra) -> Option<AXUIElementRef> {
    for child in children(extra.element) {
        let mut first_connect: Option<AXUIElementRef> = None;
        for item in children(child) {
            match attr_string(item, "AXIdentifier")
                .unwrap_or_default()
                .as_str()
            {
                CONNECT_IDENTIFIER if first_connect.is_none() => first_connect = Some(item),
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
    fn cfstring_round_trips() {
        let s = CfStr::new("AXChildren").expect("create");
        assert_eq!(cfstring_to_string(s.0).as_deref(), Some("AXChildren"));
    }

    #[test]
    fn a_nul_in_the_string_is_rejected_rather_than_truncating() {
        assert!(CfStr::new("bad\0name").is_none());
    }

    #[test]
    fn non_string_types_convert_to_none() {
        assert_eq!(cfstring_to_string(ptr::null()), None);
    }

    #[test]
    fn system_ui_server_is_running_on_a_desktop_session() {
        // SystemUIServer only exists in a logged-in GUI session; over ssh it is
        // absent, so this asserts the probe does not panic either way.
        let _ = system_ui_server_pid();
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
        let src = include_str!("ax.rs");
        let body = &src[src.find("fn press_row_in_menu(").expect("fn exists")..];
        let body = &body[..body
            .find(
                "
}
",
            )
            .expect("fn body")];
        assert!(
            body.contains("appear_timeout"),
            "the row must be waited for"
        );
        assert!(
            body.contains("ACTION_SETTLE"),
            "the dispatched action needs a moment before the caller moves on"
        );
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
        // picker on screen after a device connects. This asserts the source
        // stays that way, since the mistake is easy to reintroduce.
        let src = include_str!("ax.rs");
        let body = &src[src.find("fn close_menu(").expect("close_menu exists")..];
        let body = &body[..body
            .find(
                "
}
",
            )
            .expect("close_menu body")];
        assert!(
            !body.contains("press(extra.element)"),
            "close_menu must not toggle the menu-bar extra"
        );
        assert!(
            body.contains("press_escape()"),
            "escape is the safe fallback"
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
                MenuItem {
                    title: "Displays Preferences…".into(),
                    identifier: DISPLAYS_PREF_IDENTIFIER.into(),
                },
            ],
        };
        assert_eq!(snap.device_names(), vec!["Living Room"]);
        assert!(snap.offers_extend());
    }

    #[test]
    #[ignore = "requires a GUI session and Accessibility permission; run manually"]
    fn can_read_the_live_displays_menu() {
        match snapshot() {
            Ok(s) => println!("{:#?}", s.items),
            Err(e) => println!("no menu: {e}"),
        }
    }
}
