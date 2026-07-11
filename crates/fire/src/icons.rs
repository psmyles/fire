//! Toolbar icons: build-time-rasterized SVG coverage masks, tinted and blitted with GDI.
//!
//! `build.rs` rasterizes each `assets/icons/*.svg` to a square **A8 coverage mask** at
//! [`MASTER`]² px (white-on-transparent → the alpha channel is the coverage) and drops it in
//! `OUT_DIR`; we embed those masters via `include_bytes!`. At runtime [`Icons`] downsamples each
//! master to the exact physical icon size for the current DPI (once, cached) so icons stay crisp
//! at any scale without shipping an SVG rasterizer. Each paint, [`Icons::draw`] premultiplies the
//! mask by the button's text color into a scratch DIB and `AlphaBlend`s it — so an icon picks up
//! the same light/dark/hover/active/disabled tint the old text labels did (see [`crate::chrome`]).
//!
//! This is chrome, off the time-to-first-pixel path: the masters live in `.rodata`, the per-DPI
//! downsample runs once on theme/DPI setup, and a draw is a small CPU fill + one blit on a chrome
//! repaint (never per frame).

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ,
};

/// Master rasterization size of each embedded icon (px). Must match `build.rs`'s `ICON_MASTER`;
/// each `.a8` master is exactly `MASTER * MASTER` bytes.
const MASTER: usize = 64;

/// A toolbar icon. The order matches `build.rs`'s `ICON_STEMS` (and [`MASTERS`]) so `icon as usize`
/// indexes both the embedded master and the per-DPI mask cache. Several buttons share an icon
/// (e.g. the blue-channel and black-background buttons both use [`Icon::B`]); the chrome maps each
/// action to one of these.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Left,
    Right,
    ZoomOut,
    ZoomIn,
    Fit,
    OneToOne,
    Rgb,
    Rgba,
    R,
    G,
    B,
    A,
    Aces,
    EvUp,
    EvReset,
    EvDown,
    White,
    Checker,
    Outline,
    OpenWith,
    Fullscreen,
    Flipbook,
    Play,
    Pause,
    More,
}

/// The embedded A8 masters, indexed by `Icon as usize`. Same order as the enum / `build.rs`.
const MASTERS: [&[u8]; 25] = [
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_left.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_right.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_zoom_out.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_zoom_in.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_fit.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_1_1.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_RGB.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_rgba.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_R.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_G.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_B.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_A.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_aces.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_ev+.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_ev0.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_ev-.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_W.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_C.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_outline.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_open_with.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_fullscreen.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_flipbook.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_play.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_pause.a8")),
    include_bytes!(concat!(env!("OUT_DIR"), "/icon_more.a8")),
];

/// Compile-time guard that [`Icon`] and [`MASTERS`] stay the same length: [`Icons::draw`] indexes
/// `masks`/`MASTERS` by `icon as usize`, so a variant added without a corresponding master (or
/// vice-versa) must fail the build rather than panic in a paint. `More` must stay the last
/// `Icon` variant for this check to hold.
const _: () = assert!(Icon::More as usize + 1 == MASTERS.len());

/// Per-DPI icon renderer: the downsampled masks for the current physical icon size plus a scratch
/// 32-bit top-down DIB (with its memory DC) that [`Self::draw`] fills with a tinted, premultiplied
/// copy of one mask and `AlphaBlend`s onto the toolbar. Rebuilt on DPI change via [`Self::set_size`].
pub struct Icons {
    /// Current physical icon edge (px); also the scratch DIB edge.
    icon_px: i32,
    /// One downsampled `icon_px²` coverage mask per [`Icon`], indexed by `icon as usize`.
    masks: Vec<Vec<u8>>,
    /// Memory DC with `bmp` selected; the `AlphaBlend` source.
    dc: HDC,
    bmp: HBITMAP,
    /// The DIB's pixel memory (`icon_px²` BGRA, top-down); rewritten per `draw`.
    bits: *mut u8,
    /// The DC's prior bitmap, restored before deleting `bmp` (GDI hygiene).
    old: HGDIOBJ,
}

impl Icons {
    /// Build the per-DPI masks and the scratch DIB for a physical icon edge of `icon_px`.
    pub fn new(icon_px: i32) -> Self {
        let n = icon_px.max(1);
        let masks: Vec<Vec<u8>> = MASTERS.iter().map(|m| downsample(m, n as usize)).collect();

        let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = n;
        bmi.bmiHeader.biHeight = -n; // negative → top-down, so row 0 is the mask's top row
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut bits: *mut c_void = ptr::null_mut();
        let (dc, bmp, old) = unsafe {
            let bmp = CreateDIBSection(
                ptr::null_mut(),
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                ptr::null_mut(),
                0,
            );
            let dc = CreateCompatibleDC(ptr::null_mut());
            let old = SelectObject(dc, bmp as HGDIOBJ);
            (dc, bmp, old)
        };

        Icons {
            icon_px: n,
            masks,
            dc,
            bmp,
            bits: bits as *mut u8,
            old,
        }
    }

    /// Rebuild for a new physical icon size (after a DPI change). No-op if unchanged; otherwise
    /// reassigning drops the old GDI resources via [`Drop`] before the new ones move in.
    pub fn set_size(&mut self, icon_px: i32) {
        let n = icon_px.max(1);
        if n != self.icon_px {
            *self = Icons::new(n);
        }
    }

    /// Tint `icon`'s coverage mask with `color` (a GDI `COLORREF`, `0x00BBGGRR`) and blit it
    /// centered on (`cx`, `cy`) in `hdc`. Anti-aliased via per-pixel alpha (`AC_SRC_ALPHA`), so
    /// edges blend into whatever the toolbar already painted (bg / hover / active fill).
    pub fn draw(&self, hdc: HDC, icon: Icon, cx: i32, cy: i32, color: u32) {
        let n = self.icon_px;
        let mask = &self.masks[icon as usize];
        let (r, g, b) = (color & 0xff, (color >> 8) & 0xff, (color >> 16) & 0xff);
        // Premultiply the tint by coverage into the scratch DIB (BGRA byte order, top-down).
        for (i, &cov) in mask.iter().enumerate() {
            let a = cov as u32;
            unsafe {
                *self.bits.add(i * 4) = (b * a / 255) as u8;
                *self.bits.add(i * 4 + 1) = (g * a / 255) as u8;
                *self.bits.add(i * 4 + 2) = (r * a / 255) as u8;
                *self.bits.add(i * 4 + 3) = a as u8;
            }
        }
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            AlphaBlend(
                hdc,
                cx - n / 2,
                cy - n / 2,
                n,
                n,
                self.dc,
                0,
                0,
                n,
                n,
                blend,
            );
        }
    }
}

impl Drop for Icons {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp as HGDIOBJ);
            DeleteDC(self.dc);
        }
    }
}

/// Box-downsample a `MASTER²` coverage mask to `dst²` by averaging each destination pixel's source
/// footprint — effectively supersampled antialiasing of the master raster down to the icon size.
/// (If `dst > MASTER` the footprints collapse to single texels, i.e. nearest-neighbor upsample;
/// only the very highest DPIs reach that, and the master is sized so it rarely happens.)
fn downsample(master: &[u8], dst: usize) -> Vec<u8> {
    debug_assert_eq!(master.len(), MASTER * MASTER);
    let mut out = vec![0u8; dst * dst];
    let scale = MASTER as f32 / dst as f32;
    for ty in 0..dst {
        let sy0 = (ty as f32 * scale) as usize;
        let sy1 = (((ty + 1) as f32 * scale).ceil() as usize).clamp(sy0 + 1, MASTER);
        for tx in 0..dst {
            let sx0 = (tx as f32 * scale) as usize;
            let sx1 = (((tx + 1) as f32 * scale).ceil() as usize).clamp(sx0 + 1, MASTER);
            let mut sum = 0u32;
            let mut count = 0u32;
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    sum += master[sy * MASTER + sx] as u32;
                    count += 1;
                }
            }
            out[ty * dst + tx] = (sum / count.max(1)) as u8;
        }
    }
    out
}
