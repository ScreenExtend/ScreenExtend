pub mod cursor;

use anyhow::{Context as _, Result, anyhow, bail};
use windows::Win32::Foundation::{HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
    DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTPUT_DESC, IDXGIAdapter1,
    IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use cursor::{BlendKind, CursorSprite, QuadRenderer, build_sprite, quad_verts};

const REDUP_MAX_RETRIES: u32 = 40;

pub enum PollStatus {
    Dirty,
    Timeout,
}

pub struct Duplicator {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    redup_failures: u32,

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
    cursor_pos: POINT,
    cursor_visible: bool,
    shape_buf: Vec<u8>,
}

fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

fn find_output(device_name: &str) -> Result<(IDXGIAdapter1, IDXGIOutput1, DXGI_OUTPUT_DESC)> {
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
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
            if name == device_name {
                let adapter_desc = unsafe { adapter.GetDesc1() }.ok();
                tprintln!(
                    "dxgi: output {} found on adapter '{}'",
                    name,
                    adapter_desc
                        .map(|d| wide_to_string(&d.Description))
                        .unwrap_or_else(|| "<unknown>".into()),
                );
                let output1: IDXGIOutput1 =
                    output.cast().context("IDXGIOutput as IDXGIOutput1 (needs DXGI 1.2+)")?;
                return Ok((adapter, output1, desc));
            }
            seen.push(name);
        }
    }
    bail!("no DXGI output named {device_name} (saw: {seen:?})")
}

fn create_device_on(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let try_flags = |flags: D3D11_CREATE_DEVICE_FLAG| -> Result<(ID3D11Device, ID3D11DeviceContext)> {
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

fn duplicate_with_retry(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
    attempts: u32,
) -> Result<IDXGIOutputDuplication> {
    let mut last: Option<windows::core::Error> = None;
    for i in 0..attempts {
        match unsafe { output.DuplicateOutput(device) } {
            Ok(dup) => return Ok(dup),
            Err(e) => {
                last = Some(e);
                if i + 1 < attempts {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
    Err(anyhow!("DuplicateOutput failed after {attempts} attempts: {last:?}"))
}

fn make_target(
    device: &ID3D11Device,
    w: u32,
    h: u32,
) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView, ID3D11RenderTargetView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
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
        let (adapter, output, out_desc) = find_output(device_name)?;
        let (device, context) = create_device_on(&adapter)?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let (dup, phys_w, phys_h, raw_rotation, rotation) = loop {
            let dup = duplicate_with_retry(&output, &device, 5)?;
            let dd: DXGI_OUTDUPL_DESC = unsafe { dup.GetDesc() };
            let (w, h) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
            let rotated =
                matches!(dd.Rotation, DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270);
            if rotated && (h, w) == (logical_w, logical_h) {
                break (dup, w, h, dd.Rotation, dd.Rotation);
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
                break (dup, w, h, dd.Rotation, effective);
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
            "dxgi: duplication ready on {device_name}: {phys_w}x{phys_h}, rotation={rotation:?}, \
             logical={logical_w}x{logical_h}"
        );

        Ok(Self {
            device,
            context,
            output,
            dup: Some(dup),
            redup_failures: 0,
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
            cursor_pos: POINT::default(),
            cursor_visible: false,
            shape_buf: Vec::new(),
        })
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    fn redup(&mut self) -> Result<()> {
        match unsafe { self.output.DuplicateOutput(&self.device) } {
            Ok(dup) => {
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
                tprintln!("dxgi: duplication re-established");
                Ok(())
            }
            Err(e) => {
                self.redup_failures += 1;
                if self.redup_failures > REDUP_MAX_RETRIES {
                    bail!("re-duplication failed {} times, giving up: {e:?}", self.redup_failures);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                Ok(())
            }
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
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(PollStatus::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.dup = None;
                return Ok(PollStatus::Timeout);
            }
            Err(e) => return Err(anyhow!("AcquireNextFrame: {e:?}")),
        }

        let mut dirty = false;

        if info.LastPresentTime != 0 {
            if let Some(res) = &resource {
                let tex: ID3D11Texture2D =
                    res.cast().context("duplication resource as ID3D11Texture2D")?;
                unsafe { self.context.CopyResource(&self.desktop, &tex) };
                self.have_desktop = true;
                dirty = true;
            }
        }

        if info.PointerShapeBufferSize > 0 {
            self.shape_buf.resize(info.PointerShapeBufferSize as usize, 0);
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
            if visible && (pos.x != self.cursor_pos.x || pos.y != self.cursor_pos.y) {
                dirty = true;
            }
            self.cursor_visible = visible;
            if visible {
                self.cursor_pos = pos;
            }
        }

        let _ = unsafe { dup.ReleaseFrame() };

        Ok(if dirty && self.have_desktop { PollStatus::Dirty } else { PollStatus::Timeout })
    }

    fn refresh_cursor_state(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::{CURSORINFO, CURSOR_SHOWING, GetCursorInfo};

        let Some(sprite) = self.sprite.as_ref() else { return };
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
            self.cursor_pos = POINT {
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
            DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE180 | DXGI_MODE_ROTATION_ROTATE270
        );
        let cursor_on = self.cursor_visible && self.sprite.is_some();

        if identity && !cursor_on {
            return Ok(&self.desktop);
        }

        if identity {
            unsafe { self.context.CopyResource(&self.composite, &self.desktop) };
        } else {
            self.renderer.draw(
                &self.composite_rtv,
                &self.desktop_srv,
                BlendKind::Opaque,
                &blit_verts(self.rotation),
                self.width,
                self.height,
            )?;
        }

        if cursor_on {
            let sprite = self.sprite.as_ref().unwrap();
            let verts = quad_verts(
                self.cursor_pos.x,
                self.cursor_pos.y,
                sprite.width,
                sprite.height,
                self.width,
                self.height,
            );
            for (srv, blend) in &sprite.passes {
                self.renderer
                    .draw(&self.composite_rtv, srv, *blend, &verts, self.width, self.height)?;
            }
        }

        Ok(&self.composite)
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
        info.index, info.name, info.width, info.height
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
    let (cx, cy) = (dup.cursor_pos.x, dup.cursor_pos.y);
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
        info.width, info.height
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
