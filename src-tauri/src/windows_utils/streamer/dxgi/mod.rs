pub mod cursor;

use anyhow::{anyhow, bail, Context as _, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, POINT, RECT};
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
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTPUT_DESC,
};

use crate::windows_capture::d3d11::AdapterInfo;
use cursor::{build_sprite, quad_verts, BlendKind, CursorSprite, QuadRenderer};

const REDUP_MAX_RETRIES: u32 = 40;

/// How many `DXGI_ERROR_INVALID_CALL` recoveries to attempt before giving up. A settling virtual
/// display produces a handful; an unbounded stream of them is a real fault.
const INVALID_CALL_MAX_RECOVERIES: u32 = 20;

/// How many times duplication may be re-established without a single frame coming out of it.
///
/// `redup_failures` only ever counted *failed* re-duplications, and a success reset it to zero, so
/// an output that hands back `DXGI_ERROR_ACCESS_LOST` on every acquire looped forever: acquire,
/// lose access, re-duplicate, acquire, lose access. Measured on this project's dev box against a
/// freshly created IDD virtual display, it did exactly that for the whole run and delivered no
/// frames at all, while the client sat on a negotiated session showing nothing. A capture that
/// cannot produce a frame has to say so and stop, not spin.
const REDUP_WITHOUT_FRAME_MAX: u32 = 40;

/// Holds an acquired duplication frame and releases it on drop.
///
/// `AcquireNextFrame` and `ReleaseFrame` are a strict pair: miss one release and every subsequent
/// acquire returns `DXGI_ERROR_INVALID_CALL` forever. `poll` has a `?` between the two, so any
/// early return -- present or future -- used to leak the frame and wedge the capture thread.
struct AcquiredFrame<'a>(&'a IDXGIOutputDuplication);

impl Drop for AcquiredFrame<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.0.ReleaseFrame() };
    }
}

/// Grows `bounds` to also contain `r`.
fn union_rect(bounds: &mut Option<RECT>, r: RECT) {
    match bounds {
        Some(b) => {
            b.left = b.left.min(r.left);
            b.top = b.top.min(r.top);
            b.right = b.right.max(r.right);
            b.bottom = b.bottom.max(r.bottom);
        }
        None => *bounds = Some(r),
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
    redup_failures: u32,
    /// Successful re-duplications since the last frame actually came out. Reset by `poll` on the
    /// first acquire that yields a frame; a run of these with no frame is a dead capture.
    redup_without_frame: u32,
    invalid_call_recoveries: u32,
    /// `LastPresentTime` of the most recent dirty frame, already on the shared host clock.
    /// The compositor's own frame time; without it the pipeline can only measure from the
    /// moment `poll` returned, which hides however long the frame waited to be handed over.
    last_present_host_ns: u64,

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
    composite: ID3D11Texture2D,
    composite_rtv: ID3D11RenderTargetView,
    renderer: QuadRenderer,

    have_desktop: bool,
    sprite: Option<CursorSprite>,
    /// The pointer position exactly as DXGI reported it, in desktop coordinates. Compared
    /// against the next report to decide whether the pointer actually moved. It used to share a
    /// field with the draw position below, which is a different coordinate system, so the
    /// comparison never matched and every pointer update marked the frame dirty.
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
    /// Union of this frame's dirty and move rectangles in desktop coordinates, or `None` when the
    /// driver reported none (which means "assume everything").
    changed_bounds: Option<RECT>,
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
}

/// Every adapter that claims `device_name`, in enumeration order.
///
/// On a hybrid-graphics laptop the same display is enumerated by more than one adapter, and only
/// one of them can duplicate it: the desktop for that output is composed on exactly one GPU, and
/// `DuplicateOutput` on any other returns `DXGI_ERROR_UNSUPPORTED`. Measured on this project's
/// dev box, where `\\.\DISPLAY1` is listed by the RTX 3080 Ti but composed by the Intel iGPU, and
/// duplication on the NVIDIA adapter fails with `0x887A0004` however it is asked for. Returning
/// only the first match is what left that whole hardware class with no working DXGI path.
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
            // Before IddCx 1.4 the OS composed an indirect display on the POST adapter, or on
            // WARP when the POST adapter was PnpStopped, and the bundled driver declares
            // IddCx 1.2. A WARP-composed virtual display means every frame of it is rendered on
            // the CPU that is also running the encoder, so it is worth shouting about.
            let (label, software) = match unsafe { adapter.GetDesc1() } {
                Ok(d) => {
                    let info = AdapterInfo::from_dxgi_desc1(&d);
                    (info.label(), info.software)
                }
                Err(_) => ("an undescribed adapter".to_string(), false),
            };
            found.push(OutputCandidate {
                adapter: adapter.clone(),
                output: output1,
                desc,
                label,
                software,
            });
            break;
        }
    }
    if found.is_empty() {
        bail!("no DXGI output named {device_name} (saw: {seen:?})")
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
const DUP1_FORMATS: [windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT; 3] = [
    DXGI_FORMAT_B8G8R8A8_UNORM,
    windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R10G10B10A2_UNORM,
    windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT,
];

/// Which duplication entry point produced the live duplication. Reported so a support log says
/// which one this machine needed rather than only that capture worked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DuplicationApi {
    /// `IDXGIOutput5::DuplicateOutput1` — Windows 10 1607+, and the only one that succeeds on
    /// several hybrid-graphics laptops and on any display in HDR mode.
    Output5,
    /// `IDXGIOutput1::DuplicateOutput` — the original, present since Windows 8.
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
/// hybrid-graphics laptops — measured on this project's dev box, an RTX 3080 Ti Laptop next to an
/// Intel UHD iGPU — while `DuplicateOutput1` with an explicit format list succeeds on the same
/// output and the same device. It is also the only one that can duplicate a display in HDR mode.
/// So it is tried first and the older call is the fallback, not the other way round.
///
/// Probed by `QueryInterface`, never by a build number: `IDXGIOutput5` is simply absent on a
/// machine that does not have it, and the cast fails.
fn duplicate_once(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
) -> std::result::Result<(IDXGIOutputDuplication, DuplicationApi), windows::core::Error> {
    let out5_err = match output.cast::<IDXGIOutput5>() {
        Ok(out5) => match unsafe { out5.DuplicateOutput1(device, 0, &DUP1_FORMATS) } {
            Ok(dup) => return Ok((dup, DuplicationApi::Output5)),
            Err(e) => Some(e),
        },
        Err(e) => Some(e),
    };

    match unsafe { output.DuplicateOutput(device) } {
        Ok(dup) => Ok((dup, DuplicationApi::Output1)),
        // Report the newer call's failure, since it is the one that had the better chance.
        Err(e) => Err(out5_err.unwrap_or(e)),
    }
}

fn duplicate_with_retry(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
    attempts: u32,
) -> Result<(IDXGIOutputDuplication, DuplicationApi)> {
    let mut last: Option<windows::core::Error> = None;
    for i in 0..attempts {
        match duplicate_once(output, device) {
            Ok(pair) => return Ok(pair),
            Err(e) => {
                last = Some(e);
                if i + 1 < attempts {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
    Err(anyhow!(
        "DuplicateOutput1 and DuplicateOutput both failed after {attempts} attempts: {last:?}"
    ))
}

/// Answers "would DXGI duplication actually work on this display right now", by trying it.
///
/// The compatibility report used to answer that question with `CreateDXGIFactory1().is_ok()`,
/// which is true on every Windows since 7 and says nothing about duplication. On a hybrid laptop
/// where `DuplicateOutput` is unsupported that produced a report claiming a fallback that could
/// never run. The duplication is created and immediately dropped, so nothing is held.
pub fn probe_duplication() -> Result<DuplicationApi> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut last: Option<windows::core::Error> = None;
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
            let (device, _context) = create_device_on(&adapter)?;
            seen += 1;
            match duplicate_once(&output1, &device) {
                Ok((dup, api)) => {
                    drop(dup);
                    return Ok(api);
                }
                Err(e) => last = Some(e),
            }
        }
    }
    if seen == 0 {
        bail!("no desktop-attached DXGI output to duplicate");
    }
    Err(anyhow!(
        "DXGI duplication is unsupported on all {seen} desktop outputs: {last:?}"
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
        let candidates = find_outputs(device_name)?;
        let mut errors = Vec::new();
        let mut chosen: Option<(OutputCandidate, ID3D11Device, ID3D11DeviceContext)> = None;
        for candidate in candidates {
            let (device, context) = match create_device_on(&candidate.adapter) {
                Ok(pair) => pair,
                Err(e) => {
                    errors.push(format!("{}: {e:#}", candidate.label));
                    continue;
                }
            };
            // Duplication is adapter-local: the one adapter that composes this desktop is the
            // one that can duplicate it, and every other returns DXGI_ERROR_UNSUPPORTED. A
            // single attempt each, because retrying an unsupported call cannot help; the
            // retry loop below is for the adapter that does answer.
            match duplicate_once(&candidate.output, &device) {
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
                Err(e) => errors.push(format!("{}: {e:?}", candidate.label)),
            }
        }
        let Some((candidate, device, context)) = chosen else {
            bail!(
                "no adapter can duplicate {device_name} ({})",
                errors.join("; ")
            )
        };
        let OutputCandidate {
            output,
            desc: out_desc,
            label: adapter_label,
            ..
        } = candidate;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let (dup, api, phys_w, phys_h, raw_rotation, rotation) = loop {
            let (dup, api) = duplicate_with_retry(&output, &device, 5)?;
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
                bail!(
                    "duplication mode {w}x{h} (rotation {:?}) does not match expected desktop \
                     {logical_w}x{logical_h} (mode change did not settle)",
                    dd.Rotation
                );
            }
            drop(dup);
            std::thread::sleep(std::time::Duration::from_millis(100));
        };

        let (desktop, desktop_srv, _) = make_target(&device, phys_w, phys_h)?;
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
            redup_failures: 0,
            redup_without_frame: 0,
            invalid_call_recoveries: 0,
            last_present_host_ns: 0,
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
            composite,
            composite_rtv,
            renderer,
            have_desktop: false,
            sprite: None,
            pointer_pos: POINT::default(),
            draw_pos: POINT::default(),
            cursor_visible: false,
            drawn_rect: None,
            composite_valid: false,
            shape_buf: Vec::new(),
            meta_buf: Vec::new(),
            changed_bounds: None,
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
        match duplicate_once(&self.output, &self.device) {
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
                self.redup_without_frame += 1;
                if self.redup_without_frame > REDUP_WITHOUT_FRAME_MAX {
                    bail!(
                        "duplication was re-established {} times without ever delivering a \
                         frame; this output cannot be captured with DXGI Desktop Duplication",
                        self.redup_without_frame
                    );
                }
                // One line per re-establishment floods the log bus, and every log line is an IPC
                // event to the UI. Say it once, then only every tenth, with the running count.
                if self.redup_without_frame == 1 || self.redup_without_frame.is_multiple_of(10) {
                    tprintln!(
                        "dxgi: duplication re-established ({} since the last frame)",
                        self.redup_without_frame
                    );
                }
                Ok(())
            }
            Err(e) => {
                self.redup_failures += 1;
                if self.redup_failures > REDUP_MAX_RETRIES {
                    bail!(
                        "re-duplication failed {} times, giving up: {e:?}",
                        self.redup_failures
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                Ok(())
            }
        }
    }

    /// Reads this frame's change metadata and records the union of it in `changed_bounds`.
    ///
    /// Returns true when the driver said, positively, that nothing in the desktop image changed —
    /// the case worth skipping the copy and the encode for. Anything ambiguous (no metadata,
    /// coalesced frames, a failed query) reports false, which is the safe direction.
    ///
    /// The rectangles themselves used to be fetched and thrown away. They are the cheapest
    /// changed-region hint available, and `frame()` now uses them so that moving a cursor does
    /// not cost a full-frame copy.
    fn collect_change(
        &mut self,
        dup: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> bool {
        // "Assume everything changed" until the metadata says otherwise.
        self.changed_bounds = None;

        // >1 means the OS coalesced updates while we held the previous frame, so the metadata
        // describes more than one present and is not an exact region for this one.
        if info.AccumulatedFrames > 1 || info.TotalMetadataBufferSize == 0 {
            return false;
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
            return false;
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
            // A move touches both the rectangle it reads from and the one it writes to.
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
            return false;
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

        self.changed_bounds = bounds;
        move_count == 0 && dirty_count == 0
    }

    /// Host-clock nanoseconds of the last present, or 0 if the driver has not reported one.
    pub fn last_present_host_ns(&self) -> u64 {
        self.last_present_host_ns
    }

    pub fn poll(&mut self, timeout_ms: u32) -> Result<PollStatus> {
        let Some(dup) = self.dup.clone() else {
            self.redup()?;
            return Ok(PollStatus::Timeout);
        };

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(PollStatus::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.dup = None;
                return Ok(PollStatus::Timeout);
            }
            // A frame was left acquired somewhere, or the duplication is in a state we cannot
            // reason about. This is recoverable by re-duplicating; it used to kill the capture
            // thread, which the client experiences as a connected session that stays black.
            // A freshly created virtual display reliably reproduced it: the IDD is still
            // attaching, `ACCESS_LOST` fires repeatedly, and the next acquire is invalid.
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
            Err(e) => return Err(anyhow!("AcquireNextFrame: {e:?}")),
        }
        // Everything past here is inside the acquire; the guard releases on every exit path.
        let _acquired = AcquiredFrame(&dup);
        // An acquire that returned is proof the duplication is live, so the "re-established and
        // produced nothing" run ends here.
        self.redup_without_frame = 0;
        self.invalid_call_recoveries = 0;

        let mut dirty = false;

        if info.LastPresentTime != 0 {
            self.last_present_host_ns =
                super::clock::dxgi_present_time_to_host_ns(info.LastPresentTime);
            if let Some(res) = &resource {
                const MAX_CONSECUTIVE_SKIPS: u32 = 5;
                let region_empty = self.collect_change(&dup, &info)
                    && self.consecutive_skips < MAX_CONSECUTIVE_SKIPS;
                if !region_empty {
                    let tex: ID3D11Texture2D = res
                        .cast()
                        .context("duplication resource as ID3D11Texture2D")?;
                    unsafe { self.context.CopyResource(&self.desktop, &tex) };
                    self.have_desktop = true;
                    dirty = true;
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
                dirty = true;
            }
            if visible && (pos.x != self.pointer_pos.x || pos.y != self.pointer_pos.y) {
                dirty = true;
            }
            self.cursor_visible = visible;
            if visible {
                self.pointer_pos = pos;
            }
        }

        Ok(if dirty && self.have_desktop {
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
            // A different coordinate system from `pointer_pos`: output-relative and with the
            // hotspot subtracted, i.e. the sprite's top-left corner in composite space.
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

        if identity && !cursor_on {
            // Nothing to compose: the encoder reads the duplication copy directly. The composite
            // is now stale, so the next composed frame must be a full copy.
            self.composite_valid = false;
            self.drawn_rect = None;
            return Ok(&self.desktop);
        }

        if identity {
            // `composite` already holds the previous frame. Refreshing only what changed plus
            // the rectangle the last cursor was drawn over turns a cursor move from a
            // full-resolution copy (8.3 MB at 1080p, 14.7 MB at 1440p, every frame the pointer
            // moves) into two small ones.
            let incremental = self.composite_valid
                && self
                    .changed_bounds
                    .is_none_or(|b| !self.covers_whole_output(b));
            if incremental {
                let mut restore: Vec<RECT> = Vec::with_capacity(2);
                if let Some(b) = self.changed_bounds {
                    restore.push(b);
                }
                if let Some((x, y, w, h)) = self.drawn_rect {
                    restore.push(RECT {
                        left: x,
                        top: y,
                        right: x + w as i32,
                        bottom: y + h as i32,
                    });
                }
                for r in restore {
                    self.copy_region_from_desktop(r);
                }
            } else {
                unsafe { self.context.CopyResource(&self.composite, &self.desktop) };
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

    /// True when a rectangle already spans the whole output, in which case the incremental path
    /// would cost more than a straight `CopyResource`.
    fn covers_whole_output(&self, r: RECT) -> bool {
        r.left <= 0 && r.top <= 0 && r.right >= self.width as i32 && r.bottom >= self.height as i32
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

    /// The union of this frame's dirty and move rectangles, in output coordinates, or `None` when
    /// the driver reported none and the whole frame must be assumed changed. An encoder can use
    /// it as a region hint.
    pub fn changed_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let r = self.changed_bounds?;
        let left = r.left.clamp(0, self.width as i32) as u32;
        let top = r.top.clamp(0, self.height as i32) as u32;
        let right = r.right.clamp(0, self.width as i32) as u32;
        let bottom = r.bottom.clamp(0, self.height as i32) as u32;
        (right > left && bottom > top).then_some((left, top, right - left, bottom - top))
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

    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
        let mut p = POINT::default();
        if GetCursorPos(&mut p).is_ok() {
            let _ = SetCursorPos(p.x + 1, p.y);
            let _ = SetCursorPos(p.x, p.y);
        }
    }

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
