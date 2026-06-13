use anyhow::{Context as _, Result, bail};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BLEND_DESC,
    D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_INV_SRC_COLOR,
    D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_SRC_COLOR,
    D3D11_BLEND_ZERO, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_CPU_ACCESS_WRITE,
    D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DYNAMIC,
    D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT, ID3D11BlendState, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
use windows::core::{PCSTR, s};

const SHADER_SRC: &str = "\
cbuffer Quad : register(b0) { float4 v[4]; };\n\
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };\n\
VSOut vsmain(uint vid : SV_VertexID) {\n\
    float4 q = v[vid];\n\
    VSOut o;\n\
    o.pos = float4(q.xy, 0.0f, 1.0f);\n\
    o.uv = q.zw;\n\
    return o;\n\
}\n\
Texture2D tex0 : register(t0);\n\
SamplerState smp0 : register(s0);\n\
float4 psmain(VSOut i) : SV_Target { return tex0.Sample(smp0, i.uv); }\n";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BlendKind {
    Opaque,
    Alpha,
    Multiply,
    Xor,
}

pub type QuadVerts = [[f32; 4]; 4];

pub fn quad_verts(x: i32, y: i32, w: u32, h: u32, target_w: u32, target_h: u32) -> QuadVerts {
    let tw = target_w.max(1) as f32;
    let th = target_h.max(1) as f32;
    let x0 = x as f32 / tw * 2.0 - 1.0;
    let x1 = (x as f32 + w as f32) / tw * 2.0 - 1.0;
    let y0 = 1.0 - y as f32 / th * 2.0;
    let y1 = 1.0 - (y as f32 + h as f32) / th * 2.0;
    [
        [x0, y0, 0.0, 0.0],
        [x1, y0, 1.0, 0.0],
        [x0, y1, 0.0, 1.0],
        [x1, y1, 1.0, 1.0],
    ]
}

pub struct QuadRenderer {
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cbuf: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend_alpha: ID3D11BlendState,
    blend_multiply: ID3D11BlendState,
    blend_xor: ID3D11BlendState,
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let res = unsafe {
        D3DCompile(
            SHADER_SRC.as_ptr() as *const core::ffi::c_void,
            SHADER_SRC.len(),
            None,
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = res {
        let msg = errors
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        bail!("D3DCompile failed: {e} {msg}");
    }
    let code = code.context("D3DCompile produced no bytecode")?;
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

fn make_blend(
    device: &ID3D11Device,
    src: windows::Win32::Graphics::Direct3D11::D3D11_BLEND,
    dst: windows::Win32::Graphics::Direct3D11::D3D11_BLEND,
) -> Result<ID3D11BlendState> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: src,
        DestBlend: dst,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ZERO,
        DestBlendAlpha: D3D11_BLEND_ONE,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut out: Option<ID3D11BlendState> = None;
    unsafe { device.CreateBlendState(&desc, Some(&mut out)) }.context("CreateBlendState")?;
    out.context("blend state was null")
}

impl QuadRenderer {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<Self> {
        let vs_bytes = compile(s!("vsmain"), s!("vs_4_0"))?;
        let ps_bytes = compile(s!("psmain"), s!("ps_4_0"))?;

        let mut vs: Option<ID3D11VertexShader> = None;
        unsafe { device.CreateVertexShader(&vs_bytes, None, Some(&mut vs)) }
            .context("CreateVertexShader")?;
        let mut ps: Option<ID3D11PixelShader> = None;
        unsafe { device.CreatePixelShader(&ps_bytes, None, Some(&mut ps)) }
            .context("CreatePixelShader")?;

        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: (std::mem::size_of::<QuadVerts>() as u32).max(16),
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut cbuf: Option<ID3D11Buffer> = None;
        unsafe { device.CreateBuffer(&cb_desc, None, Some(&mut cbuf)) }
            .context("CreateBuffer (quad cbuffer)")?;

        let samp_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler: Option<ID3D11SamplerState> = None;
        unsafe { device.CreateSamplerState(&samp_desc, Some(&mut sampler)) }
            .context("CreateSamplerState")?;

        Ok(Self {
            context: context.clone(),
            vs: vs.context("vertex shader was null")?,
            ps: ps.context("pixel shader was null")?,
            cbuf: cbuf.context("quad cbuffer was null")?,
            sampler: sampler.context("sampler was null")?,
            blend_alpha: make_blend(device, D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_INV_SRC_ALPHA)?,
            blend_multiply: make_blend(device, D3D11_BLEND_ZERO, D3D11_BLEND_SRC_COLOR)?,
            blend_xor: make_blend(device, D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_COLOR)?,
        })
    }

    pub fn draw(
        &self,
        rtv: &ID3D11RenderTargetView,
        srv: &ID3D11ShaderResourceView,
        blend: BlendKind,
        verts: &QuadVerts,
        target_w: u32,
        target_h: u32,
    ) -> Result<()> {
        let ctx = &self.context;
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .context("Map(quad cbuffer)")?;
            std::ptr::copy_nonoverlapping(
                verts.as_ptr() as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of::<QuadVerts>(),
            );
            ctx.Unmap(&self.cbuf, 0);

            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: target_w as f32,
                Height: target_h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[viewport]));
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            let blend_state = match blend {
                BlendKind::Opaque => None,
                BlendKind::Alpha => Some(&self.blend_alpha),
                BlendKind::Multiply => Some(&self.blend_multiply),
                BlendKind::Xor => Some(&self.blend_xor),
            };
            ctx.OMSetBlendState(blend_state, None, 0xffff_ffff);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.Draw(4, 0);
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}

pub struct CursorSprite {
    pub passes: Vec<(ID3D11ShaderResourceView, BlendKind)>,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

fn make_srv(device: &ID3D11Device, w: u32, h: u32, pixels: &[u32]) -> Result<ID3D11ShaderResourceView> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr() as *const core::ffi::c_void,
        SysMemPitch: w * 4,
        SysMemSlicePitch: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }
        .context("CreateTexture2D (cursor sprite)")?;
    let tex = tex.context("cursor sprite texture was null")?;
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    unsafe { device.CreateShaderResourceView(&tex, None, Some(&mut srv)) }
        .context("CreateShaderResourceView (cursor sprite)")?;
    srv.context("cursor sprite SRV was null")
}

const OPAQUE_WHITE: u32 = 0xFFFF_FFFF;
const OPAQUE_BLACK: u32 = 0xFF00_0000;

pub fn build_sprite(
    device: &ID3D11Device,
    info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    data: &[u8],
) -> Result<CursorSprite> {
    let w = info.Width;
    let pitch = info.Pitch as usize;

    if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
        let h = info.Height / 2;
        let mut and_px = vec![0u32; (w * h) as usize];
        let mut xor_px = vec![0u32; (w * h) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let bit = |row: usize| -> u32 {
                    let byte = data.get(row * pitch + x / 8).copied().unwrap_or(0);
                    ((byte >> (7 - (x & 7))) & 1) as u32
                };
                let i = y * w as usize + x;
                and_px[i] = if bit(y) == 1 { OPAQUE_WHITE } else { OPAQUE_BLACK };
                xor_px[i] = if bit(y + h as usize) == 1 { OPAQUE_WHITE } else { OPAQUE_BLACK };
            }
        }
        return Ok(CursorSprite {
            passes: vec![
                (make_srv(device, w, h, &and_px)?, BlendKind::Multiply),
                (make_srv(device, w, h, &xor_px)?, BlendKind::Xor),
            ],
            width: w,
            height: h,
            hotspot_x: info.HotSpot.x,
            hotspot_y: info.HotSpot.y,
        });
    }

    let h = info.Height;
    let px_at = |x: usize, y: usize| -> [u8; 4] {
        let o = y * pitch + x * 4;
        [
            data.get(o).copied().unwrap_or(0),
            data.get(o + 1).copied().unwrap_or(0),
            data.get(o + 2).copied().unwrap_or(0),
            data.get(o + 3).copied().unwrap_or(0),
        ]
    };
    let pack = |b: u8, g: u8, r: u8, a: u8| -> u32 {
        (b as u32) | ((g as u32) << 8) | ((r as u32) << 16) | ((a as u32) << 24)
    };

    if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
        let mut base_px = vec![0u32; (w * h) as usize];
        let mut xor_px = vec![0u32; (w * h) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let [b, g, r, mask] = px_at(x, y);
                let i = y * w as usize + x;
                if mask == 0xFF {
                    base_px[i] = 0;
                    xor_px[i] = pack(b, g, r, 0xFF);
                } else {
                    base_px[i] = pack(b, g, r, 0xFF);
                    xor_px[i] = OPAQUE_BLACK;
                }
            }
        }
        return Ok(CursorSprite {
            passes: vec![
                (make_srv(device, w, h, &base_px)?, BlendKind::Alpha),
                (make_srv(device, w, h, &xor_px)?, BlendKind::Xor),
            ],
            width: w,
            height: h,
            hotspot_x: info.HotSpot.x,
            hotspot_y: info.HotSpot.y,
        });
    }

    if info.Type != DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
        bail!("unknown pointer shape type {}", info.Type);
    }

    let mut px = vec![0u32; (w * h) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let [b, g, r, a] = px_at(x, y);
            px[y * w as usize + x] = pack(b, g, r, a);
        }
    }
    Ok(CursorSprite {
        passes: vec![(make_srv(device, w, h, &px)?, BlendKind::Alpha)],
        width: w,
        height: h,
        hotspot_x: info.HotSpot.x,
        hotspot_y: info.HotSpot.y,
    })
}
