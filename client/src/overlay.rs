//! The in-game overlay, drawn into the game's back buffer from the `IDirect3DDevice9::Present` detour.
//!
//! * [`present_of`]: the Present in the vtable of the game's own device (the RenderSystem's
//!   `IDirect3DDevice9`, `mercs2_formats::profiler_sites`), which the caller detours.
//! * [`Atlas::rasterize`]: printable ASCII in GDI's stock fixed-pitch font, once, into an alpha mask laid out
//!   by [`crate::atlas::Layout`].
//! * [`Overlay::draw`]: a translucent panel and the text over it, with `DrawPrimitiveUP` and pre-transformed
//!   vertices. The game's state is captured in a state block first and applied after; the render target,
//!   which a state block does not hold, is saved and restored by hand.

use crate::atlas::{self, FIRST, LAST, Layout, Vertex};
use core::ffi::c_void;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D9::*;
use windows::Win32::Graphics::Gdi::*;
use windows::core::{HRESULT, Interface};

/// `IDirect3DDevice9::Present`.
pub type PresentFn = unsafe extern "system" fn(*mut c_void, *const RECT, *const RECT, HWND, *const RGNDATA) -> HRESULT;

const FVF: u32 = D3DFVF_XYZRHW | D3DFVF_DIFFUSE | D3DFVF_TEX1;
/// Panel and text placement, in back-buffer pixels.
const MARGIN: f32 = 12.0;
const PADDING: f32 = 6.0;
/// Each glyph cell is drawn this many times its rasterized size.
const SCALE: f32 = 2.0;
const PANEL_COLOR: u32 = 0xB000_0000;

/// The Present in `device`'s vtable.
///
/// # Safety
/// `device` is a live `IDirect3DDevice9*`.
pub unsafe fn present_of(device: *mut c_void) -> Result<*mut c_void, String> {
    let dev = unsafe { IDirect3DDevice9::from_raw_borrowed(&device) }.ok_or("the device pointer is null")?;
    Ok(dev.vtable().Present as *mut c_void)
}

/// The glyph atlas as an alpha mask, one byte per texel, `tex_w` × `tex_h`.
pub struct Atlas {
    pub layout: Layout,
    pub alpha: Vec<u8>,
}

impl Atlas {
    /// Printable ASCII in GDI's stock fixed-pitch font (`ANSI_FIXED_FONT`).
    pub fn rasterize() -> Result<Self, String> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err("CreateCompatibleDC failed".into());
            }
            let result = (|| {
                SelectObject(dc, GetStockObject(ANSI_FIXED_FONT));
                let mut tm = TEXTMETRICA::default();
                if !GetTextMetricsA(dc, &mut tm).as_bool() {
                    return Err("GetTextMetricsA failed for ANSI_FIXED_FONT".to_string());
                }
                let layout = Layout::new(tm.tmAveCharWidth as u32, tm.tmHeight as u32)?;
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: layout.tex_w as i32,
                        // Negative: rows top-down.
                        biHeight: -(layout.tex_h as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits: *mut c_void = core::ptr::null_mut();
                let dib = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                    .map_err(|e| format!("CreateDIBSection for the glyph atlas: {e}"))?;
                SelectObject(dc, dib.into());
                SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
                SetBkColor(dc, windows::Win32::Foundation::COLORREF(0));
                SetBkMode(dc, OPAQUE);
                for c in FIRST..=LAST {
                    let x = layout.cell_x(Layout::cell(c)) as i32;
                    if !TextOutA(dc, x, 0, &[c]).as_bool() {
                        let _ = DeleteObject(dib.into());
                        return Err(format!("TextOutA failed for glyph {:?}", c as char));
                    }
                }
                let _ = GdiFlush();
                let texels = (layout.tex_w * layout.tex_h) as usize;
                let rgb = core::slice::from_raw_parts(bits as *const u32, texels);
                // White text on black: any channel is the coverage.
                let alpha = rgb.iter().map(|p| (p >> 16) as u8).collect();
                let _ = DeleteObject(dib.into());
                Ok(Atlas { layout, alpha })
            })();
            let _ = DeleteDC(dc);
            result
        }
    }
}

/// The overlay's device resources; made again when the game's device changes. COM interfaces are not
/// `Send`: an `Overlay` lives on the thread that calls Present.
pub struct Overlay {
    atlas: &'static Atlas,
    device: usize,
    texture: Option<IDirect3DTexture9>,
}

impl Overlay {
    pub fn new(atlas: &'static Atlas) -> Self {
        Overlay { atlas, device: 0, texture: None }
    }

    /// The atlas texture on `dev`, made on first use and when the device changes. Managed pool: it
    /// survives a device reset.
    unsafe fn texture(&mut self, dev: &IDirect3DDevice9) -> Result<IDirect3DTexture9, String> {
        let key = dev.as_raw() as usize;
        if self.device == key {
            if let Some(t) = &self.texture {
                return Ok(t.clone());
            }
        }
        let l = self.atlas.layout;
        let mut tex: Option<IDirect3DTexture9> = None;
        unsafe {
            dev.CreateTexture(l.tex_w, l.tex_h, 1, 0, D3DFMT_A8R8G8B8, D3DPOOL_MANAGED, &mut tex, core::ptr::null_mut())
                .map_err(|e| format!("creating the {}x{} glyph texture: {e}", l.tex_w, l.tex_h))?;
            let tex = tex.ok_or("CreateTexture succeeded without a texture")?;
            let mut locked = D3DLOCKED_RECT::default();
            tex.LockRect(0, &mut locked, core::ptr::null(), 0).map_err(|e| format!("locking the glyph texture: {e}"))?;
            for y in 0..l.tex_h as usize {
                let row = (locked.pBits as *mut u8).add(y * locked.Pitch as usize) as *mut u32;
                for x in 0..l.tex_w as usize {
                    let a = u32::from(self.atlas.alpha[y * l.tex_w as usize + x]);
                    *row.add(x) = (a << 24) | 0x00FF_FFFF;
                }
            }
            tex.UnlockRect(0).map_err(|e| format!("unlocking the glyph texture: {e}"))?;
            self.device = key;
            self.texture = Some(tex.clone());
            Ok(tex)
        }
    }

    /// Draw `lines`, each in its colour, in a panel at the top-left of `device`'s back buffer, leaving the
    /// game's state as it was.
    ///
    /// # Safety
    /// `device` is the `this` of an `IDirect3DDevice9::Present` call.
    pub unsafe fn draw(&mut self, device: *mut c_void, lines: &[(String, u32)]) -> Result<(), String> {
        let dev = unsafe { IDirect3DDevice9::from_raw_borrowed(&device) }.ok_or("Present was called on a null device")?;
        let texture = unsafe { self.texture(dev)? };
        let l = self.atlas.layout;
        let line_h = l.cell_h as f32 * SCALE;
        let tw = lines.iter().map(|(t, _)| l.extent(t, SCALE).0).fold(0.0, f32::max);
        let th = line_h * lines.len() as f32;
        let (x, y) = (MARGIN + PADDING, MARGIN + PADDING);
        let panel = atlas::quad(MARGIN, MARGIN, x + tw + PADDING, y + th + PADDING, PANEL_COLOR, (0.0, 0.0, 0.0, 0.0));
        let glyphs: Vec<Vertex> =
            lines.iter().enumerate().flat_map(|(i, (t, c))| l.text(t, x, y + i as f32 * line_h, SCALE, *c)).collect();

        unsafe {
            let saved = dev.CreateStateBlock(D3DSBT_ALL).map_err(|e| format!("capturing the game's state: {e}"))?;
            let old_target = dev.GetRenderTarget(0).map_err(|e| format!("reading render target 0: {e}"))?;
            let back = dev.GetBackBuffer(0, 0, D3DBACKBUFFER_TYPE_MONO).map_err(|e| format!("reading the back buffer: {e}"))?;
            let drawn = (|| {
                dev.SetRenderTarget(0, &back).map_err(|e| format!("binding the back buffer: {e}"))?;
                dev.BeginScene().map_err(|e| format!("BeginScene: {e}"))?;
                let r = Self::draw_scene(dev, &texture, &panel, &glyphs);
                dev.EndScene().map_err(|e| format!("EndScene: {e}"))?;
                r
            })();
            dev.SetRenderTarget(0, &old_target).map_err(|e| format!("restoring render target 0: {e}"))?;
            saved.Apply().map_err(|e| format!("restoring the game's state: {e}"))?;
            drawn
        }
    }

    unsafe fn draw_scene(dev: &IDirect3DDevice9, texture: &IDirect3DTexture9, panel: &[Vertex], glyphs: &[Vertex]) -> Result<(), String> {
        let e = |what: &'static str| move |err: windows::core::Error| format!("{what}: {err}");
        unsafe {
            dev.SetVertexShader(None).map_err(e("SetVertexShader"))?;
            dev.SetPixelShader(None).map_err(e("SetPixelShader"))?;
            dev.SetFVF(FVF).map_err(e("SetFVF"))?;
            for (state, value) in [
                (D3DRS_ZENABLE, D3DZB_FALSE.0 as u32),
                (D3DRS_ZWRITEENABLE, 0),
                (D3DRS_ALPHATESTENABLE, 0),
                (D3DRS_ALPHABLENDENABLE, 1),
                (D3DRS_SRCBLEND, D3DBLEND_SRCALPHA.0 as u32),
                (D3DRS_DESTBLEND, D3DBLEND_INVSRCALPHA.0 as u32),
                (D3DRS_BLENDOP, D3DBLENDOP_ADD.0 as u32),
                (D3DRS_CULLMODE, D3DCULL_NONE.0 as u32),
                (D3DRS_FILLMODE, D3DFILL_SOLID.0 as u32),
                (D3DRS_LIGHTING, 0),
                (D3DRS_FOGENABLE, 0),
                (D3DRS_STENCILENABLE, 0),
                (D3DRS_SCISSORTESTENABLE, 0),
                (D3DRS_CLIPPING, 1),
                (D3DRS_COLORWRITEENABLE, 0xF),
                (D3DRS_SRGBWRITEENABLE, 0),
            ] {
                dev.SetRenderState(state, value).map_err(e("SetRenderState"))?;
            }
            dev.SetTextureStageState(1, D3DTSS_COLOROP, D3DTOP_DISABLE.0 as u32).map_err(e("SetTextureStageState"))?;
            dev.SetTextureStageState(1, D3DTSS_ALPHAOP, D3DTOP_DISABLE.0 as u32).map_err(e("SetTextureStageState"))?;

            // The panel: vertex colour only.
            dev.SetTexture(0, None).map_err(e("SetTexture"))?;
            for (state, value) in [
                (D3DTSS_COLOROP, D3DTOP_SELECTARG2.0 as u32),
                (D3DTSS_COLORARG2, D3DTA_DIFFUSE),
                (D3DTSS_ALPHAOP, D3DTOP_SELECTARG2.0 as u32),
                (D3DTSS_ALPHAARG2, D3DTA_DIFFUSE),
            ] {
                dev.SetTextureStageState(0, state, value).map_err(e("SetTextureStageState"))?;
            }
            Self::triangles(dev, panel)?;

            // The text: the atlas's coverage times the vertex colour.
            dev.SetTexture(0, texture).map_err(e("SetTexture"))?;
            for (state, value) in [
                (D3DTSS_COLOROP, D3DTOP_MODULATE.0 as u32),
                (D3DTSS_COLORARG1, D3DTA_TEXTURE),
                (D3DTSS_COLORARG2, D3DTA_DIFFUSE),
                (D3DTSS_ALPHAOP, D3DTOP_MODULATE.0 as u32),
                (D3DTSS_ALPHAARG1, D3DTA_TEXTURE),
                (D3DTSS_ALPHAARG2, D3DTA_DIFFUSE),
            ] {
                dev.SetTextureStageState(0, state, value).map_err(e("SetTextureStageState"))?;
            }
            for (state, value) in [
                (D3DSAMP_MINFILTER, D3DTEXF_POINT.0 as u32),
                (D3DSAMP_MAGFILTER, D3DTEXF_POINT.0 as u32),
                (D3DSAMP_MIPFILTER, D3DTEXF_NONE.0 as u32),
                (D3DSAMP_ADDRESSU, D3DTADDRESS_CLAMP.0 as u32),
                (D3DSAMP_ADDRESSV, D3DTADDRESS_CLAMP.0 as u32),
            ] {
                dev.SetSamplerState(0, state, value).map_err(e("SetSamplerState"))?;
            }
            Self::triangles(dev, glyphs)
        }
    }

    unsafe fn triangles(dev: &IDirect3DDevice9, v: &[Vertex]) -> Result<(), String> {
        if v.is_empty() {
            return Ok(());
        }
        unsafe {
            dev.DrawPrimitiveUP(D3DPT_TRIANGLELIST, (v.len() / 3) as u32, v.as_ptr().cast(), size_of::<Vertex>() as u32)
                .map_err(|e| format!("DrawPrimitiveUP: {e}"))
        }
    }
}
