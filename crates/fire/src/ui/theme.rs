//! The ImGui style: colors, metrics, spacing.
//!
//! Colors come from the same tokens the GDI chrome used ([`crate::chrome::Palette`]) so the app
//! looks like itself, and the highlight is still the user's **system accent**
//! ([`crate::chrome::system_accent`], which reads `COLOR_HIGHLIGHT` — documented, no registry).
//!
//! Everything here is one place. That is the entire point of the migration: spacing is a constant to
//! edit, not a layout to rewrite.

use dear_imgui_rs::{Style, StyleColor};

use crate::chrome::Palette;

/// Logical (96-dpi) chrome metrics. Scaled by DPI in [`Metrics::new`].
const TOOLBAR_H: f32 = 38.0;
const STATUS_H: f32 = 24.0;
const TRANSPORT_H: f32 = 34.0;
/// Base font size in logical px; DPI is applied by ImGui's `font_scale_dpi`.
pub const FONT_SIZE: f32 = 14.0;

/// Physical chrome metrics for the current DPI.
#[derive(Clone, Copy)]
pub struct Metrics {
    pub scale: f32,
    pub toolbar_h: f32,
    pub status_h: f32,
    pub transport_h: f32,
}

impl Metrics {
    pub fn new(dpi: u32) -> Self {
        let scale = dpi.max(96) as f32 / 96.0;
        Metrics {
            scale,
            toolbar_h: (TOOLBAR_H * scale).round(),
            status_h: (STATUS_H * scale).round(),
            transport_h: (TRANSPORT_H * scale).round(),
        }
    }
}

/// A GDI `COLORREF` (`0x00BBGGRR`) as an ImGui RGBA color.
pub fn col(c: u32, alpha: f32) -> [f32; 4] {
    [
        (c & 0xFF) as f32 / 255.0,
        ((c >> 8) & 0xFF) as f32 / 255.0,
        ((c >> 16) & 0xFF) as f32 / 255.0,
        alpha,
    ]
}

/// Horizontal padding inside a toolbar button, logical px. `ui` needs this to compute widths, so it
/// is public and [`apply`] is its single consumer — the two must not drift.
pub const FRAME_PAD_X: f32 = 7.0;
pub const FRAME_PAD_Y: f32 = 4.0;
/// Gap between adjacent toolbar buttons, logical px.
pub const ITEM_SPACING: f32 = 3.0;

/// Apply the whole style. Called at startup and again whenever the theme, accent or DPI changes.
///
/// `scale` is the DPI factor. ImGui's `font_scale_dpi` handles *glyphs* only — every metric below is
/// in logical px and must be scaled here, or the chrome stays 96-dpi-sized on a HiDPI monitor.
pub fn apply(style: &mut Style, dark: bool, scale: f32) {
    let p = Palette::for_mode(dark);
    let s = |v: f32| (v * scale).round();

    style.set_font_size_base(FONT_SIZE);
    style.set_font_scale_dpi(scale);

    // Flat and square, like the chrome it replaces — no ImGui-default rounding or borders.
    style.set_window_rounding(0.0);
    style.set_child_rounding(0.0);
    style.set_frame_rounding(s(2.0));
    style.set_popup_rounding(s(2.0));
    style.set_grab_rounding(s(2.0));
    style.set_scrollbar_rounding(s(2.0));
    style.set_tab_rounding(s(2.0));
    style.set_window_border_size(0.0);
    style.set_frame_border_size(0.0);
    style.set_image_border_size(0.0);
    style.set_popup_border_size(1.0);

    style.set_window_padding([s(8.0), s(6.0)]);
    style.set_frame_padding([s(FRAME_PAD_X), s(FRAME_PAD_Y)]);
    style.set_item_spacing([s(ITEM_SPACING), s(4.0)]);
    style.set_item_inner_spacing([s(4.0), s(4.0)]);
    style.set_scrollbar_size(s(12.0));
    style.set_grab_min_size(s(10.0));

    let text = col(p.text, 1.0);
    let dim = col(p.text_dim, 1.0);
    let accent = col(p.btn_active, 1.0);
    let hover = col(p.btn_hover, 1.0);

    style.set_color(StyleColor::Text, text);
    style.set_color(StyleColor::TextDisabled, dim);
    style.set_color(StyleColor::WindowBg, col(p.toolbar_bg, 1.0));
    style.set_color(StyleColor::ChildBg, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::PopupBg, col(p.toolbar_bg, 1.0));
    style.set_color(StyleColor::Border, col(p.border, 1.0));
    style.set_color(StyleColor::BorderShadow, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::Separator, col(p.separator, 1.0));

    // Toolbar buttons are transparent until touched — the bar reads as one surface, not a row of
    // chips (which is exactly what ImGui's defaults would give you).
    style.set_color(StyleColor::Button, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::ButtonHovered, hover);
    style.set_color(StyleColor::ButtonActive, accent);

    style.set_color(StyleColor::FrameBg, col(p.btn_hover, 1.0));
    style.set_color(StyleColor::FrameBgHovered, hover);
    style.set_color(StyleColor::FrameBgActive, hover);

    style.set_color(StyleColor::Header, accent);
    style.set_color(StyleColor::HeaderHovered, hover);
    style.set_color(StyleColor::HeaderActive, accent);

    style.set_color(StyleColor::SliderGrab, accent);
    style.set_color(StyleColor::SliderGrabActive, accent);
    style.set_color(StyleColor::CheckMark, accent);

    style.set_color(StyleColor::ScrollbarBg, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::ScrollbarGrab, col(p.separator, 1.0));
    style.set_color(StyleColor::ScrollbarGrabHovered, hover);
    style.set_color(StyleColor::ScrollbarGrabActive, accent);
}

/// The status bar sits on its own slightly darker fill; the caller pushes this for that window.
pub fn status_bg(dark: bool) -> [f32; 4] {
    col(Palette::for_mode(dark).status_bg, 1.0)
}

/// The chrome fill, used to clear the parts of the backbuffer the image doesn't cover.
pub fn chrome_bg(dark: bool) -> [f32; 4] {
    col(Palette::for_mode(dark).toolbar_bg, 1.0)
}
