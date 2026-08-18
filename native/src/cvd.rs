//! Overseer — in-game colour-vision (daltonization) accessibility filter.
//!
//! A fullscreen D3D11 post-process applied to the game's FINISHED frame in the back
//! buffer, right before our imgui overlay draws over it. It simulates a colour
//! deficiency, then redistributes the lost signal into channels the viewer can still
//! tell apart (Fidaner error-redistribution) — so red/green (or blue/yellow) cues
//! that would collapse into one colour stay distinguishable.
//!
//! Runs entirely on the render thread as an imgui background-draw-list `RawCallback`,
//! mirroring `intro_player`: it reuses the device/immediate-context that module
//! captured (`intro_player::device()/context()`), needs no IL2CPP, and builds all its
//! D3D11 resources lazily on the render thread the first time it draws.
//!
//! FAIL-SAFE INVARIANTS (see `render_filter`):
//!   * OFF is free — when the mode is 0 the fast path returns before any device touch.
//!   * Any D3D11 error (a null device/context, a failed HRESULT, a missing resource)
//!     trips a permanent `DISABLED` flag: the frame is never touched again this
//!     session, logged exactly once. There are NO unwraps/panics on the render thread
//!     — every fallible step is `?`-propagated and a `None` means "give up silently".
//!
//! The D3D11 body needs the `windows` crate, which is gated behind the `banner`
//! feature (in `default`). Without `banner` every entry point is a no-op.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use hudhook::imgui::sys;

// ── state ────────────────────────────────────────────────────────────────────────
// 0 = off, 1 = deuteranopia, 2 = protanopia, 3 = tritanopia.
static CVD_MODE: AtomicU8 = AtomicU8::new(0);
// Permanent kill switch: set the first time any D3D11 step fails. Once set the filter
// never touches the frame again (checked on the fast path).
static DISABLED: AtomicBool = AtomicBool::new(false);
// Guards the one-shot disable log line.
static LOGGED_DISABLE: AtomicBool = AtomicBool::new(false);

/// Set the active mode (0 off, 1 deuter, 2 protan, 3 tritan). Out-of-range → off.
pub fn set_mode(m: u8) {
    CVD_MODE.store(if m > 3 { 0 } else { m }, Ordering::Relaxed);
}

/// Correction strength, 0..1 (f32 bits). The pass now preserves luminance, so strength controls how
/// far hues are separated — NOT how bright the image is. 0.85 keeps the full discriminability gain
/// while stopping short of the loudest possible shift.
static CVD_STRENGTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x3f59_999a); // 0.85

/// Current correction strength (0..1).
pub fn strength() -> f32 {
    f32::from_bits(CVD_STRENGTH.load(Ordering::Relaxed))
}

/// Set the correction strength (clamped 0..1).
pub fn set_strength(v: f32) {
    CVD_STRENGTH.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

/// The active mode (0 = off). Cheap atomic load — safe to poll every frame.
pub fn mode() -> u8 {
    CVD_MODE.load(Ordering::Relaxed)
}

/// Permanently disable the filter after a D3D failure. Idempotent; logs once.
fn disable() {
    DISABLED.store(true, Ordering::Relaxed);
    if !LOGGED_DISABLE.swap(true, Ordering::Relaxed) {
        crate::tools::log("[cvd] disabled (d3d error)");
    }
}

// ── draw-list registration ─────────────────────────────────────────────────────────
/// Enqueue the fullscreen filter into imgui's background draw list, followed by a
/// `ResetRenderState` command so hudhook re-binds its own imgui pipeline afterwards
/// (our pass rebinds VS/PS/topology/viewport/blend, which would otherwise corrupt the
/// panels). Call at the VERY START of the overlay's `render()`, before any panel draws
/// — the background list renders first, so the callback lands after the game frame is
/// in the back buffer but before the overlay. Cheap no-op when the filter is off.
pub fn enqueue() {
    if mode() == 0 || DISABLED.load(Ordering::Relaxed) {
        return;
    }
    unsafe {
        let dl = sys::igGetBackgroundDrawList();
        if dl.is_null() {
            return;
        }
        sys::ImDrawList_AddCallback(dl, Some(draw_trampoline), std::ptr::null_mut());
        // -1 sentinel == ImDrawCallback_ResetRenderState → hudhook re-sets its pipeline.
        let reset: sys::ImDrawCallback = Some(std::mem::transmute(-1isize));
        sys::ImDrawList_AddCallback(dl, reset, std::ptr::null_mut());
    }
}

// imgui RawCallback trampoline (render thread, back-buffer RTV bound).
unsafe extern "C" fn draw_trampoline(_list: *const sys::ImDrawList, _cmd: *const sys::ImDrawCmd) {
    render_filter();
}

/// Apply the daltonization pass to the current back buffer. Called every frame from the
/// overlay RawCallback. Fast path returns immediately when off or permanently disabled;
/// otherwise any failure trips `disable()` and never panics.
pub fn render_filter() {
    let m = mode();
    if m == 0 || DISABLED.load(Ordering::Relaxed) {
        return;
    }
    #[cfg(feature = "banner")]
    unsafe {
        if gpu::run(m).is_none() {
            disable();
        }
    }
    #[cfg(not(feature = "banner"))]
    let _ = m; // no D3D11 without `banner` — the filter is a no-op.
}

// ── D3D11 implementation (banner-only: the `windows` crate lives behind that feature) ──
#[cfg(feature = "banner")]
mod gpu {
    use std::ffi::c_void;

    use windows::core::{s, Interface};
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

    fn log(msg: &str) {
        crate::tools::log(msg);
    }

    // Fullscreen triangle generated from SV_VertexID — no vertex buffer / input layout.
    // uv (0,0) is the top-left texel, mapped to NDC top-left, so the sampled copy of the
    // back buffer stays upright.
    const VS_SRC: &str = r"
struct VSO { float4 pos: SV_POSITION; float2 uv: TEXCOORD0; };
VSO main(uint id: SV_VertexID) {
    VSO o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}
";

    // Daltonize: simulate the deficiency (SIM * c), take the error the deficient eye loses
    // (c - sim), redistribute it into channels the viewer CAN distinguish, add it back.
    // Matrices are row-major, applied as mul(M, c) = M * c.
    //
    // Three things this pass gets right that the first version did not, and which together are why
    // it used to look "muted and similar" — the two failure modes you least want from an
    // accessibility filter, since they are the exact opposite of its purpose:
    //
    //  1. **Linear light.** The matrices model how light mixes, so they are only meaningful on
    //     linear values. Applying them straight to gamma-encoded sRGB (as before) muddies every
    //     result — mid-tones drift grey and saturation collapses. We now decode to linear, correct,
    //     and re-encode.
    //
    //  2. **Luminance preservation.** Adding the redistributed error changes how BRIGHT a pixel is,
    //     not just its hue, so the image drifted lighter and the old code compensated by blending
    //     back toward the original — which halves the discriminability gain, i.e. mutes it. Instead
    //     we rescale the corrected colour so its luminance matches the input exactly. Nothing gets
    //     brighter, nothing gets washed out, and the full hue separation is kept.
    //
    //  3. **Type-correct redistribution.** Protanopia and deuteranopia lose red↔green, so their
    //     error belongs in green/blue. Tritanopia loses blue↔yellow, so its error belongs in
    //     red/green — but it was being given the red-green matrix, pushing the error into channels
    //     the viewer already can't separate. That is why tritanopia looked the worst of the three.
    //
    // `uStrength` (0..1) is the user's dial. At 1.0 the correction is applied in full; the default
    // sits below that because a full-strength shift is visually loud even when it is correct.
    const PS_SRC: &str = r"
cbuffer Cb : register(b0) { int uMode; float uStrength; float2 uPad; };
Texture2D tex0 : register(t0);
SamplerState s0 : register(s0);

// sRGB transfer function, both directions. Cheap enough at fullscreen and the whole pass is wrong
// without it.
float3 toLinear(float3 c) {
    return (c <= 0.04045) ? (c / 12.92) : pow(abs(c + 0.055) / 1.055, 2.4);
}
float3 toSrgb(float3 c) {
    c = max(c, 0.0);
    return (c <= 0.0031308) ? (c * 12.92) : (1.055 * pow(c, 1.0 / 2.4) - 0.055);
}
float luma(float3 c) { return dot(c, float3(0.2126, 0.7152, 0.0722)); }

float4 main(float4 pos: SV_POSITION, float2 uv: TEXCOORD0) : SV_Target {
    float3 src = tex0.Sample(s0, uv).rgb;
    float3x3 sim;
    float3x3 R;
    // NB: 1 = deuteranopia, 2 = protanopia, matching the UI chips and settings. The previous shader
    // had these two the wrong way round, so a deuteranope who selected Deuteranopia was given the
    // protanopia correction: a related but distinctly different remap.
    if (uMode == 1) {         // deuteranopia — no medium-wave (green) cone
        sim = float3x3(0.330, 0.670, 0.0,  0.330, 0.670, 0.0,  -0.028, 0.126, 0.902);
        R   = float3x3(0.0, 0.0, 0.0,  0.7, 1.0, 0.0,  0.7, 0.0, 1.0);
    } else if (uMode == 2) {  // protanopia — no long-wave (red) cone
        sim = float3x3(0.170, 0.830, 0.0,  0.170, 0.830, 0.0,  -0.004, 0.024, 0.980);
        R   = float3x3(0.0, 0.0, 0.0,  0.7, 1.0, 0.0,  0.7, 0.0, 1.0);
    } else if (uMode == 3) {  // tritanopia — no short-wave (blue) cone
        sim = float3x3(1.255, -0.077, -0.178,  -0.078, 0.931, 0.148,  0.005, 0.691, 0.304);
        // Blue-yellow loss: push the error into RED and GREEN, which this viewer CAN separate.
        R   = float3x3(1.0, 0.0, 0.7,  0.0, 1.0, 0.7,  0.0, 0.0, 0.0);
    } else {
        return float4(src, 1.0);
    }

    float3 lin  = toLinear(src);
    float3 sm   = mul(sim, lin);
    float3 err  = lin - sm;
    float3 fixd = lin + mul(R, err);

    // Preserve luminance: scale the corrected colour back onto the source's brightness. This is what
    // lets the correction run at full strength without the image drifting lighter or washing out.
    float lSrc = luma(lin);
    float lFix = luma(fixd);
    fixd *= (lFix > 1e-5) ? (lSrc / lFix) : 1.0;

    // Clipping a channel would silently re-mute the very separation we just created, so pull the
    // whole colour back along the line to its own luminance until it fits the display range.
    float peak = max(max(fixd.r, fixd.g), fixd.b);
    if (peak > 1.0) {
        fixd = lerp(lSrc.xxx, fixd, saturate((1.0 - lSrc) / max(peak - lSrc, 1e-5)));
    }

    float3 outLin = lerp(lin, max(fixd, 0.0), saturate(uStrength));
    return float4(saturate(toSrgb(outLin)), 1.0);
}
";

    // Render-thread-only resources: built and used exclusively inside the imgui
    // RawCallback, which always runs on the game's render thread. Never touched from
    // any other thread — so the `static mut` access (mirroring intro_player's PIPE) is
    // sound. The COM handles here are not Send, which is why this is a render-thread
    // static rather than a global Mutex.
    struct Res {
        vs: ID3D11VertexShader,
        ps: ID3D11PixelShader,
        sampler: ID3D11SamplerState,
        cbuf: ID3D11Buffer,
        blend: ID3D11BlendState,
        raster: ID3D11RasterizerState,
        // Private snapshot of the back buffer (bindable as an SRV) + its view. Recreated
        // whenever the back-buffer size or format changes.
        copy: Option<ID3D11Texture2D>,
        copy_srv: Option<ID3D11ShaderResourceView>,
        cw: u32,
        ch: u32,
        cfmt: DXGI_FORMAT,
    }

    static mut RES: Option<Res> = None;

    #[repr(C)]
    struct Cb {
        mode: i32,
        strength: f32,
        _pad: [f32; 2],
    }

    unsafe fn compile(
        src: &str,
        entry: windows::core::PCSTR,
        target: windows::core::PCSTR,
    ) -> Option<Vec<u8>> {
        let mut blob: Option<windows::Win32::Graphics::Direct3D::ID3DBlob> = None;
        let mut err: Option<windows::Win32::Graphics::Direct3D::ID3DBlob> = None;
        let hr = D3DCompile(
            src.as_ptr() as *const c_void,
            src.len(),
            None,
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut blob,
            Some(&mut err),
        );
        if hr.is_err() {
            log(&format!("[cvd] shader compile failed: {hr:?}"));
            return None;
        }
        let blob = blob?;
        let ptr = blob.GetBufferPointer() as *const u8;
        let len = blob.GetBufferSize();
        Some(std::slice::from_raw_parts(ptr, len).to_vec())
    }

    unsafe fn build(dev: &ID3D11Device) -> Option<Res> {
        let vs_bc = compile(VS_SRC, s!("main"), s!("vs_5_0"))?;
        let ps_bc = compile(PS_SRC, s!("main"), s!("ps_5_0"))?;

        let mut vs: Option<ID3D11VertexShader> = None;
        dev.CreateVertexShader(&vs_bc, None, Some(&mut vs)).ok()?;
        let mut ps: Option<ID3D11PixelShader> = None;
        dev.CreatePixelShader(&ps_bc, None, Some(&mut ps)).ok()?;

        let samp_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_ALWAYS,
            MaxLOD: D3D11_FLOAT32_MAX,
            ..Default::default()
        };
        let mut sampler: Option<ID3D11SamplerState> = None;
        dev.CreateSamplerState(&samp_desc, Some(&mut sampler)).ok()?;

        // 16-byte constant buffer {int mode; float3 pad}, rewritten each frame.
        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let mut cbuf: Option<ID3D11Buffer> = None;
        dev.CreateBuffer(&cb_desc, None, Some(&mut cbuf)).ok()?;

        // Opaque: we fully replace the back-buffer pixels, alpha forced to 1.
        let mut blend_desc = D3D11_BLEND_DESC::default();
        blend_desc.RenderTarget[0].BlendEnable = false.into();
        blend_desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let mut blend: Option<ID3D11BlendState> = None;
        dev.CreateBlendState(&blend_desc, Some(&mut blend)).ok()?;

        // No culling, scissor OFF so the imgui clip rect can't clip our fullscreen draw.
        let rast_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ScissorEnable: false.into(),
            ..Default::default()
        };
        let mut raster: Option<ID3D11RasterizerState> = None;
        dev.CreateRasterizerState(&rast_desc, Some(&mut raster)).ok()?;

        log("[cvd] pipeline built");
        Some(Res {
            vs: vs?,
            ps: ps?,
            sampler: sampler?,
            cbuf: cbuf?,
            blend: blend?,
            raster: raster?,
            copy: None,
            copy_srv: None,
            cw: 0,
            ch: 0,
            cfmt: DXGI_FORMAT(0),
        })
    }

    /// One frame of the filter. Returns None on ANY failure (caller then permanently
    /// disables). Never unwraps/panics. Render-thread-only (RES is render-thread-owned).
    #[allow(static_mut_refs)]
    pub unsafe fn run(mode: u8) -> Option<()> {
        let dev = crate::intro_player::device()?;
        let ctx = crate::intro_player::context()?;

        if RES.is_none() {
            RES = Some(build(&dev)?);
        }
        let res = RES.as_mut()?;

        // The back-buffer RTV hudhook has bound for this frame (owned ref → released on
        // drop). None means nothing is bound — bail without disabling permanently.
        let mut rtvs: [Option<ID3D11RenderTargetView>; 1] = [None];
        ctx.OMGetRenderTargets(Some(&mut rtvs), None);
        let rtv = rtvs[0].take()?;

        let bb_res = rtv.GetResource().ok()?;
        let bb_tex: ID3D11Texture2D = bb_res.cast().ok()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        bb_tex.GetDesc(&mut desc);
        if desc.Width == 0 || desc.Height == 0 {
            return None;
        }

        // (Re)create the private copy target if the back-buffer size/format changed.
        if res.copy.is_none()
            || res.cw != desc.Width
            || res.ch != desc.Height
            || res.cfmt.0 != desc.Format.0
        {
            let cdesc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            dev.CreateTexture2D(&cdesc, None, Some(&mut tex)).ok()?;
            let tex = tex?;
            let mut srv: Option<ID3D11ShaderResourceView> = None;
            dev.CreateShaderResourceView(&tex, None, Some(&mut srv)).ok()?;
            res.copy = Some(tex);
            res.copy_srv = srv;
            res.cw = desc.Width;
            res.ch = desc.Height;
            res.cfmt = desc.Format;
        }
        let copy = res.copy.as_ref()?;
        let copy_srv = res.copy_srv.as_ref()?;

        // Snapshot the finished game frame, then read it back through the shader.
        ctx.CopyResource(copy, &bb_res);

        let cb = Cb {
            mode: mode as i32,
            strength: super::strength(),
            _pad: [0.0; 2],
        };
        ctx.UpdateSubresource(&res.cbuf, 0, None, &cb as *const Cb as *const c_void, 0, 0);

        // Bind our fullscreen pass onto the SAME back-buffer RTV (no depth). We rebind
        // shaders/topology/viewport/blend/raster; the trailing ResetRenderState (queued
        // by enqueue()) hands the pipeline back to hudhook for the overlay.
        let vp = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: desc.Width as f32,
            Height: desc.Height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        ctx.RSSetViewports(Some(&[vp]));
        ctx.RSSetState(&res.raster);
        ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
        ctx.OMSetBlendState(&res.blend, Some(&[0.0; 4]), 0xffffffff);
        ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        ctx.VSSetShader(&res.vs, Some(&[]));
        ctx.PSSetShader(&res.ps, Some(&[]));
        ctx.PSSetSamplers(0, Some(&[Some(res.sampler.clone())]));
        ctx.PSSetConstantBuffers(0, Some(&[Some(res.cbuf.clone())]));
        ctx.PSSetShaderResources(0, Some(&[Some(copy_srv.clone())]));
        ctx.Draw(3, 0);

        // Don't leave our copy SRV bound to t0 — imgui/the game render there next.
        ctx.PSSetShaderResources(0, Some(&[None]));
        Some(())
    }
}
