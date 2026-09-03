//! Bonjour advertisement for the fake AirPlay receiver.
//!
//! Hand-written FFI against `dns_sd.h` in libSystem rather than a crate: the
//! symbols are 10.5-era, libSystem is linked into every Rust binary already, and
//! it keeps the pure-Rust responders (which want port 5353 for themselves) out
//! of the tree. Registering with a NULL callback hands the advertisement to
//! `mDNSResponder`, so there is no event loop to pump — we only have to hold the
//! `DNSServiceRef` alive and deallocate it on teardown.

use std::ffi::{c_char, c_void, CString};
use std::ptr;

type DNSServiceRef = *mut c_void;
type DNSServiceFlags = u32;
type DNSServiceErrorType = i32;

/// `dns_sd.h`: `typedef union _TXTRecordRef_t { char PrivateData[16]; char *ForceNaturalAlignment; }`.
/// The header asserts `sizeof(TXTRecordRef) == 16`.
#[repr(C, align(8))]
struct TXTRecordRefRaw([u8; 16]);

unsafe extern "C" {
    fn DNSServiceRegister(
        sdRef: *mut DNSServiceRef,
        flags: DNSServiceFlags,
        interfaceIndex: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txtLen: u16,
        txtRecord: *const c_void,
        callBack: *const c_void,
        context: *mut c_void,
    ) -> DNSServiceErrorType;

    fn DNSServiceRefDeallocate(sdRef: DNSServiceRef);

    fn TXTRecordCreate(txtRecord: *mut TXTRecordRefRaw, bufferLen: u16, buffer: *mut c_void);
    fn TXTRecordDeallocate(txtRecord: *mut TXTRecordRefRaw);
    fn TXTRecordSetValue(
        txtRecord: *mut TXTRecordRefRaw,
        key: *const c_char,
        valueSize: u8,
        value: *const c_void,
    ) -> DNSServiceErrorType;
    fn TXTRecordGetLength(txtRecord: *const TXTRecordRefRaw) -> u16;
    fn TXTRecordGetBytesPtr(txtRecord: *const TXTRecordRefRaw) -> *const c_void;
}

const KDNS_SERVICE_FLAGS_NO_AUTO_RENAME: DNSServiceFlags = 0x0;

fn err(what: &str, code: DNSServiceErrorType) -> String {
    format!("{what} failed (DNSServiceErrorType {code})")
}

/// The interface to register on, and why it is never `kDNSServiceInterfaceIndexAny`.
///
/// macOS refuses to offer an AirPlay target it believes is itself:
/// `APTransportDeviceIsSelf` is `deviceID == primaryMAC || IsLocallyAdvertised`,
/// and `IsLocallyAdvertised` is true as soon as *any* Bonjour record for the
/// device is seen with `interfaceIndex == 0`. Registering with the "any"
/// interface produces exactly such a record. The override that would allow a
/// local endpoint anyway is gated on the host being an Apple TV or an AirPort
/// speaker, so on a Mac it is permanently off.
///
/// So: pick a real interface index, and pair it with a synthetic `deviceid`
/// (see [`Identity::derive`]) to clear the other half of the test.
fn primary_interface_index() -> Option<u32> {
    use std::ffi::CStr;

    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut best: Option<u32> = None;
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;

            if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
                continue;
            }
            if (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let flags = ifa.ifa_flags as i32;
            if flags & libc::IFF_LOOPBACK != 0
                || flags & libc::IFF_UP == 0
                || flags & libc::IFF_RUNNING == 0
            {
                continue;
            }
            let Ok(name) = CStr::from_ptr(ifa.ifa_name).to_str() else {
                continue;
            };
            // Apple Wireless Direct Link and the low-latency companion link are
            // not general-purpose interfaces and are a poor place to advertise.
            if name.starts_with("awdl") || name.starts_with("llw") || name.starts_with("utun") {
                continue;
            }
            let Ok(cname) = std::ffi::CString::new(name) else {
                continue;
            };
            let index = libc::if_nametoindex(cname.as_ptr());
            if index != 0 && best.is_none() {
                best = Some(index);
            }
        }
        libc::freeifaddrs(ifap);
        best
    }
}

/// A built TXT record. Holds the mDNSResponder-owned buffer until dropped.
struct TxtRecord {
    raw: Box<TXTRecordRefRaw>,
}

impl TxtRecord {
    fn build(pairs: &[(&str, String)]) -> Result<Self, String> {
        let mut raw = Box::new(TXTRecordRefRaw([0u8; 16]));
        unsafe { TXTRecordCreate(&mut *raw, 0, ptr::null_mut()) };
        let mut this = Self { raw };
        for (k, v) in pairs {
            this.set(k, v)?;
        }
        Ok(this)
    }

    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let ckey = CString::new(key).map_err(|_| format!("TXT key {key:?} contains a NUL"))?;
        if value.len() > u8::MAX as usize {
            return Err(format!("TXT value for {key:?} is longer than 255 bytes"));
        }
        let rc = unsafe {
            TXTRecordSetValue(
                &mut *self.raw,
                ckey.as_ptr(),
                value.len() as u8,
                value.as_ptr() as *const c_void,
            )
        };
        if rc != 0 {
            return Err(err(&format!("TXTRecordSetValue({key})"), rc));
        }
        Ok(())
    }

    fn bytes(&self) -> (u16, *const c_void) {
        unsafe {
            (
                TXTRecordGetLength(&*self.raw),
                TXTRecordGetBytesPtr(&*self.raw),
            )
        }
    }
}

impl Drop for TxtRecord {
    fn drop(&mut self) {
        unsafe { TXTRecordDeallocate(&mut *self.raw) };
    }
}

/// One registered Bonjour service. Deregisters on drop.
pub struct Service {
    sd_ref: DNSServiceRef,
    // The TXT buffer must outlive the registration.
    _txt: TxtRecord,
    name: String,
}

// DNSServiceRef is an opaque handle; mDNSResponder owns the socket. Deallocating
// from a thread other than the registering one is supported.
unsafe impl Send for Service {}

impl Service {
    fn register(
        name: &str,
        regtype: &str,
        port: u16,
        interface_index: u32,
        pairs: &[(&str, String)],
    ) -> Result<Self, String> {
        let txt = TxtRecord::build(pairs)?;
        let (txt_len, txt_ptr) = txt.bytes();

        let cname = CString::new(name).map_err(|_| "service name contains a NUL".to_string())?;
        let ctype = CString::new(regtype).map_err(|_| "service type contains a NUL".to_string())?;

        let mut sd_ref: DNSServiceRef = ptr::null_mut();
        let rc = unsafe {
            DNSServiceRegister(
                &mut sd_ref,
                KDNS_SERVICE_FLAGS_NO_AUTO_RENAME,
                interface_index,
                cname.as_ptr(),
                ctype.as_ptr(),
                ptr::null(),
                ptr::null(),
                port.to_be(),
                txt_len,
                txt_ptr,
                ptr::null(),
                ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(err(&format!("DNSServiceRegister({regtype})"), rc));
        }
        Ok(Self {
            sd_ref,
            _txt: txt,
            name: name.to_string(),
        })
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if !self.sd_ref.is_null() {
            unsafe { DNSServiceRefDeallocate(self.sd_ref) };
            self.sd_ref = ptr::null_mut();
        }
        tprintln!("[airplay] withdrew Bonjour advertisement {:?}", self.name);
    }
}

/// The identity we advertise. Stable per install so the sender's pairing cache
/// stays valid across restarts.
#[derive(Clone, Debug)]
pub struct Identity {
    /// Colon-separated *lower-case* MAC-shaped string, e.g. `a2:b4:c6:d8:ea:fc`.
    /// The case matters: the reference receivers lower-case the `_airplay._tcp`
    /// `deviceid` and upper-case the `_raop._tcp` instance name, and a sender
    /// that cross-references the two will not match if we get it backwards.
    pub device_id: String,
    /// 64 lower-case hex chars.
    pub public_key: String,
    /// A UUID string.
    pub pairing_id: String,
}

impl Identity {
    /// Derives a stable, locally-administered identity from the machine's
    /// hardware UUID. Deliberately *not* the host's real NIC MAC: the sender
    /// runs on the same Mac as this receiver, and a `deviceid` that collides
    /// with one of the host's own interfaces is the obvious thing for macOS to
    /// filter out.
    pub fn derive() -> Self {
        let seed = machine_seed();
        let h = fnv1a(&seed);

        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = ((h >> (8 * i)) & 0xff) as u8;
        }
        // Locally administered, unicast.
        mac[0] = (mac[0] | 0x02) & 0xfe;
        let device_id = mac
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":");

        // 32 bytes of stable pseudo-key material. This is never used as a real
        // key — nothing on the mirroring path verifies a receiver signature —
        // but the field must be present and stable.
        let mut pk = String::with_capacity(64);
        let mut x = h;
        for _ in 0..32 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            pk.push_str(&format!("{:02x}", (x >> 33) as u8));
        }

        let pairing_id = format!(
            "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
            (h >> 32) as u32,
            (h >> 16) as u16,
            (h & 0x0fff) as u16,
            ((h >> 12) & 0x0fff) as u16,
            h & 0xffff_ffff_ffff,
        );

        Self {
            device_id,
            public_key: pk,
            pairing_id,
        }
    }

    /// Upper-case, colon-free — the form the `_raop._tcp` instance name takes.
    fn device_id_compact(&self) -> String {
        self.device_id.replace(':', "").to_ascii_uppercase()
    }
}

fn machine_seed() -> String {
    let out = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output();
    if let Ok(out) = out {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if line.contains("IOPlatformUUID") {
                if let Some(v) = line.split('=').nth(1) {
                    let v = v.trim().trim_matches('"').to_string();
                    if !v.is_empty() {
                        return v;
                    }
                }
            }
        }
    }
    whoami::hostname().unwrap_or_else(|_| "ScreenExtend".to_string())
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Model string we impersonate.
///
/// Matched to the reference receiver that is field-proven against a macOS
/// sender on this OS era rather than picked for its capabilities: a newer model
/// or `srcvers` pushes the sender down AirPlay-2 code paths this receiver does
/// not implement.
pub const MODEL: &str = "AppleTV2,1";
/// `srcvers` / `vs`, paired with [`MODEL`].
pub const SOURCE_VERSION: &str = "220.68";

/// Base feature word with mirroring on. Bit 27 ("supports legacy pairing") is
/// applied separately — see [`features_hex`].
const FEATURES_BASE: u64 = 0x527F_FEE6;
const FEATURES_LEGACY_PAIRING_BIT: u64 = 1 << 27;

pub fn features_word(legacy_pairing: bool) -> u64 {
    if legacy_pairing {
        FEATURES_BASE | FEATURES_LEGACY_PAIRING_BIT
    } else {
        FEATURES_BASE & !FEATURES_LEGACY_PAIRING_BIT
    }
}

pub fn features_hex(legacy_pairing: bool) -> String {
    format!("0x{:X},0x0", features_word(legacy_pairing))
}

/// TXT pairs for `_airplay._tcp`. Also the source of the blob we hand back from
/// the qualifier form of `GET /info`, so the two can never drift apart.
pub fn airplay_txt_pairs(id: &Identity, legacy_pairing: bool) -> Vec<(&'static str, String)> {
    vec![
        ("deviceid", id.device_id.clone()),
        ("features", features_hex(legacy_pairing)),
        ("flags", "0x4".to_string()),
        ("model", MODEL.to_string()),
        // Deliberately no `pw`: the known-good advertisement is exactly these
        // eight keys, and senders on this OS era have been seen to ignore
        // receivers carrying extra ones.
        ("pk", id.public_key.clone()),
        ("pi", id.pairing_id.clone()),
        ("srcvers", SOURCE_VERSION.to_string()),
        ("vv", "2".to_string()),
    ]
}

/// The same pairs in DNS-SD TXT wire form: each entry a length byte followed by
/// `key=value`.
pub fn airplay_txt_blob(id: &Identity, legacy_pairing: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in airplay_txt_pairs(id, legacy_pairing) {
        let entry = format!("{k}={v}");
        let len = entry.len().min(u8::MAX as usize);
        out.push(len as u8);
        out.extend_from_slice(&entry.as_bytes()[..len]);
    }
    out
}

/// Both Bonjour registrations for one receiver, withdrawn together on drop.
pub struct Advertisement {
    _airplay: Service,
    _raop: Service,
}

impl Advertisement {
    /// Publishes `_airplay._tcp` and `_raop._tcp` on the same TCP port, the way
    /// every reference receiver does.
    pub fn publish(
        name: &str,
        port: u16,
        id: &Identity,
        legacy_pairing: bool,
    ) -> Result<Self, String> {
        let features = features_hex(legacy_pairing);

        let interface_index = primary_interface_index().ok_or_else(|| {
            "no usable network interface to advertise on. The AirPlay display fallback needs one,              because macOS ignores an AirPlay target whose Bonjour records arrive on the              \"any\" interface as if it were the Mac itself."
                .to_string()
        })?;

        let airplay = Service::register(
            name,
            "_airplay._tcp",
            port,
            interface_index,
            &airplay_txt_pairs(id, legacy_pairing),
        )?;

        // RAOP instance names are `<MAC-without-colons>@<name>`.
        let raop_name = format!("{}@{}", id.device_id_compact(), name);
        let raop = Service::register(
            &raop_name,
            "_raop._tcp",
            port,
            interface_index,
            &[
                ("txtvers", "1".to_string()),
                ("ch", "2".to_string()),
                ("cn", "0,1,2,3".to_string()),
                ("da", "true".to_string()),
                ("et", "0,3,5".to_string()),
                ("vv", "2".to_string()),
                ("ft", features),
                ("am", MODEL.to_string()),
                ("md", "0,1,2".to_string()),
                ("rhd", "5.6.0.0".to_string()),
                ("pw", "false".to_string()),
                ("sr", "44100".to_string()),
                ("ss", "16".to_string()),
                ("sv", "false".to_string()),
                ("tp", "UDP".to_string()),
                ("sf", "0x4".to_string()),
                ("vs", SOURCE_VERSION.to_string()),
                ("vn", "65537".to_string()),
                ("pk", id.public_key.clone()),
            ],
        )?;

        tprintln!(
            "[airplay] advertising {name:?} on port {port}, interface index {interface_index},              deviceid {} (features {})",
            id.device_id,
            features_hex(legacy_pairing),
        );
        Ok(Self {
            _airplay: airplay,
            _raop: raop,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_and_locally_administered() {
        let a = Identity::derive();
        let b = Identity::derive();
        assert_eq!(a.device_id, b.device_id);
        assert_eq!(a.public_key, b.public_key);
        assert_eq!(a.public_key.len(), 64);

        let first: u8 = u8::from_str_radix(&a.device_id[0..2], 16).unwrap();
        assert_eq!(first & 0x03, 0x02, "must be locally administered + unicast");

        // The two services disagree on case on purpose; see `Identity`.
        assert_eq!(a.device_id, a.device_id.to_ascii_lowercase());
        assert_eq!(
            a.device_id_compact(),
            a.device_id.replace(':', "").to_ascii_uppercase()
        );
    }

    #[test]
    fn the_airplay_txt_is_exactly_the_known_good_key_set() {
        let id = Identity::derive();
        let keys: Vec<&str> = airplay_txt_pairs(&id, false)
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(
            keys,
            vec!["deviceid", "features", "flags", "model", "pk", "pi", "srcvers", "vv"],
        );
    }

    #[test]
    fn the_txt_blob_is_length_prefixed_wire_form() {
        let id = Identity::derive();
        let blob = airplay_txt_blob(&id, false);
        let mut i = 0;
        let mut seen = 0;
        while i < blob.len() {
            let len = blob[i] as usize;
            assert!(
                len > 0 && i + 1 + len <= blob.len(),
                "entry {seen} overruns"
            );
            let entry = std::str::from_utf8(&blob[i + 1..i + 1 + len]).expect("utf8");
            assert!(entry.contains('='), "{entry:?} is not key=value");
            i += 1 + len;
            seen += 1;
        }
        assert_eq!(seen, 8);
    }

    #[test]
    fn a_real_interface_index_is_found() {
        // Registering on index 0 makes macOS treat the receiver as the host
        // itself, so this must never fall back to it.
        let idx = primary_interface_index();
        assert!(
            idx.is_some(),
            "a running Mac has at least one usable interface"
        );
        assert_ne!(idx, Some(0));
    }

    #[test]
    fn the_screen_and_advertiser_feature_bits_are_set() {
        // bit 7 = SupportsAirPlayScreen, bit 30 = HasUnifiedAdvertiserInfo.
        for legacy in [false, true] {
            let w = features_word(legacy);
            assert_eq!(w & (1 << 7), 1 << 7, "screen mirroring bit must be set");
            assert_eq!(w & (1 << 30), 1 << 30, "unified advertiser bit must be set");
        }
    }

    #[test]
    fn feature_bit_27_toggles() {
        assert_eq!(features_hex(false), "0x527FFEE6,0x0");
        assert_eq!(features_hex(true), "0x5A7FFEE6,0x0");
    }

    #[test]
    #[ignore = "registers a real Bonjour service; run manually"]
    fn publishes_and_withdraws() {
        let id = Identity::derive();
        let ad = Advertisement::publish("ScreenExtend Test", 7000, &id, false).expect("publish");
        std::thread::sleep(std::time::Duration::from_secs(2));
        drop(ad);
    }
}
