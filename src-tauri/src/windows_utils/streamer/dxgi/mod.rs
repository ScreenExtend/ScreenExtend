pub mod cursor;

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_TYPELESS,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutput6, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
    DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTPUT_DESC,
};

use crate::windows_capture::d3d11::AdapterInfo;
use cursor::{build_sprite, quad_verts, BlendKind, CursorSprite, QuadRenderer, ScRgbPass};

const REDUP_MAX_RETRIES: u32 = 40;

/// How many `DXGI_ERROR_INVALID_CALL` recoveries to attempt before giving up. A settling virtual
/// display produces a handful; an unbounded stream of them is a real fault.
const INVALID_CALL_MAX_RECOVERIES: u32 = 20;

/// How many times duplication may be re-established without a single frame coming out of it.
///
/// An output that hands back `DXGI_ERROR_ACCESS_LOST` on every acquire (a freshly created IDD
/// virtual display can) re-duplicates successfully every time, so a count of failed
/// re-duplications never ends it.
const REDUP_WITHOUT_FRAME_MAX: u32 = 40;

/// The "re-established but starving" watchdog, kept apart from the D3D calls so it can be tested.
///
/// Reset only by a frame handed onward, never by an acquire that merely returns: a starving IDD
/// virtual display re-established 1,190 times in 30 s and delivered 0 frames, and every cycle
/// contained a returning acquire.
#[derive(Default)]
struct RedupWatchdog {
    /// Re-establishments since the last *delivered* frame.
    since_frame: u32,
}

impl RedupWatchdog {
    /// A frame reached the pipeline: the run ends.
    fn on_frame_delivered(&mut self) {
        self.since_frame = 0;
    }

    /// Duplication was re-established. `Err` once the output has proved it cannot be captured.
    fn on_reestablished(&mut self) -> Result<()> {
        self.since_frame += 1;
        if self.since_frame > REDUP_WITHOUT_FRAME_MAX {
            bail!(
                "duplication was re-established {} times without ever delivering a \
                 frame; this output cannot be captured with DXGI Desktop Duplication",
                self.since_frame
            );
        }
        Ok(())
    }

    /// The first, then every tenth: one line per re-establishment floods the log bus.
    fn should_log(&self) -> bool {
        self.since_frame == 1 || self.since_frame.is_multiple_of(10)
    }

    fn count(&self) -> u32 {
        self.since_frame
    }
}

/// Holds an acquired duplication frame and releases it on drop.
///
/// `AcquireNextFrame` and `ReleaseFrame` are a strict pair: miss one release and every later
/// acquire returns `DXGI_ERROR_INVALID_CALL`. `poll` has a `?` between the two, so the release
/// cannot be left to the fall-through path.
struct AcquiredFrame<'a>(&'a IDXGIOutputDuplication);

impl Drop for AcquiredFrame<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.0.ReleaseFrame() };
    }
}

fn union(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

fn union_rect(bounds: &mut Option<RECT>, r: RECT) {
    *bounds = Some(match *bounds {
        Some(b) => union(b, r),
        None => r,
    });
}

/// True when a rectangle already spans a whole `width` x `height` surface, in which case the
/// incremental path would cost more than a straight `CopyResource`.
fn covers_whole(r: RECT, width: u32, height: u32) -> bool {
    r.left <= 0 && r.top <= 0 && r.right >= width as i32 && r.bottom >= height as i32
}

/// What one acquired desktop image's metadata said changed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum FrameChange {
    /// The driver said, positively, that nothing in the image changed.
    Empty,
    /// Everything that changed lies inside this rectangle, in the duplication surface's own
    /// pixel coordinates.
    Region(RECT),
    /// Not known: coalesced frames, no metadata, or a failed metadata query.
    Unknown,
}

/// What `composite` lacks relative to `desktop`, accumulated over every copy into `desktop`
/// since [`Duplicator::frame`] last consumed it.
///
/// The pipeline polls several times per staged frame, and each poll with new content overwrites
/// `desktop` whole. The region `frame()` has to refresh is therefore the union over all of those
/// polls, not the last one's, and one poll whose change is unknown makes the whole image suspect
/// until the next full refresh.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Pending {
    /// `composite` holds everything `desktop` does, apart from the cursor.
    #[default]
    Clean,
    /// Only this rectangle differs.
    Region(RECT),
    /// Anything may differ.
    Full,
}

impl Pending {
    /// A copy into `desktop` happened, and its metadata said `change`.
    fn note(&mut self, change: FrameChange) {
        *self = match (*self, change) {
            (Pending::Full, _) | (_, FrameChange::Unknown) => Pending::Full,
            (p, FrameChange::Empty) => p,
            (Pending::Clean, FrameChange::Region(r)) => Pending::Region(r),
            (Pending::Region(a), FrameChange::Region(r)) => Pending::Region(union(a, r)),
        };
    }
}

/// How `frame()` brings `composite` up to date on the unrotated, cursor-drawn path.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Refresh {
    /// Copy the whole of `desktop`.
    Whole,
    /// Copy this rectangle, if any, plus wherever the previous cursor was drawn.
    Partial(Option<RECT>),
}

fn plan_refresh(pending: Pending, composite_valid: bool, width: u32, height: u32) -> Refresh {
    if !composite_valid {
        return Refresh::Whole;
    }
    match pending {
        Pending::Clean => Refresh::Partial(None),
        Pending::Full => Refresh::Whole,
        Pending::Region(r) if covers_whole(r, width, height) => Refresh::Whole,
        Pending::Region(r) => Refresh::Partial(Some(r)),
    }
}

/// Why a re-duplication failed, which decides whether to keep trying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RedupFailure {
    /// Duplication is refused for now and will come back on its own, however long that takes:
    /// the secure desktop (UAC, Ctrl+Alt+Del, the lock screen) answers `E_ACCESSDENIED`, a
    /// remote session switch `DXGI_ERROR_SESSION_DISCONNECTED`. Waited out without a limit.
    Denied,
    /// Usually a transition, but can be permanent for this device and this output:
    /// `DXGI_ERROR_UNSUPPORTED` (a desktop switch or a mode in transition, or an adapter that no
    /// longer composes the output) and `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE` (another duplication
    /// holding the output). Retried like [`RedupFailure::Denied`], but only until
    /// [`UNAVAILABLE_DEADLINE`].
    Unavailable,
    /// The device under the duplication is gone; nothing built on it can be used again.
    DeviceLost,
    /// Anything else: bounded retries, then the capture stops.
    Other,
}

fn is_device_lost(code: HRESULT) -> bool {
    code == DXGI_ERROR_DEVICE_REMOVED
        || code == DXGI_ERROR_DEVICE_RESET
        || code == DXGI_ERROR_DEVICE_HUNG
        || code == DXGI_ERROR_DRIVER_INTERNAL_ERROR
}

fn classify_redup_failure(code: HRESULT) -> RedupFailure {
    if code == E_ACCESSDENIED || code == DXGI_ERROR_SESSION_DISCONNECTED {
        RedupFailure::Denied
    } else if code == DXGI_ERROR_UNSUPPORTED || code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
        RedupFailure::Unavailable
    } else if is_device_lost(code) {
        RedupFailure::DeviceLost
    } else {
        RedupFailure::Other
    }
}

/// Why a duplication could not start yet. Only ever made from a code the duplication call itself
/// returned on the adapter that decides, never from the text of an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    /// Not listed yet, its mode not settled, or the desktop busy setting the monitor up
    /// (`DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`, `DXGI_ERROR_ACCESS_LOST`, `DXGI_ERROR_INVALID_CALL`).
    Settling,
    /// The secure desktop or a disconnected session (`E_ACCESSDENIED`,
    /// `DXGI_ERROR_SESSION_DISCONNECTED`): back when the desktop is, however long that takes.
    Withheld,
    /// `DXGI_ERROR_UNSUPPORTED`: a composing-adapter change still taking effect, or for good on a
    /// machine whose driver cannot move composition.
    Unsupported,
}

/// A duplication start refused for one of the [`NotReady`] reasons.
#[derive(Debug)]
pub struct DuplicationNotReady {
    pub kind: NotReady,
    pub detail: String,
}

impl std::fmt::Display for DuplicationNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for DuplicationNotReady {}

fn not_ready(kind: NotReady, detail: String) -> anyhow::Error {
    anyhow::Error::new(DuplicationNotReady { kind, detail })
}

/// The [`NotReady`] reason a duplication call's code stands for, if any.
pub(crate) fn not_ready_code(code: HRESULT) -> Option<NotReady> {
    if code == E_ACCESSDENIED || code == DXGI_ERROR_SESSION_DISCONNECTED {
        Some(NotReady::Withheld)
    } else if code == DXGI_ERROR_UNSUPPORTED {
        Some(NotReady::Unsupported)
    } else if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
        || code == DXGI_ERROR_ACCESS_LOST
        || code == DXGI_ERROR_INVALID_CALL
    {
        Some(NotReady::Settling)
    } else {
        None
    }
}

/// Whether a start that no adapter could duplicate is only not ready. `tried` is each adapter
/// tried (packed LUID) with the [`NotReady`] reason its failure stands for, `None` for any other
/// failure (its device could not be made, or another code). When the composing adapter is among
/// them only its answer counts, since every other adapter refuses with `DXGI_ERROR_UNSUPPORTED`
/// whatever the state of the monitor; otherwise every one must be only not ready.
pub(crate) fn start_verdict(
    tried: &[(i64, Option<NotReady>)],
    composing: Option<i64>,
) -> Option<NotReady> {
    let decides = |luid: i64| match composing {
        Some(c) if tried.iter().any(|t| t.0 == c) => luid == c,
        _ => true,
    };
    let kinds = tried
        .iter()
        .filter(|t| decides(t.0))
        .map(|t| t.1)
        .collect::<Option<Vec<_>>>()?;
    if kinds.is_empty() {
        None
    } else if kinds.contains(&NotReady::Withheld) {
        Some(NotReady::Withheld)
    } else if kinds.iter().all(|k| *k == NotReady::Unsupported) {
        Some(NotReady::Unsupported)
    } else {
        Some(NotReady::Settling)
    }
}

/// Longest sleep between attempts while duplication is refused. It is also the longest a
/// session teardown can wait for the capture thread to notice `stop` during a lock screen,
/// because the sleep happens inside `poll`.
const DENIED_BACKOFF_MAX: Duration = Duration::from_millis(200);

/// Sleep before the next attempt in a run of refusals: 50, 100, 200, 200, ... ms.
fn denied_backoff(attempt: u32) -> Duration {
    let ms = 50u64 << attempt.saturating_sub(1).min(4);
    Duration::from_millis(ms).min(DENIED_BACKOFF_MAX)
}

/// How often a run of refusals says it is still going.
const DENIED_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// How long an unbroken run of [`RedupFailure::Unavailable`] answers is waited out before the
/// capture is ended with the code. A secure-desktop or mode transition that answers
/// `DXGI_ERROR_UNSUPPORTED` is over within a few seconds; an output whose composing adapter
/// moved answers it forever on this device.
const UNAVAILABLE_DEADLINE: Duration = Duration::from_secs(15);

/// A run of re-duplications refused with a [`RedupFailure::Denied`] or
/// [`RedupFailure::Unavailable`] code.
struct DeniedRun {
    since: Instant,
    attempts: u32,
    logged_at: Instant,
    /// Start of the current unbroken stretch of [`RedupFailure::Unavailable`] answers inside
    /// the run; cleared by a [`RedupFailure::Denied`] answer, which says the secure desktop is
    /// what is in the way.
    unavailable_since: Option<Instant>,
}

impl DeniedRun {
    /// Records one refusal of class `kind` at `now`; `true` when the run has been answered
    /// `Unavailable` without a break for longer than [`UNAVAILABLE_DEADLINE`].
    fn note(&mut self, kind: RedupFailure, now: Instant) -> bool {
        self.attempts += 1;
        if kind == RedupFailure::Unavailable {
            let since = *self.unavailable_since.get_or_insert(now);
            now.duration_since(since) > UNAVAILABLE_DEADLINE
        } else {
            self.unavailable_since = None;
            false
        }
    }
}

/// Something built on first use, whose failure is remembered as well as its success.
///
/// Without the `Failed` state a build that cannot succeed is attempted again on every frame
/// that needs it, on the capture thread (for the scRGB pass: a display-config query and a
/// `D3DCompile` per frame, and the same log line each time).
enum Lazy<T> {
    Untried,
    Ready(T),
    Failed,
}

impl<T> Lazy<T> {
    /// The built value, building it with `build` if this is the first ask. After a failed
    /// build `build` is never called again (until the value is reset to `Untried`).
    fn get_or_try(&mut self, build: impl FnOnce() -> Result<T>) -> Result<&T> {
        if matches!(self, Lazy::Untried) {
            match build() {
                Ok(v) => *self = Lazy::Ready(v),
                Err(e) => {
                    *self = Lazy::Failed;
                    return Err(e);
                }
            }
        }
        match self {
            Lazy::Ready(v) => Ok(v),
            _ => bail!("it could not be built earlier, and is not retried until re-duplication"),
        }
    }
}

/// The D3D device under the duplication is gone: a TDR, a driver update, a removed adapter.
///
/// Nothing built on it can be used again: not the duplication, not `desktop`, not the encoder
/// or ring the pipeline built on [`Duplicator::device`]. Recovery is a new `Duplicator` and a new
/// pipeline on top of it, which only the pipeline can do; this type is how it can tell the case
/// apart (`err.downcast_ref::<DeviceLost>()`) from a capture that cannot work at all.
#[derive(Debug)]
pub struct DeviceLost {
    pub during: &'static str,
    pub code: HRESULT,
    /// `GetDeviceRemovedReason`, `S_OK` when the device does not say.
    pub reason: HRESULT,
}

impl std::fmt::Display for DeviceLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the duplication device was lost during {} ({:#010x}; removed reason {:#010x})",
            self.during, self.code.0 as u32, self.reason.0 as u32
        )
    }
}

impl std::error::Error for DeviceLost {}

/// How one acquired duplication surface gets into `desktop`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceRoute {
    /// Same typeless family as `desktop`: `CopyResource`.
    Copy,
    /// A 10-bit UNORM surface: drawn through the plain quad shader, which samples it as 0..1 and
    /// writes BGRA8.
    Blit,
    /// scRGB FP16 (an HDR desktop): drawn through [`ScRgbPass`].
    ScRgb,
    /// Neither: the frame is skipped and counted.
    Unsupported,
}

/// `CopyResource` between formats outside one typeless family, or between textures of different
/// sizes, is invalid and D3D11 drops it with only a debug-layer message: `desktop` would keep its
/// old pixels while `poll` reports a fresh frame. This is the check that decides instead.
fn surface_route(format: DXGI_FORMAT, size: (u32, u32), desktop: (u32, u32)) -> SurfaceRoute {
    if size != desktop {
        SurfaceRoute::Unsupported
    } else if format == DXGI_FORMAT_B8G8R8A8_UNORM
        || format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        || format == DXGI_FORMAT_B8G8R8A8_TYPELESS
    {
        SurfaceRoute::Copy
    } else if format == DXGI_FORMAT_R10G10B10A2_UNORM {
        SurfaceRoute::Blit
    } else if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
        SurfaceRoute::ScRgb
    } else {
        SurfaceRoute::Unsupported
    }
}

pub enum PollStatus {
    Dirty,
    Timeout,
}

pub struct Duplicator {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    /// Which of the two duplication entry points is live, so `redup` keeps using the one that
    /// worked and the pipeline can report it.
    api: DuplicationApi,
    /// GDI name of the output, for the log and the SDR white-level lookup.
    device_name: String,
    /// `dxgi_sdr_bgra_only`: offer `DuplicateOutput1` only BGRA8 unless the output is in HDR.
    sdr_bgra_only: bool,
    /// Consecutive re-duplication failures that were not [`RedupFailure::Denied`].
    redup_failures: u32,
    /// The current run of refused re-duplications, if one is going on.
    denied: Option<DeniedRun>,
    /// Successful re-duplications since a frame was last *delivered*. A run of these with no
    /// delivered frame is a dead capture, however healthy each individual acquire looks.
    redup: RedupWatchdog,
    invalid_call_recoveries: u32,
    /// `LastPresentTime` of the most recent dirty frame, already on the shared host clock.
    /// The compositor's own frame time; without it the pipeline can only measure from the
    /// moment `poll` returned, which hides however long the frame waited to be handed over.
    last_present_host_ns: u64,
    /// Frames the OS accumulated while the previous one was being handled, summed since the
    /// last drain. `AccumulatedFrames` counts the frames accumulated since the last desktop
    /// image was processed, so everything past the first is a present the client never saw.
    accumulated_extra: u64,
    /// `ProtectedContentMaskedOut` is reported once per capture, not per frame: DRM content is
    /// a persistent property of what is on screen, and the symptom (a black rectangle where a
    /// video should be) otherwise has no explanation anywhere.
    protected_content_warned: bool,

    rotation: DXGI_MODE_ROTATION,
    raw_rotation: DXGI_MODE_ROTATION,
    phys_w: u32,
    phys_h: u32,
    width: u32,
    height: u32,
    origin_x: i32,
    origin_y: i32,

    desktop: ID3D11Texture2D,
    desktop_srv: ID3D11ShaderResourceView,
    /// Target for a duplication surface that cannot be `CopyResource`d into `desktop`.
    desktop_rtv: ID3D11RenderTargetView,
    /// Built on the first scRGB surface, and again after every re-duplication (the HDR state
    /// and the SDR white level can both change under a mode change). A failed build is not
    /// retried until then.
    scrgb: Lazy<ScRgbPass>,
    /// Surfaces that could not be brought into `desktop` and were skipped.
    format_skips: u64,
    /// Refused runs waited out (the secure desktop and its relatives), one per run.
    denied_waits: u64,
    /// One line per kind of conversion trouble, not one per frame.
    format_logged: u8,
    composite: ID3D11Texture2D,
    composite_rtv: ID3D11RenderTargetView,
    renderer: QuadRenderer,

    have_desktop: bool,
    /// Successful acquires that handed over a desktop surface, however the poll then classified
    /// them. The first one is what proves the duplication delivers.
    images_acquired: u64,
    sprite: Option<CursorSprite>,
    /// The pointer position exactly as DXGI reported it, in desktop coordinates. Compared
    /// against the next report to decide whether the pointer actually moved. Not `draw_pos`,
    /// which is a different coordinate system and would never compare equal.
    pointer_pos: POINT,
    /// Top-left of the cursor sprite in composite coordinates: output-relative, hotspot applied.
    draw_pos: POINT,
    cursor_visible: bool,
    /// Where the cursor was drawn into `composite` last time, so it can be erased without
    /// recopying the whole frame.
    drawn_rect: Option<(i32, i32, u32, u32)>,
    /// False until `composite` holds a full copy of `desktop`; the first composed frame and any
    /// frame after a rotation change has to be a full copy.
    composite_valid: bool,
    shape_buf: Vec<u8>,
    meta_buf: Vec<u8>,
    /// What `composite` is missing, accumulated over every poll since `frame()` last ran.
    pending: Pending,
    consecutive_skips: u32,
}

fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// One adapter's view of a display.
struct OutputCandidate {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    desc: DXGI_OUTPUT_DESC,
    label: String,
    software: bool,
    /// The adapter's LUID, packed as `AdapterInfo::luid`.
    luid: i64,
}

/// Put the adapter Windows composes the output on first: duplication is adapter-local, so that
/// is the one that can answer. A stable reorder; with nothing known the order is unchanged.
fn composing_first<T>(
    mut candidates: Vec<T>,
    luid: impl Fn(&T) -> i64,
    composing: Option<i64>,
) -> Vec<T> {
    if let Some(c) = composing {
        candidates.sort_by_key(|t| luid(t) != c);
    }
    candidates
}

/// The LUIDs (packed) of the adapters DXGI lists `device_name` under, in its order.
pub fn listing_adapters(device_name: &str) -> Vec<i64> {
    find_outputs(device_name)
        .map(|c| c.iter().map(|o| o.luid).collect())
        .unwrap_or_default()
}

/// Every adapter that claims `device_name`, in enumeration order.
///
/// On a hybrid-graphics laptop the same display is enumerated by more than one adapter, and only
/// one of them can duplicate it: the desktop for that output is composed on exactly one GPU, and
/// `DuplicateOutput` on any other returns `DXGI_ERROR_UNSUPPORTED` (`0x887A0004`) however it is
/// asked for. So the first match is not enough.
fn find_outputs(device_name: &str) -> Result<Vec<OutputCandidate>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut found = Vec::new();
    let mut seen = Vec::new();
    for a in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(ad) => ad,
            Err(_) => break,
        };
        for o in 0.. {
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(o) } {
                Ok(out) => out,
                Err(_) => break,
            };
            let desc = match unsafe { output.GetDesc() } {
                Ok(d) => d,
                Err(_) => continue,
            };
            let name = wide_to_string(&desc.DeviceName);
            if name != device_name {
                seen.push(name);
                continue;
            }
            let Ok(output1) = output.cast::<IDXGIOutput1>() else {
                continue;
            };
            // before IddCx 1.4 (the bundled driver declares 1.2) the OS composes an indirect
            // display on the POST adapter, or on WARP when that adapter is PnpStopped. `software`
            // is kept because a WARP-composed display is rendered on the CPU the encoder runs on
            let (label, software, luid) = match unsafe { adapter.GetDesc1() } {
                Ok(d) => {
                    let info = AdapterInfo::from_dxgi_desc1(&d);
                    (info.label(), info.software, info.luid)
                }
                Err(_) => ("an undescribed adapter".to_string(), false, 0),
            };
            found.push(OutputCandidate {
                adapter: adapter.clone(),
                output: output1,
                desc,
                label,
                software,
                luid,
            });
            break;
        }
    }
    if found.is_empty() {
        return Err(not_ready(
            NotReady::Settling,
            format!("no DXGI output named {device_name} (saw: {seen:?})"),
        ));
    }
    Ok(found)
}

fn create_device_on(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let try_flags =
        |flags: D3D11_CREATE_DEVICE_FLAG| -> Result<(ID3D11Device, ID3D11DeviceContext)> {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            unsafe {
                D3D11CreateDevice(
                    adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    flags,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            }
            .context("D3D11CreateDevice on duplication adapter")?;
            Ok((
                device.context("duplication device was null")?,
                context.context("duplication context was null")?,
            ))
        };

    let (device, context) = try_flags(
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    )
    .or_else(|e| {
        teprintln!("dxgi: device with VIDEO_SUPPORT failed ({e:?}); retrying without");
        try_flags(D3D11_CREATE_DEVICE_BGRA_SUPPORT)
    })?;

    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    Ok((device, context))
}

/// Formats `DuplicateOutput1` is allowed to hand back, most-wanted first.
///
/// The composited desktop is BGRA8; the two HDR formats are here because a display in HDR mode
/// has no BGRA8 surface to give and duplication would otherwise be refused outright.
///
/// Listing them on an SDR output is not free: `DuplicateOutput1` hands over a running fullscreen
/// application's original back buffer format, so a fullscreen 10-bit or FP16 game on an SDR
/// display arrives in that format instead of the BGRA8 DXGI would otherwise convert it to. See
/// [`dup_formats`].
const DUP1_FORMATS: [DXGI_FORMAT; 3] = [
    DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT,
];

/// The format list for one duplication attempt.
///
/// An output in HDR gets all three, because it has no BGRA8 surface. An SDR output gets BGRA8
/// alone when `sdr_bgra_only` is set (the `dxgi_sdr_bgra_only` switch, on by default), so DXGI
/// converts a fullscreen application's high-colour back buffer instead of handing it over raw.
/// Every display this app creates is SDR: the bundled driver is IddCx 1.2 and HDR on an indirect
/// display needs 1.10.
fn dup_formats(hdr: bool, sdr_bgra_only: bool) -> &'static [DXGI_FORMAT] {
    if hdr || !sdr_bgra_only {
        &DUP1_FORMATS
    } else {
        &DUP1_FORMATS[..1]
    }
}

/// The wider format list to retry `DuplicateOutput1` with after `formats` was refused with
/// `code`, if any: the full list, when only part of it was offered and the answer was
/// `DXGI_ERROR_UNSUPPORTED` (what an HDR output says to BGRA8 alone).
fn widened_formats(formats: &[DXGI_FORMAT], code: HRESULT) -> Option<&'static [DXGI_FORMAT]> {
    if code == DXGI_ERROR_UNSUPPORTED && formats.len() < DUP1_FORMATS.len() {
        Some(&DUP1_FORMATS)
    } else {
        None
    }
}

/// Whether the output is in HDR (advanced colour), by asking it.
///
/// `IDXGIOutput6` is probed by `QueryInterface`; a system without it has no HDR desktop either.
/// Asked again before every duplication, because HDR is toggled from Settings with the capture
/// running, and that arrives here as an ordinary `ACCESS_LOST`. The answer can be stale: an
/// output's description is cached by the factory that enumerated it, so a toggle after
/// `Duplicator::new` may still read SDR here. [`duplicate_once`] covers that by widening a
/// refused BGRA8-only list to all three formats ([`widened_formats`]).
fn output_is_hdr(output: &IDXGIOutput1) -> bool {
    output
        .cast::<IDXGIOutput6>()
        .ok()
        .and_then(|o| unsafe { o.GetDesc1() }.ok())
        .is_some_and(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
}

/// `SDRWhiteLevel` for the display named `device_name`, as the factor that puts SDR white at
/// scRGB 1.0 (`1000 / SDRWhiteLevel`; the level is in units of 80/1000 nits). Read-only.
fn sdr_white_scale(device_name: &str) -> Option<f32> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;

    let mut n_path = 0u32;
    let mut n_mode = 0u32;
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_path, &mut n_mode) }
        != ERROR_SUCCESS
    {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_path as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_mode as usize];
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut n_path,
            paths.as_mut_ptr(),
            &mut n_mode,
            modes.as_mut_ptr(),
            None,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    paths.truncate(n_path as usize);
    for path in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != ERROR_SUCCESS.0 as i32
            || wide_to_string(&source.viewGdiDeviceName) != device_name
        {
            continue;
        }
        let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            SDRWhiteLevel: 0,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } != ERROR_SUCCESS.0 as i32 {
            return None;
        }
        return white_level_scale(white.SDRWhiteLevel);
    }
    None
}

/// `1000 / SDRWhiteLevel`, or `None` for a level no display reports (below 80 nits).
fn white_level_scale(level: u32) -> Option<f32> {
    (level >= 1000).then(|| 1000.0 / level as f32)
}

/// Which duplication entry point produced the live duplication. Reported so a support log says
/// which one this machine needed rather than only that capture worked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DuplicationApi {
    /// `IDXGIOutput5::DuplicateOutput1`: Windows 10 1607+, and the only one that succeeds on
    /// several hybrid-graphics laptops and on any display in HDR mode.
    Output5,
    /// `IDXGIOutput1::DuplicateOutput`: the original, present since Windows 8.
    Output1,
}

impl DuplicationApi {
    pub fn name(self) -> &'static str {
        match self {
            DuplicationApi::Output5 => "DuplicateOutput1",
            DuplicationApi::Output1 => "DuplicateOutput",
        }
    }
}

/// One duplication attempt, newest API first.
///
/// `DuplicateOutput` returns `DXGI_ERROR_UNSUPPORTED` on the primary display of some
/// hybrid-graphics laptops (seen on an RTX 3080 Ti Laptop next to an Intel UHD iGPU), while
/// `DuplicateOutput1` with an explicit format list succeeds on the same output and device. It is
/// also the only one that can duplicate a display in HDR mode.
///
/// `IDXGIOutput5` is probed by `QueryInterface`, never by a build number.
fn duplicate_once(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
    formats: &[DXGI_FORMAT],
) -> std::result::Result<(IDXGIOutputDuplication, DuplicationApi), windows::core::Error> {
    let out5_err = match output.cast::<IDXGIOutput5>() {
        Ok(out5) => match unsafe { out5.DuplicateOutput1(device, 0, formats) } {
            Ok(dup) => return Ok((dup, DuplicationApi::Output5)),
            Err(e) => match widened_formats(formats, e.code()) {
                // BGRA8 alone was refused: the output may have gone HDR while `output_is_hdr`
                // still read the description its factory cached, so offer the full list once
                Some(wider) => match unsafe { out5.DuplicateOutput1(device, 0, wider) } {
                    Ok(dup) => return Ok((dup, DuplicationApi::Output5)),
                    Err(_) => Some(e),
                },
                None => Some(e),
            },
        },
        Err(e) => Some(e),
    };

    match unsafe { output.DuplicateOutput(device) } {
        Ok(dup) => Ok((dup, DuplicationApi::Output1)),
        // the newer call's failure: it had the better chance
        Err(e) => Err(out5_err.unwrap_or(e)),
    }
}

/// Whether the desktop on `device_name` is withheld from every capture for now: the secure
/// desktop (the lock screen, UAC, Ctrl+Alt+Del) or a disconnected session. Asked by attempting a
/// duplication, which the OS refuses with exactly the codes [`RedupFailure::Denied`] names in those
/// states. A duplication that succeeds, or fails any other way, is `false`. Blocking: it builds a
/// device per candidate adapter.
pub fn desktop_withheld(device_name: &str) -> bool {
    let Ok(candidates) = find_outputs(device_name) else {
        return false;
    };
    let sdr_bgra_only = super::tune::on("dxgi_sdr_bgra_only");
    let mut denied = false;
    for candidate in candidates {
        let Ok((device, _context)) = create_device_on(&candidate.adapter) else {
            continue;
        };
        let formats = dup_formats(output_is_hdr(&candidate.output), sdr_bgra_only);
        match duplicate_once(&candidate.output, &device, formats) {
            Ok(_) => return false,
            Err(e) => denied |= classify_redup_failure(e.code()) == RedupFailure::Denied,
        }
    }
    denied
}

fn duplicate_with_retry(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
    sdr_bgra_only: bool,
    attempts: u32,
) -> Result<(IDXGIOutputDuplication, DuplicationApi)> {
    let mut last: Option<windows::core::Error> = None;
    for i in 0..attempts {
        let formats = dup_formats(output_is_hdr(output), sdr_bgra_only);
        match duplicate_once(output, device, formats) {
            Ok(pair) => return Ok(pair),
            Err(e) => {
                last = Some(e);
                if i + 1 < attempts {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
    let detail = format!(
        "DuplicateOutput1 and DuplicateOutput both failed after {attempts} attempts: {last:?}"
    );
    match last.as_ref().and_then(|e| not_ready_code(e.code())) {
        Some(kind) => Err(not_ready(kind, detail)),
        None => Err(anyhow!(detail)),
    }
}

/// Answers "does DXGI duplication work on at least one desktop-attached output right now", by
/// trying it: `CreateDXGIFactory1` succeeds on every Windows since 7 and says nothing about
/// duplication. The duplication is created and immediately dropped, so nothing is held.
///
/// It is not, and at report time cannot be, an answer about the display a session will capture:
/// that display does not exist yet. On a hybrid laptop the panel can duplicate while an indirect
/// display composed elsewhere does not, so a success here means "some attached output", not
/// "the virtual display".
pub fn probe_duplication() -> Result<DuplicationApi> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let sdr_bgra_only = super::tune::on("dxgi_sdr_bgra_only");
    let mut last: Option<String> = None;
    let mut seen = 0u32;
    for a in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else {
            break;
        };
        for o in 0.. {
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(o) } {
                Ok(out) => out,
                Err(_) => break,
            };
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let Ok(output1) = output.cast::<IDXGIOutput1>() else {
                continue;
            };
            seen += 1;
            // recorded like a refused duplication: an adapter whose device cannot be created
            // must not hide another adapter's output that duplicates fine
            let device = match create_device_on(&adapter) {
                Ok((device, _context)) => device,
                Err(e) => {
                    last = Some(format!("{e:#}"));
                    continue;
                }
            };
            let formats = dup_formats(output_is_hdr(&output1), sdr_bgra_only);
            match duplicate_once(&output1, &device, formats) {
                Ok((dup, api)) => {
                    drop(dup);
                    return Ok(api);
                }
                Err(e) => last = Some(format!("{e:?}")),
            }
        }
    }
    if seen == 0 {
        bail!("no desktop-attached DXGI output to duplicate");
    }
    Err(anyhow!(
        "DXGI duplication is unsupported on all {seen} desktop outputs (last: {})",
        last.unwrap_or_default()
    ))
}

fn make_target(
    device: &ID3D11Device,
    w: u32,
    h: u32,
) -> Result<(
    ID3D11Texture2D,
    ID3D11ShaderResourceView,
    ID3D11RenderTargetView,
)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .context("CreateTexture2D (duplication target)")?;
    let tex = tex.context("duplication target texture was null")?;
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    unsafe { device.CreateShaderResourceView(&tex, None, Some(&mut srv)) }
        .context("CreateShaderResourceView (duplication target)")?;
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    unsafe { device.CreateRenderTargetView(&tex, None, Some(&mut rtv)) }
        .context("CreateRenderTargetView (duplication target)")?;
    Ok((
        tex,
        srv.context("duplication target SRV was null")?,
        rtv.context("duplication target RTV was null")?,
    ))
}

fn blit_verts(rotation: DXGI_MODE_ROTATION) -> cursor::QuadVerts {
    let uvs: [[f32; 2]; 4] = match rotation {
        DXGI_MODE_ROTATION_ROTATE90 => [[0.0, 1.0], [0.0, 0.0], [1.0, 1.0], [1.0, 0.0]],
        DXGI_MODE_ROTATION_ROTATE180 => [[1.0, 1.0], [0.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        DXGI_MODE_ROTATION_ROTATE270 => [[1.0, 0.0], [1.0, 1.0], [0.0, 0.0], [0.0, 1.0]],
        _ => [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
    };
    [
        [-1.0, 1.0, uvs[0][0], uvs[0][1]],
        [1.0, 1.0, uvs[1][0], uvs[1][1]],
        [-1.0, -1.0, uvs[2][0], uvs[2][1]],
        [1.0, -1.0, uvs[3][0], uvs[3][1]],
    ]
}

impl Duplicator {
    pub fn new(device_name: &str, logical_w: u32, logical_h: u32) -> Result<Self> {
        // the driver says which adapter composes a display this app created. On a hybrid laptop
        // DXGI lists it under the NVIDIA adapter only, and duplication there opens but never
        // delivers a frame (ACCESS_LOST on every acquire) unless that adapter also composes it
        let composing =
            match crate::windows_utils::virtual_display::composing_luid_of_name(device_name) {
                Some(Ok(l)) => Some(l.packed()),
                _ => None,
            };
        let candidates = composing_first(find_outputs(device_name)?, |c| c.luid, composing);
        if let Some(c) = composing {
            if !candidates.iter().any(|o| o.luid == c) {
                teprintln!(
                    "dxgi: {device_name} is composed on adapter LUID 0x{c:X}, which does not list \
                     it; duplication on {} is expected to deliver nothing. Composing it on that \
                     adapter (SE_TUNE_ON=render_adapter_follows_encoder with an encoder there) \
                     is what makes it work",
                    candidates
                        .iter()
                        .map(|o| o.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        let sdr_bgra_only = super::tune::on("dxgi_sdr_bgra_only");
        let mut errors = Vec::new();
        let mut tried: Vec<(i64, Option<NotReady>)> = Vec::new();
        let mut chosen: Option<(OutputCandidate, ID3D11Device, ID3D11DeviceContext)> = None;
        for candidate in candidates {
            let (device, context) = match create_device_on(&candidate.adapter) {
                Ok(pair) => pair,
                Err(e) => {
                    errors.push(format!("{}: {e:#}", candidate.label));
                    tried.push((candidate.luid, None));
                    continue;
                }
            };
            // one attempt each: only the adapter that composes this desktop can duplicate it,
            // every other returns DXGI_ERROR_UNSUPPORTED and a retry cannot change that. The
            // retry loop below is for the adapter that does answer
            let formats = dup_formats(output_is_hdr(&candidate.output), sdr_bgra_only);
            match duplicate_once(&candidate.output, &device, formats) {
                Ok((dup, _)) => {
                    drop(dup);
                    if candidate.software {
                        teprintln!(
                            "dxgi: output {device_name} is composed by a SOFTWARE rasteriser -- \
                             {}. Expect very high latency and CPU use; the POST adapter is \
                             probably stopped",
                            candidate.label
                        );
                    }
                    chosen = Some((candidate, device, context));
                    break;
                }
                Err(e) => {
                    errors.push(format!("{}: {e:?}", candidate.label));
                    tried.push((candidate.luid, not_ready_code(e.code())));
                }
            }
        }
        let Some((candidate, device, context)) = chosen else {
            let detail = format!(
                "no adapter can duplicate {device_name} ({})",
                errors.join("; ")
            );
            return Err(match start_verdict(&tried, composing) {
                Some(kind) => not_ready(kind, detail),
                None => anyhow!(detail),
            });
        };
        let OutputCandidate {
            output,
            desc: out_desc,
            label: adapter_label,
            ..
        } = candidate;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let (dup, api, phys_w, phys_h, raw_rotation, rotation) = loop {
            let (dup, api) = duplicate_with_retry(&output, &device, sdr_bgra_only, 5)?;
            let dd: DXGI_OUTDUPL_DESC = unsafe { dup.GetDesc() };
            let (w, h) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
            let rotated = matches!(
                dd.Rotation,
                DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270
            );
            if rotated && (h, w) == (logical_w, logical_h) {
                break (dup, api, w, h, dd.Rotation, dd.Rotation);
            }
            if (w, h) == (logical_w, logical_h) {
                let effective = if rotated {
                    tprintln!(
                        "dxgi: output flags rotation {:?} but the duplication surface is already \
                         desktop-oriented ({w}x{h}); compositing without an un-rotate pass",
                        dd.Rotation
                    );
                    DXGI_MODE_ROTATION_IDENTITY
                } else {
                    dd.Rotation
                };
                break (dup, api, w, h, dd.Rotation, effective);
            }
            if std::time::Instant::now() >= deadline {
                return Err(not_ready(
                    NotReady::Settling,
                    format!(
                        "duplication mode {w}x{h} (rotation {:?}) does not match expected \
                         desktop {logical_w}x{logical_h} (mode change did not settle)",
                        dd.Rotation
                    ),
                ));
            }
            drop(dup);
            std::thread::sleep(std::time::Duration::from_millis(100));
        };

        let (desktop, desktop_srv, desktop_rtv) = make_target(&device, phys_w, phys_h)?;
        let (composite, _, composite_rtv) = make_target(&device, logical_w, logical_h)?;
        let renderer = QuadRenderer::new(&device, &context)?;

        tprintln!(
            "dxgi: duplication ready on {device_name} via {} on {adapter_label}: \
             {phys_w}x{phys_h}, rotation={rotation:?}, logical={logical_w}x{logical_h}",
            api.name()
        );

        Ok(Self {
            device,
            context,
            output,
            dup: Some(dup),
            api,
            device_name: device_name.to_string(),
            sdr_bgra_only,
            redup_failures: 0,
            denied: None,
            redup: RedupWatchdog::default(),
            invalid_call_recoveries: 0,
            last_present_host_ns: 0,
            accumulated_extra: 0,
            protected_content_warned: false,
            rotation,
            raw_rotation,
            phys_w,
            phys_h,
            width: logical_w,
            height: logical_h,
            origin_x: out_desc.DesktopCoordinates.left,
            origin_y: out_desc.DesktopCoordinates.top,
            desktop,
            desktop_srv,
            desktop_rtv,
            scrgb: Lazy::Untried,
            format_skips: 0,
            denied_waits: 0,
            format_logged: 0,
            composite,
            composite_rtv,
            renderer,
            have_desktop: false,
            images_acquired: 0,
            sprite: None,
            pointer_pos: POINT::default(),
            draw_pos: POINT::default(),
            cursor_visible: false,
            drawn_rect: None,
            composite_valid: false,
            shape_buf: Vec::new(),
            meta_buf: Vec::new(),
            pending: Pending::Full,
            consecutive_skips: 0,
        })
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// Which duplication entry point is live.
    pub fn api(&self) -> DuplicationApi {
        self.api
    }

    fn redup(&mut self) -> Result<()> {
        let formats = dup_formats(output_is_hdr(&self.output), self.sdr_bgra_only);
        match duplicate_once(&self.output, &self.device, formats) {
            Ok((dup, api)) => {
                self.api = api;
                let dd: DXGI_OUTDUPL_DESC = unsafe { dup.GetDesc() };
                if (dd.ModeDesc.Width, dd.ModeDesc.Height) != (self.phys_w, self.phys_h)
                    || dd.Rotation != self.raw_rotation
                {
                    bail!(
                        "display mode changed under duplication ({}x{} rot {:?} -> {}x{} rot {:?}); \
                         capture must restart",
                        self.phys_w, self.phys_h, self.raw_rotation,
                        dd.ModeDesc.Width, dd.ModeDesc.Height, dd.Rotation,
                    );
                }
                self.dup = Some(dup);
                self.redup_failures = 0;
                if let Some(run) = self.denied.take() {
                    tprintln!(
                        "dxgi: duplication of {} is back after {:.1} s refused ({} attempts)",
                        self.device_name,
                        run.since.elapsed().as_secs_f64(),
                        run.attempts
                    );
                }
                // nothing says what changed while there was no duplication, and the HDR state
                // or the white level may have changed with it
                self.pending = Pending::Full;
                self.scrgb = Lazy::Untried;
                self.redup.on_reestablished()?;
                if self.redup.should_log() {
                    tprintln!(
                        "dxgi: duplication re-established ({} since the last delivered frame)",
                        self.redup.count()
                    );
                }
                Ok(())
            }
            Err(e) => match classify_redup_failure(e.code()) {
                // waited out for as long as the session lasts: a UAC prompt or a lock screen has
                // no bound, and the pipeline checks `stop` between polls, so a teardown still ends
                // this within one backoff. `Unavailable` only up to `UNAVAILABLE_DEADLINE`
                kind @ (RedupFailure::Denied | RedupFailure::Unavailable) => {
                    let now = Instant::now();
                    if self.denied.is_none() {
                        self.denied_waits += 1;
                    }
                    let run = self.denied.get_or_insert_with(|| {
                        teprintln!(
                            "dxgi: duplication of {} refused ({e:?}); the secure desktop (UAC, \
                             Ctrl+Alt+Del, the lock screen) or a desktop or session switch is \
                             up. Waiting for it to end; the client keeps the last picture",
                            self.device_name
                        );
                        DeniedRun {
                            since: now,
                            attempts: 0,
                            logged_at: now,
                            unavailable_since: None,
                        }
                    });
                    if run.note(kind, now) {
                        let (secs, attempts) =
                            (now.duration_since(run.since).as_secs_f64(), run.attempts);
                        self.denied = None;
                        bail!(
                            "re-duplication of {} refused for {:.0} s ({} attempts), the last \
                             {} s without a break with {e:?}; the output is not coming back on \
                             this device, capture must restart",
                            self.device_name,
                            secs,
                            attempts,
                            UNAVAILABLE_DEADLINE.as_secs()
                        );
                    }
                    if now.duration_since(run.logged_at) >= DENIED_LOG_INTERVAL {
                        run.logged_at = now;
                        teprintln!(
                            "dxgi: duplication still refused after {:.0} s ({} attempts, {e:?})",
                            now.duration_since(run.since).as_secs_f64(),
                            run.attempts
                        );
                    }
                    let wait = denied_backoff(run.attempts);
                    std::thread::sleep(wait);
                    Ok(())
                }
                RedupFailure::DeviceLost => {
                    Err(self.device_lost("re-duplication", e.code()).into())
                }
                RedupFailure::Other => {
                    self.redup_failures += 1;
                    if self.redup_failures > REDUP_MAX_RETRIES {
                        bail!(
                            "re-duplication failed {} times, giving up: {e:?}",
                            self.redup_failures
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    Ok(())
                }
            },
        }
    }

    fn device_lost(&self, during: &'static str, code: HRESULT) -> DeviceLost {
        let reason = match unsafe { self.device.GetDeviceRemovedReason() } {
            Ok(()) => HRESULT(0),
            Err(e) => e.code(),
        };
        DeviceLost {
            during,
            code,
            reason,
        }
    }

    /// Reads this frame's change metadata.
    ///
    /// [`FrameChange::Empty`] only when the driver said, positively, that nothing in the desktop
    /// image changed: the case worth skipping the copy and the encode for. Anything ambiguous
    /// (no metadata, coalesced frames, a failed query) is [`FrameChange::Unknown`], which is the
    /// safe direction. The region feeds [`Pending`], so that moving a cursor over a static
    /// desktop does not cost a full-frame copy in `frame()`.
    fn collect_change(
        &mut self,
        dup: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> FrameChange {
        // >1: the OS coalesced updates while the previous frame was held, so the metadata
        // describes more than one present and is not an exact region for this one
        if info.AccumulatedFrames > 1 || info.TotalMetadataBufferSize == 0 {
            return FrameChange::Unknown;
        }
        let needed = info.TotalMetadataBufferSize as usize;
        if self.meta_buf.len() < needed {
            self.meta_buf.resize(needed, 0);
        }

        let mut move_bytes = 0u32;
        let move_ok = unsafe {
            dup.GetFrameMoveRects(
                self.meta_buf.len() as u32,
                self.meta_buf.as_mut_ptr() as *mut DXGI_OUTDUPL_MOVE_RECT,
                &mut move_bytes,
            )
        };
        if move_ok.is_err() {
            return FrameChange::Unknown;
        }
        let move_count = move_bytes as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        let mut bounds: Option<RECT> = None;
        if move_count > 0 {
            let moves = unsafe {
                std::slice::from_raw_parts(
                    self.meta_buf.as_ptr() as *const DXGI_OUTDUPL_MOVE_RECT,
                    move_count,
                )
            };
            // a move touches both the rectangle it reads from and the one it writes to
            for m in moves {
                let w = m.DestinationRect.right - m.DestinationRect.left;
                let h = m.DestinationRect.bottom - m.DestinationRect.top;
                union_rect(&mut bounds, m.DestinationRect);
                union_rect(
                    &mut bounds,
                    RECT {
                        left: m.SourcePoint.x,
                        top: m.SourcePoint.y,
                        right: m.SourcePoint.x + w,
                        bottom: m.SourcePoint.y + h,
                    },
                );
            }
        }

        let mut dirty_bytes = 0u32;
        let dirty_ok = unsafe {
            dup.GetFrameDirtyRects(
                self.meta_buf.len() as u32,
                self.meta_buf.as_mut_ptr() as *mut RECT,
                &mut dirty_bytes,
            )
        };
        if dirty_ok.is_err() {
            return FrameChange::Unknown;
        }
        let dirty_count = dirty_bytes as usize / std::mem::size_of::<RECT>();
        if dirty_count > 0 {
            let dirty = unsafe {
                std::slice::from_raw_parts(self.meta_buf.as_ptr() as *const RECT, dirty_count)
            };
            for r in dirty {
                union_rect(&mut bounds, *r);
            }
        }

        match bounds {
            Some(r) => FrameChange::Region(r),
            // metadata was present and both lists were empty: rects with no area still count
            // as reported changes above, so this is only reached on a positive "nothing"
            None => FrameChange::Empty,
        }
    }

    /// Host-clock nanoseconds of the last present, or 0 if the driver has not reported one.
    pub fn last_present_host_ns(&self) -> u64 {
        self.last_present_host_ns
    }

    /// Take the frames the OS coalesced away since the last call, and reset the count.
    pub fn take_accumulated_drops(&mut self) -> u64 {
        std::mem::take(&mut self.accumulated_extra)
    }

    /// Desktop surfaces handed over since the capture began, counted at the acquire.
    pub fn images_acquired(&self) -> u64 {
        self.images_acquired
    }

    /// Surfaces skipped since the capture began because they could not be brought into
    /// `desktop` (an unexpected format or size, or a failed conversion).
    pub fn format_skips(&self) -> u64 {
        self.format_skips
    }

    /// Refused runs this duplicator has waited out or is waiting out.
    pub fn denied_waits(&self) -> u64 {
        self.denied_waits
    }

    /// Says one thing about format trouble once: `bit` picks which thing.
    fn log_format_once(&mut self, bit: u8, msg: impl FnOnce() -> String) {
        if self.format_logged & bit == 0 {
            self.format_logged |= bit;
            teprintln!("dxgi: {}", msg());
        }
    }

    /// Brings one acquired duplication surface into `desktop`. False when it could not, in
    /// which case `desktop` is unchanged and the frame is not reported.
    fn ingest(&mut self, tex: &ID3D11Texture2D) -> bool {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let route = surface_route(
            desc.Format,
            (desc.Width, desc.Height),
            (self.phys_w, self.phys_h),
        );
        if route == SurfaceRoute::Copy {
            unsafe { self.context.CopyResource(&self.desktop, tex) };
            return true;
        }
        let converted = match route {
            SurfaceRoute::Blit | SurfaceRoute::ScRgb => self.convert(tex, route),
            _ => Err(anyhow!(
                "duplication surface is {:?} {}x{}, desktop copy is BGRA8 {}x{}",
                desc.Format,
                desc.Width,
                desc.Height,
                self.phys_w,
                self.phys_h
            )),
        };
        match converted {
            Ok(()) => {
                let format = desc.Format;
                self.log_format_once(1, || {
                    format!(
                        "the duplication surface is {format:?}, not BGRA8; converting it on the \
                         GPU (HDR content is clipped to SDR white)"
                    )
                });
                true
            }
            Err(e) => {
                self.format_skips += 1;
                self.log_format_once(2, || {
                    format!("skipping duplication frames that cannot be converted: {e:#}")
                });
                false
            }
        }
    }

    /// Draws a non-BGRA surface into `desktop` through a shader.
    fn convert(&mut self, tex: &ID3D11Texture2D, route: SurfaceRoute) -> Result<()> {
        // per frame, and only on this path: a kept view would hold the duplication's surface
        // alive past `ReleaseFrame`
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            self.device
                .CreateShaderResourceView(tex, None, Some(&mut srv))
        }
        .context("CreateShaderResourceView on the duplication surface")?;
        let srv = srv.context("duplication surface SRV was null")?;
        let verts = blit_verts(DXGI_MODE_ROTATION_IDENTITY);
        if route == SurfaceRoute::ScRgb {
            let (device, device_name) = (&self.device, &self.device_name);
            let pass = self
                .scrgb
                .get_or_try(|| {
                    let scale = sdr_white_scale(device_name);
                    tprintln!(
                        "dxgi: {} is in HDR; converting scRGB with SDR white at {}",
                        device_name,
                        match scale {
                            Some(s) => format!("{:.0} nits", 80.0 / s),
                            None => {
                                "80 nits (the display's SDR white level could not be read)".into()
                            }
                        }
                    );
                    ScRgbPass::new(device, scale.unwrap_or(1.0))
                })
                .context("the scRGB conversion pass")?;
            pass.draw(
                &self.renderer,
                &self.desktop_rtv,
                &srv,
                &verts,
                self.phys_w,
                self.phys_h,
            )
        } else {
            self.renderer.draw(
                &self.desktop_rtv,
                &srv,
                BlendKind::Opaque,
                &verts,
                self.phys_w,
                self.phys_h,
            )
        }
    }

    pub fn poll(&mut self, timeout_ms: u32) -> Result<PollStatus> {
        let Some(dup) = self.dup.clone() else {
            self.redup()?;
            return Ok(PollStatus::Timeout);
        };

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {
                if resource.is_some() {
                    self.images_acquired += 1;
                }
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(PollStatus::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.dup = None;
                return Ok(PollStatus::Timeout);
            }
            // recoverable by re-duplicating. A freshly created virtual display does it reliably:
            // the IDD is still attaching, `ACCESS_LOST` fires repeatedly, and the next acquire
            // is invalid
            Err(e) if e.code() == DXGI_ERROR_INVALID_CALL => {
                self.invalid_call_recoveries += 1;
                if self.invalid_call_recoveries > INVALID_CALL_MAX_RECOVERIES {
                    return Err(anyhow!(
                        "AcquireNextFrame kept returning DXGI_ERROR_INVALID_CALL after \
                         {INVALID_CALL_MAX_RECOVERIES} re-duplications: {e:?}"
                    ));
                }
                teprintln!(
                    "dxgi: AcquireNextFrame returned DXGI_ERROR_INVALID_CALL \
                     (recovery {}/{INVALID_CALL_MAX_RECOVERIES}); re-duplicating",
                    self.invalid_call_recoveries
                );
                self.dup = None;
                return Ok(PollStatus::Timeout);
            }
            Err(e) if is_device_lost(e.code()) => {
                return Err(self.device_lost("AcquireNextFrame", e.code()).into());
            }
            Err(e) => return Err(anyhow!("AcquireNextFrame: {e:?}")),
        }
        // everything past here is inside the acquire; the guard releases on every exit path
        let _acquired = AcquiredFrame(&dup);
        // `invalid_call_recoveries` counts DXGI_ERROR_INVALID_CALL, and a returning acquire
        // proves that pairing recovered. `redup` is reset only at delivery (`RedupWatchdog`)
        self.invalid_call_recoveries = 0;

        self.accumulated_extra += u64::from(info.AccumulatedFrames.saturating_sub(1));
        if info.ProtectedContentMaskedOut.as_bool() && !self.protected_content_warned {
            self.protected_content_warned = true;
            teprintln!(
                "dxgi: the desktop image has protected content masked out: DRM-protected                  video will reach the client as a black rectangle"
            );
        }

        let mut dirty = false;
        // this poll put a new image into `desktop`, as opposed to only a pointer change
        let mut desktop_updated = false;
        let mut pointer_changed = false;

        if info.LastPresentTime != 0 {
            self.last_present_host_ns =
                super::clock::dxgi_present_time_to_host_ns(info.LastPresentTime);
            if let Some(res) = &resource {
                const MAX_CONSECUTIVE_SKIPS: u32 = 5;
                let change = self.collect_change(&dup, &info);
                let skip =
                    change == FrameChange::Empty && self.consecutive_skips < MAX_CONSECUTIVE_SKIPS;
                if !skip {
                    let tex: ID3D11Texture2D = res
                        .cast()
                        .context("duplication resource as ID3D11Texture2D")?;
                    if self.ingest(&tex) {
                        self.have_desktop = true;
                        dirty = true;
                        desktop_updated = true;
                        // a forced copy exists because "nothing changed" reports are not trusted
                        // forever, so this one is not trusted either
                        self.pending.note(match change {
                            FrameChange::Empty => FrameChange::Unknown,
                            other => other,
                        });
                    }
                    self.consecutive_skips = 0;
                } else {
                    self.consecutive_skips += 1;
                }
            }
        }

        if info.PointerShapeBufferSize > 0 {
            self.shape_buf
                .resize(info.PointerShapeBufferSize as usize, 0);
            let mut required = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let got = unsafe {
                dup.GetFramePointerShape(
                    self.shape_buf.len() as u32,
                    self.shape_buf.as_mut_ptr() as *mut core::ffi::c_void,
                    &mut required,
                    &mut shape,
                )
            };
            match got {
                Ok(()) => match build_sprite(&self.device, &shape, &self.shape_buf) {
                    Ok(sprite) => {
                        self.sprite = Some(sprite);
                        if self.cursor_visible {
                            dirty = true;
                            pointer_changed = true;
                        }
                    }
                    Err(e) => teprintln!("dxgi: cursor shape conversion failed: {e:?}"),
                },
                Err(e) => teprintln!("dxgi: GetFramePointerShape failed: {e:?}"),
            }
        }

        if info.LastMouseUpdateTime != 0 {
            let visible = info.PointerPosition.Visible.as_bool();
            let pos = info.PointerPosition.Position;
            if visible != self.cursor_visible {
                pointer_changed = true;
            }
            if visible && (pos.x != self.pointer_pos.x || pos.y != self.pointer_pos.y) {
                pointer_changed = true;
            }
            dirty |= pointer_changed;
            self.cursor_visible = visible;
            if visible {
                self.pointer_pos = pos;
            }
        }

        // a frame that is new only because the pointer moved is as old as the pointer move, not
        // the last desktop present (seconds earlier over a static desktop). `LastMouseUpdateTime`
        // is raw QPC like `LastPresentTime`; `max` because an earlier poll in this frame period
        // may already hold a newer desktop present
        if pointer_changed && !desktop_updated && info.LastMouseUpdateTime != 0 {
            let mouse_ns = super::clock::dxgi_present_time_to_host_ns(info.LastMouseUpdateTime);
            self.last_present_host_ns = self.last_present_host_ns.max(mouse_ns);
        }

        Ok(if dirty && self.have_desktop {
            // the one event that proves this output can be captured
            self.redup.on_frame_delivered();
            PollStatus::Dirty
        } else {
            PollStatus::Timeout
        })
    }

    fn refresh_cursor_state(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};

        let Some(sprite) = self.sprite.as_ref() else {
            return;
        };
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut ci) }.is_err() {
            return;
        }
        if (ci.flags.0 & CURSOR_SHOWING.0) == 0 {
            self.cursor_visible = false;
            return;
        }
        let on_output = ci.ptScreenPos.x >= self.origin_x
            && ci.ptScreenPos.x < self.origin_x + self.width as i32
            && ci.ptScreenPos.y >= self.origin_y
            && ci.ptScreenPos.y < self.origin_y + self.height as i32;
        self.cursor_visible = on_output;
        if on_output {
            // a different coordinate system from `pointer_pos`: output-relative and with the
            // hotspot subtracted, i.e. the sprite's top-left corner in composite space
            self.draw_pos = POINT {
                x: ci.ptScreenPos.x - self.origin_x - sprite.hotspot_x,
                y: ci.ptScreenPos.y - self.origin_y - sprite.hotspot_y,
            };
        }
    }

    pub fn frame(&mut self) -> Result<&ID3D11Texture2D> {
        if !self.have_desktop {
            bail!("no desktop image acquired yet");
        }
        self.refresh_cursor_state();
        let identity = !matches!(
            self.rotation,
            DXGI_MODE_ROTATION_ROTATE90
                | DXGI_MODE_ROTATION_ROTATE180
                | DXGI_MODE_ROTATION_ROTATE270
        );
        let cursor_on = self.cursor_visible && self.sprite.is_some();

        // whatever happens below, every change noted so far is accounted for by this frame
        let pending = std::mem::take(&mut self.pending);

        if identity && !cursor_on {
            // nothing to compose: the encoder reads the duplication copy directly, which leaves
            // the composite stale, so the next composed frame must be a full copy
            self.composite_valid = false;
            self.drawn_rect = None;
            return Ok(&self.desktop);
        }

        if identity {
            // `composite` already holds the previous frame: refreshing only what changed plus
            // the rectangle the last cursor was drawn over makes a cursor move two small copies
            // instead of a full-resolution one (8.3 MB at 1080p, 14.7 MB at 1440p)
            match plan_refresh(pending, self.composite_valid, self.width, self.height) {
                Refresh::Partial(changed) => {
                    if let Some(r) = changed {
                        self.copy_region_from_desktop(r);
                    }
                    if let Some((x, y, w, h)) = self.drawn_rect {
                        self.copy_region_from_desktop(RECT {
                            left: x,
                            top: y,
                            right: x + w as i32,
                            bottom: y + h as i32,
                        });
                    }
                }
                Refresh::Whole => unsafe {
                    self.context.CopyResource(&self.composite, &self.desktop)
                },
            }
            self.composite_valid = true;
        } else {
            self.renderer.draw(
                &self.composite_rtv,
                &self.desktop_srv,
                BlendKind::Opaque,
                &blit_verts(self.rotation),
                self.width,
                self.height,
            )?;
            self.composite_valid = true;
        }
        self.drawn_rect = None;

        if cursor_on {
            let sprite = self
                .sprite
                .as_ref()
                .ok_or_else(|| anyhow!("cursor sprite vanished mid-compose"))?;
            let verts = quad_verts(
                self.draw_pos.x,
                self.draw_pos.y,
                sprite.width,
                sprite.height,
                self.width,
                self.height,
            );
            for (srv, blend) in &sprite.passes {
                self.renderer.draw(
                    &self.composite_rtv,
                    srv,
                    *blend,
                    &verts,
                    self.width,
                    self.height,
                )?;
            }
            self.drawn_rect = Some((
                self.draw_pos.x,
                self.draw_pos.y,
                sprite.width,
                sprite.height,
            ));
        }

        Ok(&self.composite)
    }

    /// Copies one clipped rectangle of `desktop` into the same place in `composite`.
    fn copy_region_from_desktop(&self, r: RECT) {
        use windows::Win32::Graphics::Direct3D11::D3D11_BOX;

        let left = r.left.clamp(0, self.width as i32);
        let top = r.top.clamp(0, self.height as i32);
        let right = r.right.clamp(0, self.width as i32);
        let bottom = r.bottom.clamp(0, self.height as i32);
        if right <= left || bottom <= top {
            return;
        }
        let src = D3D11_BOX {
            left: left as u32,
            top: top as u32,
            front: 0,
            right: right as u32,
            bottom: bottom as u32,
            back: 1,
        };
        unsafe {
            self.context.CopySubresourceRegion(
                &self.composite,
                0,
                left as u32,
                top as u32,
                0,
                &self.desktop,
                0,
                Some(&src),
            );
        }
    }

    pub fn cursor_drawn(&self) -> bool {
        self.cursor_visible && self.sprite.is_some()
    }
}

pub fn probe_to_bmp(requested_monitor: u32, path: &str) -> Result<()> {
    let (monitor, info) = super::capture::select_monitor(requested_monitor)?;
    let device_name = monitor
        .device_name()
        .map_err(|e| anyhow!("monitor device name: {e}"))?;
    tprintln!(
        "dxgi probe: monitor[{}] '{}' ({device_name}) {}x{}",
        info.index,
        info.name,
        info.width,
        info.height
    );

    let mut dup = Duplicator::new(&device_name, info.width, info.height)?;
    let mut reader =
        super::scaler::TextureReader::new(dup.device(), dup.context(), info.width, info.height)?;

    // no pointer nudge to provoke a frame: `SetCursorPos` is injected input on whatever machine
    // runs the probe, and a fresh duplication's first acquire carries the whole desktop anyway
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut frames = 0u32;
    while std::time::Instant::now() < deadline {
        if let PollStatus::Dirty = dup.poll(100)? {
            frames += 1;
            if frames >= 3 && dup.cursor_drawn() {
                break;
            }
        }
    }
    if frames == 0 {
        bail!("no desktop frame within 3s (no desktop access, or duplication denied?)");
    }

    let cursor_drawn = dup.cursor_drawn();
    let (cx, cy) = (dup.draw_pos.x, dup.draw_pos.y);
    let (cw, ch) = dup
        .sprite
        .as_ref()
        .map(|s| (s.width, s.height))
        .unwrap_or((0, 0));
    let tex = dup.frame()?;
    let (data, pitch) = reader.read_back(tex)?;
    write_bmp(path, info.width, info.height, data, pitch)?;
    tprintln!(
        "dxgi probe: wrote {}x{} (updates={frames}, cursor_drawn={cursor_drawn}, \
         cursor_pos={cx},{cy}, cursor_size={cw}x{ch}) -> {path}",
        info.width,
        info.height
    );
    Ok(())
}

fn write_bmp(path: &str, w: u32, h: u32, data: &[u8], pitch: u32) -> Result<()> {
    use std::io::Write as _;

    let row = (w as usize) * 4;
    let img_size = row * h as usize;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);

    out.write_all(b"BM")?;
    out.write_all(&(54u32 + img_size as u32).to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;
    out.write_all(&54u32.to_le_bytes())?;
    out.write_all(&40u32.to_le_bytes())?;
    out.write_all(&(w as i32).to_le_bytes())?;
    out.write_all(&(h as i32).to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?;
    out.write_all(&32u16.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;
    out.write_all(&(img_size as u32).to_le_bytes())?;
    out.write_all(&[0u8; 16])?;

    for y in (0..h as usize).rev() {
        let start = y * pitch as usize;
        out.write_all(&data[start..start + row])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_composing_adapter_is_tried_first() {
        let order = |composing| super::composing_first(vec![1i64, 2, 3], |l| *l, composing);
        assert_eq!(order(Some(3)), vec![3, 1, 2]);
        assert_eq!(order(Some(1)), vec![1, 2, 3]);
        assert_eq!(order(Some(9)), vec![1, 2, 3]);
        assert_eq!(order(None), vec![1, 2, 3]);
    }

    use super::*;

    #[test]
    fn a_starving_duplication_gives_up_instead_of_spinning() {
        let mut w = RedupWatchdog::default();
        // every acquire returns and none yields a frame, so nothing calls `on_frame_delivered`
        for i in 1..=REDUP_WITHOUT_FRAME_MAX {
            assert!(
                w.on_reestablished().is_ok(),
                "re-establishment {i} should still be within the budget"
            );
        }
        let err = w
            .on_reestablished()
            .expect_err("the re-establishment past the ceiling must fail the capture");
        let msg = format!("{err}");
        assert!(msg.contains("without ever delivering a"), "{msg}");
        assert!(
            msg.contains(&(REDUP_WITHOUT_FRAME_MAX + 1).to_string()),
            "{msg}"
        );
    }

    #[test]
    fn a_delivered_frame_is_what_clears_the_run() {
        let mut w = RedupWatchdog::default();
        for _ in 0..REDUP_WITHOUT_FRAME_MAX {
            w.on_reestablished().unwrap();
        }
        assert_eq!(w.count(), REDUP_WITHOUT_FRAME_MAX);
        w.on_frame_delivered();
        assert_eq!(w.count(), 0);
        // the budget is whole again, which is what lets an intermittent output survive
        for _ in 0..REDUP_WITHOUT_FRAME_MAX {
            w.on_reestablished().unwrap();
        }
        assert!(w.on_reestablished().is_err());
    }

    #[test]
    fn the_log_cadence_is_first_then_every_tenth() {
        let mut w = RedupWatchdog::default();
        let mut logged = Vec::new();
        for _ in 0..REDUP_WITHOUT_FRAME_MAX {
            w.on_reestablished().unwrap();
            if w.should_log() {
                logged.push(w.count());
            }
        }
        assert_eq!(logged, vec![1, 10, 20, 30, 40]);
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Replays a sequence of polls the way `poll` feeds them in, then asks what `frame()` copies.
    fn refresh_after(changes: &[FrameChange]) -> Refresh {
        let mut pending = Pending::Clean;
        for c in changes {
            pending.note(*c);
        }
        plan_refresh(pending, true, 1920, 1080)
    }

    /// Two polls inside one frame period, each copying a new desktop with its own dirty
    /// rectangle: `frame()` must refresh both regions, not only the second.
    #[test]
    fn every_polls_region_reaches_the_composite() {
        let a = rect(10, 10, 50, 50);
        let b = rect(1000, 600, 1100, 700);
        assert_eq!(
            refresh_after(&[FrameChange::Region(a), FrameChange::Region(b)]),
            Refresh::Partial(Some(rect(10, 10, 1100, 700)))
        );
    }

    /// "Unknown" means everything may have changed, not that nothing did.
    #[test]
    fn an_unknown_change_refreshes_the_whole_image() {
        assert_eq!(refresh_after(&[FrameChange::Unknown]), Refresh::Whole);
        // a later, exact region cannot narrow it again
        assert_eq!(
            refresh_after(&[FrameChange::Unknown, FrameChange::Region(rect(0, 0, 4, 4))]),
            Refresh::Whole
        );
        assert_eq!(
            refresh_after(&[FrameChange::Region(rect(0, 0, 4, 4)), FrameChange::Unknown]),
            Refresh::Whole
        );
    }

    /// A positively empty frame after a dirty one must not erase what the dirty one owed.
    #[test]
    fn an_empty_frame_keeps_what_is_owed() {
        let a = rect(100, 100, 200, 200);
        assert_eq!(
            refresh_after(&[FrameChange::Region(a), FrameChange::Empty]),
            Refresh::Partial(Some(a))
        );
        assert_eq!(refresh_after(&[]), Refresh::Partial(None));
        assert_eq!(refresh_after(&[FrameChange::Empty]), Refresh::Partial(None));
    }

    #[test]
    fn a_region_spanning_the_output_is_a_whole_copy() {
        assert_eq!(
            refresh_after(&[
                FrameChange::Region(rect(0, 0, 960, 1080)),
                FrameChange::Region(rect(960, 0, 1920, 1080)),
            ]),
            Refresh::Whole
        );
    }

    #[test]
    fn an_invalid_composite_is_always_a_whole_copy() {
        assert_eq!(
            plan_refresh(Pending::Clean, false, 1920, 1080),
            Refresh::Whole
        );
        assert_eq!(
            plan_refresh(Pending::Region(rect(0, 0, 1, 1)), false, 1920, 1080),
            Refresh::Whole
        );
    }

    /// The secure desktop answers re-duplication with `E_ACCESSDENIED` for as long as it is up;
    /// that, and its relatives, must be waited out, not counted towards giving up.
    #[test]
    fn secure_desktop_refusals_are_waited_out() {
        for code in [E_ACCESSDENIED, DXGI_ERROR_SESSION_DISCONNECTED] {
            assert_eq!(
                classify_redup_failure(code),
                RedupFailure::Denied,
                "{code:?}"
            );
        }
        // these two can be permanent on a fixed device (an output whose composing adapter
        // moved), so they are retried only up to a deadline
        for code in [DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE] {
            assert_eq!(
                classify_redup_failure(code),
                RedupFailure::Unavailable,
                "{code:?}"
            );
        }
        for code in [
            DXGI_ERROR_DEVICE_REMOVED,
            DXGI_ERROR_DEVICE_RESET,
            DXGI_ERROR_DEVICE_HUNG,
            DXGI_ERROR_DRIVER_INTERNAL_ERROR,
        ] {
            assert_eq!(
                classify_redup_failure(code),
                RedupFailure::DeviceLost,
                "{code:?}"
            );
        }
        assert_eq!(
            classify_redup_failure(DXGI_ERROR_INVALID_CALL),
            RedupFailure::Other
        );
    }

    fn run_at(t0: Instant) -> DeniedRun {
        DeniedRun {
            since: t0,
            attempts: 0,
            logged_at: t0,
            unavailable_since: None,
        }
    }

    /// An `UNSUPPORTED` run that outlasts the deadline is fatal; a secure desktop is not.
    #[test]
    fn an_unsupported_run_past_its_deadline_is_fatal() {
        let t0 = Instant::now();
        let mut run = run_at(t0);
        assert!(!run.note(RedupFailure::Unavailable, t0));
        assert!(!run.note(RedupFailure::Unavailable, t0 + UNAVAILABLE_DEADLINE));
        assert!(run.note(
            RedupFailure::Unavailable,
            t0 + UNAVAILABLE_DEADLINE + Duration::from_millis(200)
        ));

        // E_ACCESSDENIED for far longer than the deadline is still waited out
        let mut run = run_at(t0);
        for s in 0..120 {
            assert!(!run.note(RedupFailure::Denied, t0 + Duration::from_secs(s)));
        }
        assert_eq!(run.attempts, 120);

        // a secure-desktop answer breaks an UNSUPPORTED stretch: the deadline counts again from
        // the next UNSUPPORTED, not from the start of the run
        let mut run = run_at(t0);
        assert!(!run.note(RedupFailure::Unavailable, t0));
        assert!(!run.note(RedupFailure::Denied, t0 + Duration::from_secs(10)));
        let t1 = t0 + Duration::from_secs(20);
        assert!(!run.note(RedupFailure::Unavailable, t1));
        assert!(!run.note(RedupFailure::Unavailable, t1 + Duration::from_secs(10)));
        assert!(run.note(
            RedupFailure::Unavailable,
            t1 + UNAVAILABLE_DEADLINE + Duration::from_secs(1)
        ));
    }

    /// BGRA8 alone refused with UNSUPPORTED (an HDR output whose cached description still said
    /// SDR) is retried once with every format; nothing else is.
    #[test]
    fn a_refused_bgra_only_list_is_widened_once() {
        assert_eq!(
            widened_formats(&DUP1_FORMATS[..1], DXGI_ERROR_UNSUPPORTED),
            Some(&DUP1_FORMATS[..])
        );
        assert_eq!(widened_formats(&DUP1_FORMATS, DXGI_ERROR_UNSUPPORTED), None);
        assert_eq!(widened_formats(&DUP1_FORMATS[..1], E_ACCESSDENIED), None);
    }

    /// A build that fails is not attempted again on the next frame.
    #[test]
    fn a_failed_lazy_build_is_not_retried() {
        let mut calls = 0;
        let mut lazy: Lazy<u32> = Lazy::Untried;
        for _ in 0..5 {
            assert!(lazy
                .get_or_try(|| {
                    calls += 1;
                    bail!("D3DCompile failed")
                })
                .is_err());
        }
        assert_eq!(calls, 1);

        // reset (a re-duplication) allows exactly one more try, and a success is kept
        lazy = Lazy::Untried;
        assert_eq!(*lazy.get_or_try(|| Ok(7)).unwrap(), 7);
        assert_eq!(
            *lazy
                .get_or_try(|| -> Result<u32> { panic!("built twice") })
                .unwrap(),
            7
        );
    }

    #[test]
    fn the_refusal_backoff_climbs_and_is_capped() {
        let ms: Vec<u128> = (1..=6).map(|n| denied_backoff(n).as_millis()).collect();
        assert_eq!(ms, vec![50, 100, 200, 200, 200, 200]);
        assert_eq!(denied_backoff(0).as_millis(), 50);
        assert_eq!(denied_backoff(u32::MAX), DENIED_BACKOFF_MAX);
    }

    /// A surface DXGI hands back in a format `CopyResource` cannot take must never reach it.
    #[test]
    fn only_bgra_family_surfaces_are_copied() {
        let size = (1920, 1080);
        assert_eq!(
            surface_route(DXGI_FORMAT_B8G8R8A8_UNORM, size, size),
            SurfaceRoute::Copy
        );
        assert_eq!(
            surface_route(DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, size, size),
            SurfaceRoute::Copy
        );
        assert_eq!(
            surface_route(DXGI_FORMAT_R10G10B10A2_UNORM, size, size),
            SurfaceRoute::Blit
        );
        assert_eq!(
            surface_route(DXGI_FORMAT_R16G16B16A16_FLOAT, size, size),
            SurfaceRoute::ScRgb
        );
        assert_eq!(
            surface_route(DXGI_FORMAT(29), size, size), // R8G8B8A8_UNORM_SRGB
            SurfaceRoute::Unsupported
        );
        assert_eq!(
            surface_route(DXGI_FORMAT_B8G8R8A8_UNORM, (1920, 1200), size),
            SurfaceRoute::Unsupported
        );
    }

    #[test]
    fn an_sdr_output_is_offered_bgra_alone() {
        assert_eq!(dup_formats(false, true), &[DXGI_FORMAT_B8G8R8A8_UNORM]);
        assert_eq!(dup_formats(true, true), &DUP1_FORMATS);
        // with the switch off, the full list everywhere
        assert_eq!(dup_formats(false, false), &DUP1_FORMATS);
    }

    #[test]
    fn the_white_level_scale_puts_sdr_white_at_one() {
        assert_eq!(white_level_scale(1000), Some(1.0));
        assert_eq!(white_level_scale(2500), Some(0.4)); // 200 nits
        assert_eq!(white_level_scale(0), None);
    }

    #[test]
    fn a_lost_device_says_so() {
        let e: anyhow::Error = DeviceLost {
            during: "AcquireNextFrame",
            code: DXGI_ERROR_DEVICE_REMOVED,
            reason: DXGI_ERROR_DEVICE_HUNG,
        }
        .into();
        assert!(e.downcast_ref::<DeviceLost>().is_some());
        let msg = format!("{e}");
        assert!(
            msg.contains("0x887a0005") && msg.contains("0x887a0006"),
            "{msg}"
        );
    }
}
