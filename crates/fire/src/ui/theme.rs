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

/// The **settings window's** style: fire's palette and the user's accent, on ImGui's form geometry.
///
/// Deliberately not [`apply`]. That one styles a *toolbar* — buttons transparent until touched, tight
/// spacing, no field frames, because it sits over an image and must not compete with it. A dialog
/// that inherited it would have invisible buttons and inputs you can't see the edges of. So the
/// settings window starts from ImGui's factory *shape* (see [`crate::render::imgui::FormStyle`]) and
/// gets the app's *color* here — same greys, same system accent, arranged for a form.
pub fn form(style: &mut Style, dark: bool, scale: f32) {
    let p = Palette::for_mode(dark);
    let s = |v: f32| (v * scale).round();

    let text = col(p.text, 1.0);
    let dim = col(p.text_dim, 1.0);
    let accent = col(p.btn_active, 1.0);
    let surface = col(p.toolbar_bg, 1.0);
    let sunken = col(p.status_bg, 1.0);
    let hover = col(p.btn_hover, 1.0);
    // An input is a hole in the surface, not a raised chip — so it goes *below* the window, not above.
    let field = if dark {
        [0.10, 0.10, 0.10, 1.0]
    } else {
        [1.0, 1.0, 1.0, 1.0]
    };

    // Geometry: a form, but a tight one. The complaint that started this migration was that the old
    // dialog was too airy and its tab strip too tall — so this stays close-packed.
    style.set_window_padding([s(12.0), s(10.0)]);
    style.set_frame_padding([s(8.0), s(5.0)]);
    style.set_item_spacing([s(8.0), s(6.0)]);
    style.set_item_inner_spacing([s(6.0), s(4.0)]);
    style.set_indent_spacing(s(16.0));
    style.set_scrollbar_size(s(13.0));
    style.set_grab_min_size(s(12.0));

    style.set_window_rounding(s(6.0));
    style.set_child_rounding(s(4.0));
    style.set_frame_rounding(s(4.0));
    style.set_popup_rounding(s(4.0));
    style.set_grab_rounding(s(4.0));
    style.set_scrollbar_rounding(s(4.0));
    style.set_tab_rounding(s(4.0));
    style.set_window_border_size(1.0);
    style.set_child_border_size(1.0);
    style.set_popup_border_size(1.0);
    style.set_frame_border_size(0.0);

    style.set_color(StyleColor::Text, text);
    style.set_color(StyleColor::TextDisabled, dim);
    style.set_color(StyleColor::WindowBg, surface);
    style.set_color(StyleColor::PopupBg, surface);
    style.set_color(StyleColor::ChildBg, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::Border, col(p.border, 1.0));
    style.set_color(StyleColor::BorderShadow, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::Separator, col(p.separator, 1.0));

    // The title bar reads as part of the window, not as a blue band across the top of it.
    style.set_color(StyleColor::TitleBg, sunken);
    style.set_color(StyleColor::TitleBgActive, sunken);
    style.set_color(StyleColor::TitleBgCollapsed, sunken);

    style.set_color(StyleColor::FrameBg, field);
    style.set_color(StyleColor::FrameBgHovered, lift(field, 0.06));
    style.set_color(StyleColor::FrameBgActive, lift(field, 0.10));

    // Buttons here *are* visible (unlike the toolbar's): a dialog's OK is a thing you press.
    style.set_color(StyleColor::Button, hover);
    style.set_color(StyleColor::ButtonHovered, lift(hover, 0.08));
    style.set_color(StyleColor::ButtonActive, accent);

    // Everything that says "this one is chosen" is the user's accent — the same one the toolbar
    // latches with, so the two windows are recognisably the same app.
    style.set_color(StyleColor::CheckMark, on_accent(accent));
    style.set_color(StyleColor::CheckboxSelectedBg, accent);
    style.set_color(StyleColor::SliderGrab, accent);
    style.set_color(StyleColor::SliderGrabActive, lift(accent, 0.10));
    style.set_color(StyleColor::Header, accent);
    style.set_color(StyleColor::HeaderHovered, hover);
    style.set_color(StyleColor::HeaderActive, accent);
    style.set_color(StyleColor::NavCursor, accent);
    style.set_color(StyleColor::TextSelectedBg, [accent[0], accent[1], accent[2], 0.45]);
    style.set_color(StyleColor::InputTextCursor, text);

    // The selected tab is the *raised* one, with the accent ruled along its top edge; the rest are
    // bare text on the window. ImGui's default is the reverse — an unselected tab gets a fill and the
    // selected one is left to blend into the page behind it, which reads as "this tab is disabled and
    // those are buttons".
    style.set_color(StyleColor::Tab, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::TabHovered, hover);
    style.set_color(StyleColor::TabSelected, hover);
    style.set_color(StyleColor::TabSelectedOverline, accent);
    style.set_color(StyleColor::TabDimmed, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::TabDimmedSelected, hover);
    style.set_color(StyleColor::TabDimmedSelectedOverline, col(p.separator, 1.0));
    style.set_tab_bar_overline_size(s(2.0));

    style.set_color(StyleColor::ScrollbarBg, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::ScrollbarGrab, col(p.separator, 1.0));
    style.set_color(StyleColor::ScrollbarGrabHovered, hover);
    style.set_color(StyleColor::ScrollbarGrabActive, accent);

    style.set_color(StyleColor::ResizeGrip, [0.0, 0.0, 0.0, 0.0]);
    style.set_color(StyleColor::ResizeGripHovered, hover);
    style.set_color(StyleColor::ResizeGripActive, accent);
}

/// Nudge a color toward white (or, for an already-bright one, toward black) — the "one step more
/// prominent" a hover needs, without a second hand-picked color per token.
fn lift(c: [f32; 4], amount: f32) -> [f32; 4] {
    let target = if luminance(c) > 0.5 { -1.0 } else { 1.0 };
    let f = |v: f32| (v + target * amount).clamp(0.0, 1.0);
    [f(c[0]), f(c[1]), f(c[2]), c[3]]
}

/// Black or white, whichever stays readable on `c` — a user whose system accent is a pale yellow
/// must not get white-on-yellow.
pub(crate) fn on_accent(c: [f32; 4]) -> [f32; 4] {
    if luminance(c) > 0.59 {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        [1.0, 1.0, 1.0, 1.0]
    }
}

fn luminance(c: [f32; 4]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The status bar sits on its own slightly darker fill; the caller pushes this for that window.
pub fn status_bg(dark: bool) -> [f32; 4] {
    col(Palette::for_mode(dark).status_bg, 1.0)
}

/// The chrome fill, used to clear the parts of the backbuffer the image doesn't cover.
pub fn chrome_bg(dark: bool) -> [f32; 4] {
    col(Palette::for_mode(dark).toolbar_bg, 1.0)
}
