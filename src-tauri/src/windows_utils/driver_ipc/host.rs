//! What a host tells the driver beyond its monitor list, and when.
//!
//! Each decision is a pure function here so it can be tested without a driver; the virtual
//! display calls them and does the I/O.

use std::sync::OnceLock;

use super::v2::{Hello, Luid, Owner, OwnerToken};
use crate::streamer::config::EncoderVendor;
use crate::windows_capture::d3d11::GpuPreference;

/// How long the driver waits after a host's last connection closes before removing the
/// host's displays.
///
/// It covers the link watcher's whole retry schedule: each attempt is a pipe open (up to 0.5 s
/// while every instance is busy) plus a `Hello` with a 1 s deadline, [`RELINK_BACKOFF_MS`]
/// apart, so six attempts begin inside 15 s even when each runs to its deadline. A crashed
/// host's displays stay that long, and on a hybrid laptop so does the refresh rate one virtual
/// display forces on the panel (about 20 s with the time Windows takes to close a killed
/// process's pipe). The driver caps the grace at 30 s.
pub const OWNER_GRACE_MS: u32 = 15_000;

/// The pause before each further reconnect attempt; the last value repeats for as long as the
/// host lives.
pub const RELINK_BACKOFF_MS: [u64; 4] = [250, 500, 1000, 2000];

/// The pause between reconnect attempts while the driver serves no versioned pipe at all: it is
/// restarting (its v2 task comes up a moment after the v1 one) or was replaced by a driver
/// without it, and neither is helped by asking every 2 s.
pub const RELINK_NO_V2_PAUSE: std::time::Duration = std::time::Duration::from_secs(30);

/// The pause before reconnect attempt `n` (0 is the first, made at once).
pub fn relink_backoff(n: u32) -> std::time::Duration {
    let ms = match n {
        0 => 0,
        n => RELINK_BACKOFF_MS[(n as usize - 1).min(RELINK_BACKOFF_MS.len() - 1)],
    };
    std::time::Duration::from_millis(ms)
}

/// Whether a shared-driver host may reuse a released id. Not on a driver that speaks the
/// versioned protocol, whatever this host's own switches say: the driver keeps a released id
/// under the host that created it, and a display reused there would go when that host goes.
pub fn may_reuse_released(protocol: u32) -> bool {
    protocol < 2
}

/// What a default-mode host takes out of the driver's list at start-up, when the driver reports
/// owners: monitors nobody owns and monitors whose owner has no live connection, never another
/// live host's. `monitors` is `(id, owner prefix)`; `live_owners` the prefixes with a connection.
/// The removal keeps the list one run from 0 or 1 (`compact`), so some may have to stay.
pub fn startup_removals(monitors: &[(u32, Option<String>)], live_owners: &[String]) -> Vec<u32> {
    let mut ids: Vec<u32> = monitors.iter().map(|(id, _)| *id).collect();
    let mut removable: Vec<u32> = monitors
        .iter()
        .filter(|(_, owner)| !owner.as_ref().is_some_and(|o| live_owners.contains(o)))
        .map(|(id, _)| *id)
        .collect();
    crate::windows_utils::virtual_display::compact(&mut ids, &mut removable)
}

/// Where the driver should compose a display about to be created: the adapter DXGI lists
/// outputs under when duplication is the capture (it delivers nothing otherwise), else the
/// encoder's adapter when the render switch is on. `None` = do not ask.
pub fn render_want(
    dxgi_capture: bool,
    listing: Option<Luid>,
    follow_encoder: bool,
    encoder_target: Option<Luid>,
) -> Option<Option<Luid>> {
    if dxgi_capture && listing.is_some() {
        Some(listing)
    } else if follow_encoder {
        Some(encoder_target)
    } else {
        None
    }
}

/// The two driver features a host may use, each behind its switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkOptions {
    /// Declare an owner, so the driver removes this host's displays when it dies. Ships on;
    /// `SE_TUNE_OFF=driver_owned_monitors` turns it off.
    pub owned_monitors: bool,
    /// Ask the driver to compose on the encoder's adapter. Ships off;
    /// `SE_TUNE_ON=render_adapter_follows_encoder` turns it on.
    pub render_follows_encoder: bool,
}

impl LinkOptions {
    pub const OFF: Self = Self {
        owned_monitors: false,
        render_follows_encoder: false,
    };

    /// The switches, read once per process.
    pub fn from_switches() -> Self {
        static READ: OnceLock<LinkOptions> = OnceLock::new();
        *READ.get_or_init(|| {
            use crate::windows_utils::streamer::tune;
            Self {
                owned_monitors: tune::on("driver_owned_monitors"),
                render_follows_encoder: tune::candidate("render_adapter_follows_encoder"),
            }
        })
    }
}

/// This process's owner token: one per process, made on first use, kept only in memory. A new
/// process is a new owner, so a restarted host never inherits the displays of the one before.
pub fn owner_token() -> Option<&'static OwnerToken> {
    static TOKEN: OnceLock<Option<OwnerToken>> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            let token = OwnerToken::generate();
            if token.is_none() {
                teprintln!(
                    "[virtual-display] the OS random source failed; declaring no owner, so a host \
                     that dies leaves its displays attached"
                );
            }
            token
        })
        .as_ref()
}

/// The `Hello` a host connection sends, or `None` (both switches off) to send nothing new at all.
pub fn hello_for(options: LinkOptions, token: Option<&OwnerToken>) -> Option<Hello> {
    let owner = token.filter(|_| options.owned_monitors).map(|t| Owner {
        token: t.clone(),
        grace_ms: OWNER_GRACE_MS,
    });
    if owner.is_none() && !options.render_follows_encoder {
        return None;
    }
    Some(Hello {
        client: Some(format!("ScreenExtend {}", env!("CARGO_PKG_VERSION"))),
        owner,
    })
}

/// One adapter as DXGI lists it, in the order the capture device's preference enumerates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterCandidate {
    pub luid: Luid,
    pub vendor_id: u32,
    pub software: bool,
    /// The adapter of our own driver's devnode. DXGI lists the indirect display adapter under
    /// its render adapter's name and vendor, so a vendor match alone could pick it; composing a
    /// display on itself is not a request worth sending.
    pub ours: bool,
    pub description: String,
}

/// The adapter a session's encoder will run on, known before its display exists, or `None`
/// for "no preference".
///
/// Hardware encoders open on the capture device's adapter, so this resolves
/// `capture_gpu_preference(vendor)` the way the capture device does
/// (`windows_capture::d3d11::resolve_adapter`), over the same high-performance ordering, never
/// by adapter name. x264 reads every frame back to the CPU and gains nothing from either GPU,
/// so it asks for nothing and Windows keeps its choice.
pub fn render_target(
    vendor: EncoderVendor,
    ordered: &[AdapterCandidate],
) -> Option<&AdapterCandidate> {
    let usable = || ordered.iter().filter(|a| !a.software && !a.ours);
    match crate::windows_utils::streamer::capture_gpu_preference(vendor) {
        GpuPreference::Vendor(id) => usable()
            .find(|a| a.vendor_id == id)
            .or_else(|| usable().next()),
        GpuPreference::HighPerformance => usable().next(),
        GpuPreference::MinimumPower | GpuPreference::Default => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderStep {
    Leave,
    Send(Option<Luid>),
}

/// Whether to send `SetRenderAdapter`, given what the session wants, what the driver last
/// reported (`None` when it has not said) and the values it has already refused.
///
/// A change re-creates every live swap chain, so nothing is sent when the driver already has
/// the value. A refused adapter is treated as no preference, and a refused value is never sent
/// twice.
pub fn render_step(
    want: Option<Luid>,
    reported: Option<Option<Luid>>,
    refused: &[Option<Luid>],
) -> RenderStep {
    let want = if refused.contains(&want) { None } else { want };
    if reported == Some(want) || refused.contains(&want) {
        RenderStep::Leave
    } else {
        RenderStep::Send(want)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::windows_utils::streamer::{INTEL_PCI_VENDOR_ID, NVIDIA_PCI_VENDOR_ID};

    const NV: Luid = Luid {
        low: 97388,
        high: 0,
    };
    const INTEL: Luid = Luid {
        low: 74978,
        high: 0,
    };
    const IDD: Luid = Luid {
        low: 2121341485,
        high: 0,
    };

    fn cand(luid: Luid, vendor_id: u32, software: bool, ours: bool) -> AdapterCandidate {
        AdapterCandidate {
            luid,
            vendor_id,
            software,
            ours,
            description: String::new(),
        }
    }

    /// A hybrid laptop's list in DXGI's high-performance order: the dGPU, the indirect display
    /// adapter (listed under the iGPU's name), the iGPU, and Basic Render.
    fn hybrid() -> Vec<AdapterCandidate> {
        vec![
            cand(NV, NVIDIA_PCI_VENDOR_ID, false, false),
            cand(IDD, INTEL_PCI_VENDOR_ID, false, true),
            cand(INTEL, INTEL_PCI_VENDOR_ID, false, false),
            cand(
                Luid {
                    low: 97324,
                    high: 0,
                },
                0x1414,
                true,
                false,
            ),
        ]
    }

    fn target(vendor: EncoderVendor, list: &[AdapterCandidate]) -> Option<Luid> {
        render_target(vendor, list).map(|a| a.luid)
    }

    #[test]
    fn the_render_adapter_is_the_encoders_for_each_choice() {
        let list = hybrid();
        assert_eq!(target(EncoderVendor::Auto, &list), Some(NV));
        assert_eq!(target(EncoderVendor::Nvidia, &list), Some(NV));
        // never the indirect display adapter, though it is listed first under Intel's id
        assert_eq!(target(EncoderVendor::Intel, &list), Some(INTEL));
        assert_eq!(target(EncoderVendor::Software, &list), None);
        // no AMD adapter: like the capture device, fall back to the first hardware one
        assert_eq!(target(EncoderVendor::Amd, &list), Some(NV));
        // only a software adapter: nothing worth asking for
        let soft = [cand(Luid { low: 1, high: 0 }, 0x1414, true, false)];
        assert_eq!(target(EncoderVendor::Auto, &soft), None);
        assert_eq!(target(EncoderVendor::Intel, &[]), None);
    }

    #[test]
    fn a_preference_is_sent_only_when_it_changes_anything() {
        // the driver has no preference and the encoder wants the dGPU
        assert_eq!(
            render_step(Some(NV), Some(None), &[]),
            RenderStep::Send(Some(NV))
        );
        // already there: a send would only re-create live swap chains
        assert_eq!(
            render_step(Some(NV), Some(Some(NV)), &[]),
            RenderStep::Leave
        );
        // another session's encoder set the iGPU: the newest session wins
        assert_eq!(
            render_step(Some(NV), Some(Some(INTEL)), &[]),
            RenderStep::Send(Some(NV))
        );
        // x264 after an NVENC session: clear it. Nothing to clear: leave it
        assert_eq!(
            render_step(None, Some(Some(NV)), &[]),
            RenderStep::Send(None)
        );
        assert_eq!(render_step(None, Some(None), &[]), RenderStep::Leave);
        // the driver never said: send
        assert_eq!(render_step(Some(NV), None, &[]), RenderStep::Send(Some(NV)));
    }

    #[test]
    fn a_refused_adapter_is_no_preference_and_is_never_sent_again() {
        let refused = [Some(NV)];
        assert_eq!(
            render_step(Some(NV), Some(None), &refused),
            RenderStep::Leave
        );
        assert_eq!(
            render_step(Some(NV), Some(Some(INTEL)), &refused),
            RenderStep::Send(None)
        );
        // clearing was refused too: give up rather than loop
        assert_eq!(
            render_step(Some(NV), Some(Some(INTEL)), &[Some(NV), None]),
            RenderStep::Leave
        );
        assert_eq!(
            render_step(Some(INTEL), Some(None), &refused),
            RenderStep::Send(Some(INTEL))
        );
    }

    #[test]
    fn the_grace_covers_six_worst_case_attempts() {
        // each attempt: up to 0.5 s waiting for a pipe instance and 1 s for the Hello
        let attempt = 1_500u64;
        let mut start = 0u64;
        let mut begun = 0;
        for n in 0..10 {
            start += relink_backoff(n).as_millis() as u64;
            if start + attempt <= u64::from(OWNER_GRACE_MS) {
                begun += 1;
            }
            start += attempt;
        }
        assert!(begun >= 5, "{begun}");
        assert_eq!(relink_backoff(0).as_millis(), 0);
        assert_eq!(relink_backoff(9).as_millis(), 2000);
    }

    #[test]
    fn released_ids_are_reused_only_on_the_unversioned_pipe() {
        assert!(may_reuse_released(1));
        assert!(!may_reuse_released(2));
    }

    #[test]
    fn the_startup_clear_spares_live_owners_and_keeps_one_run() {
        let o = |s: &str| Some(s.to_string());
        let live = vec!["aaaaaa".to_string()];
        // 0 unowned, 1 a dead owner's, 2 a live owner's: 0 can go ([1, 2] still attaches), 1
        // cannot without leaving 2 alone
        let m = [(0, None), (1, o("dddddd")), (2, o("aaaaaa"))];
        assert_eq!(startup_removals(&m, &live), vec![0]);
        // the live owner holds 0: 1 and 2 go, top first
        let m = [(0, o("aaaaaa")), (1, None), (2, o("dddddd"))];
        assert_eq!(startup_removals(&m, &live), vec![2, 1]);
        // nobody live: everything goes
        let m = [(0, None), (1, o("dddddd"))];
        assert_eq!(startup_removals(&m, &[]), vec![1, 0]);
    }

    #[test]
    fn duplication_overrides_the_encoder_only_when_it_is_the_capture() {
        assert_eq!(render_want(true, Some(NV), false, None), Some(Some(NV)));
        assert_eq!(
            render_want(true, Some(NV), true, Some(INTEL)),
            Some(Some(NV))
        );
        assert_eq!(
            render_want(false, Some(NV), true, Some(INTEL)),
            Some(Some(INTEL))
        );
        assert_eq!(render_want(false, Some(NV), false, Some(INTEL)), None);
        assert_eq!(render_want(true, None, false, None), None);
    }

    #[test]
    fn both_switches_off_send_no_hello() {
        let token = OwnerToken::generate().unwrap();
        assert_eq!(hello_for(LinkOptions::OFF, Some(&token)), None);
        let owned = hello_for(
            LinkOptions {
                owned_monitors: true,
                render_follows_encoder: false,
            },
            Some(&token),
        )
        .unwrap();
        let owner = owned.owner.unwrap();
        assert_eq!(owner.token, token);
        assert_eq!(owner.grace_ms, OWNER_GRACE_MS);
        let render_only = hello_for(
            LinkOptions {
                owned_monitors: false,
                render_follows_encoder: true,
            },
            Some(&token),
        )
        .unwrap();
        assert_eq!(render_only.owner, None);
        // owned on but no token could be made: a Hello only if the other feature wants one
        let no_token = LinkOptions {
            owned_monitors: true,
            render_follows_encoder: false,
        };
        assert_eq!(hello_for(no_token, None), None);
    }
}
