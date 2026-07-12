//! The whole UI, rebuilt every frame in immediate mode: toolbar, status bar, flipbook transport,
//! and the empty-state hint.
//!
//! This module is **pure UI**: no Win32, no COM, no GDI. It reads a [`ViewSnapshot`] (and, in
//! flipbook mode, a [`TransportSnapshot`]) and returns a [`Frame`] describing what the user asked
//! for. The win shell applies it. That separation is why the layout can be reasoned about at all —
//! and why scrolling, hit-testing, focus and hover are ImGui's problem now, not ours.
//!
//! The one thing still owned by Win32 is the two *popup menus* (Open-with, overflow): they remain
//! `TrackPopupMenu` for now, so this module only reports where to anchor them. They become ImGui
//! popups in a later phase, which is what finally retires the undocumented uxtheme ordinals.

pub mod theme;

use dear_imgui_rs::{Condition, StyleColor, TextureId, Ui, WindowFlags};

use crate::chrome::{Action, ViewSnapshot};
use crate::icons::Icon;
use crate::flipbook::Grid;
use crate::transport::{TransportEdit, TransportSnapshot};
use theme::Metrics;

/// Bars are fixed panes, not windows the user can move, resize, collapse or scroll.
const BAR: WindowFlags = WindowFlags::from_bits_truncate(
    WindowFlags::NO_TITLE_BAR.bits()
        | WindowFlags::NO_RESIZE.bits()
        | WindowFlags::NO_MOVE.bits()
        | WindowFlags::NO_SCROLLBAR.bits()
        | WindowFlags::NO_SCROLL_WITH_MOUSE.bits()
        | WindowFlags::NO_COLLAPSE.bits()
        | WindowFlags::NO_SAVED_SETTINGS.bits()
        | WindowFlags::NO_BRING_TO_FRONT_ON_FOCUS.bits()
        | WindowFlags::NO_NAV_FOCUS.bits(),
);

/// The empty-state hint: a bar that must not eat clicks (the shell wants the double-click).
const OVERLAY: WindowFlags =
    WindowFlags::from_bits_truncate(BAR.bits() | WindowFlags::NO_MOUSE_INPUTS.bits());

/// A button that needs a Win32 popup anchored under it, in client coords.
pub struct MenuAnchor {
    pub action: Action,
    pub x: i32,
    /// Bottom edge of the button — the menu drops from here.
    pub y: i32,
}

/// What one UI frame produced.
#[derive(Default)]
pub struct Frame {
    pub actions: Vec<Action>,
    pub edits: Vec<TransportEdit>,
    /// The overflow menu's contents, when the "»" button was clicked (empty otherwise). The shell
    /// builds the Win32 popup from these, so a dropped button keeps its enabled/active state.
    pub overflow: Vec<Action>,
    pub menu: Option<MenuAnchor>,
    /// The flipbook hint chip: enter flipbook mode / never ask again for this image.
    pub chip_accept: bool,
    pub chip_dismiss: bool,
}

/// Left-docked slots, in order, each with the overflow priority the GDI chrome used: navigation is
/// kept longest, then zoom, then flipbook, then the all-channels reset, the channel solos, and the
/// HDR group is shed first. Higher = stays on the bar longer.
const LEFT: &[(Action, u8, u8)] = &[
    // (action, group, prio) — groups are separated by a divider.
    (Action::Prev, 0, 90),
    (Action::Next, 0, 90),
    (Action::ZoomOut, 1, 70),
    (Action::ZoomToggle, 1, 75),
    (Action::ZoomIn, 1, 70),
    (Action::Channel(Ch::Rgb), 2, 50),
    (Action::Channel(Ch::R), 2, 40),
    (Action::Channel(Ch::G), 2, 40),
    (Action::Channel(Ch::B), 2, 40),
    (Action::Channel(Ch::A), 2, 40),
    (Action::ToggleFlipbook, 4, 55),
    (Action::ToggleTonemap, 3, 20),
    (Action::ExpUp, 3, 20),
    (Action::ExpReset, 3, 15),
    (Action::ExpDown, 3, 20),
];

/// Right-docked slots, in visual left→right order. Never overflow (anchored to the far edge).
const RIGHT: &[(Action, u8)] = &[
    (Action::ToggleOutline, 0),
    (Action::Background(Bg::Black), 1),
    (Action::Background(Bg::White), 1),
    (Action::Background(Bg::Grey), 1),
    (Action::Background(Bg::Checker), 1),
    (Action::ToggleFullscreen, 3),
    (Action::OpenWithMenu, 2),
    (Action::OpenSettings, 2),
];

use crate::render::view::{Background as Bg, Channel as Ch};

/// The HDR group: laid out only for float sources.
const HDR_GROUP: u8 = 3;

/// Build the whole UI for one frame.
///
/// `client` is the window client size in physical px; `image` is the sub-rect the image occupies
/// (see `App::image_rect`).
#[allow(clippy::too_many_arguments)]
pub fn build(
    ui: &Ui,
    tex: TextureId,
    snap: &ViewSnapshot,
    transport: Option<&TransportSnapshot>,
    chip: Option<Grid>,
    m: &Metrics,
    icon_px: f32,
    dark: bool,
    client: (f32, f32),
    image: (f32, f32, f32, f32),
    fullscreen: bool,
) -> Frame {
    let mut out = Frame::default();
    let (w, h) = client;

    // Full-screen hides the chrome entirely — the image owns the monitor.
    if !fullscreen {
        toolbar(ui, tex, snap, m, icon_px, w, &mut out);
        if let Some(t) = transport {
            transport_band(ui, tex, snap, t, m, icon_px, w, h, dark, &mut out);
        }
        status_bar(ui, snap, m, dark, w, h);
        if let Some(g) = chip {
            hint_chip(ui, g, m, image, &mut out);
        }
    }

    // Empty state: no image and none loading. Purely decorative — OVERLAY takes no mouse input, so
    // the shell still sees the double-click that opens the file picker.
    if !snap.has_image && !snap.loading {
        empty_hint(ui, image);
    }

    out
}

/// The flipbook detection chip: a small card floating at the top of the image when a sprite-sheet
/// grid was detected. Used to be its own layered popup HWND; it is an ImGui panel now.
fn hint_chip(ui: &Ui, g: Grid, m: &Metrics, image: (f32, f32, f32, f32), out: &mut Frame) {
    let (ix, iy, iw, _) = image;
    if iw <= 0.0 {
        return;
    }
    let label = format!("{}\u{00d7}{} sprite sheet?", g.cols, g.rows);
    let pad = (10.0 * m.scale).round();
    let spacing = (theme::ITEM_SPACING * m.scale).round();
    let btn_pad = (theme::FRAME_PAD_X * m.scale).round() * 2.0;

    // Centered horizontally over the image. There is no position-pivot in this binding, so measure
    // the content and place the top-left ourselves: label + "Play" + "×", plus padding and spacing.
    let w = pad * 2.0
        + text_w(ui, &label)
        + spacing
        + (text_w(ui, "Play") + btn_pad)
        + spacing
        + (text_w(ui, "\u{00d7}") + btn_pad);
    let h = ui.frame_height() + pad * 2.0;
    let x = (ix + (iw - w) * 0.5).round().max(ix);
    let y = (iy + (10.0 * m.scale)).round();

    ui.window("##chip")
        .position([x, y], Condition::Always)
        .size([w, h], Condition::Always)
        .flags(BAR)
        .build(|| {
            ui.set_cursor_pos([pad, ((h - ui.text_line_height()) * 0.5).round()]);
            ui.text(&label);
            ui.same_line();
            if ui.button("Play") {
                out.chip_accept = true;
            }
            ui.same_line();
            if ui.button("\u{00d7}##chipclose") {
                out.chip_dismiss = true;
            }
        });
}

/// Physical size of one toolbar button.
fn button_size(icon_px: f32, m: &Metrics) -> [f32; 2] {
    let pad_x = (theme::FRAME_PAD_X * m.scale).round();
    let pad_y = (theme::FRAME_PAD_Y * m.scale).round();
    [icon_px + pad_x * 2.0, icon_px + pad_y * 2.0]
}

fn toolbar(
    ui: &Ui,
    tex: TextureId,
    snap: &ViewSnapshot,
    m: &Metrics,
    icon_px: f32,
    w: f32,
    out: &mut Frame,
) {
    let bs = button_size(icon_px, m);
    let spacing = (theme::ITEM_SPACING * m.scale).round();
    // A group divider: spacing, a 1px rule, spacing.
    let div_w = spacing * 2.0 + 1.0;

    // Which left slots apply at all (the HDR group is float-only)?
    let candidates: Vec<(Action, u8, u8)> = LEFT
        .iter()
        .copied()
        .filter(|(_, g, _)| *g != HDR_GROUP || snap.is_hdr)
        .collect();

    let right: Vec<(Action, u8)> = RIGHT.to_vec();
    let right_w = strip_width(&right.iter().map(|(a, g)| (*a, *g)).collect::<Vec<_>>(), bs[0], spacing, div_w);

    // Drop the lowest-priority left slots until the strip fits. Ties break toward the *right*, so a
    // group collapses from its tail inward — same rule the GDI chrome used.
    let mut kept = candidates.clone();
    let mut dropped: Vec<Action> = Vec::new();
    let edge = (8.0 * m.scale).round();
    loop {
        let more_w = if dropped.is_empty() { 0.0 } else { bs[0] + spacing };
        let left_w = strip_width(
            &kept.iter().map(|(a, g, _)| (*a, *g)).collect::<Vec<_>>(),
            bs[0],
            spacing,
            div_w,
        );
        if left_w + more_w + right_w + edge * 2.0 <= w || kept.is_empty() {
            break;
        }
        // Lowest prio wins; on a tie, the later (righter) slot goes first.
        let victim = kept
            .iter()
            .enumerate()
            .min_by_key(|(i, (_, _, p))| (*p, std::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        match victim {
            Some(i) => dropped.insert(0, kept.remove(i).0),
            None => break,
        }
    }

    let flags = BAR;
    ui.window("##toolbar")
        .position([0.0, 0.0], Condition::Always)
        .size([w, m.toolbar_h], Condition::Always)
        .flags(flags)
        .build(|| {
            let y = ((m.toolbar_h - bs[1]) * 0.5).round();
            let mut x = edge;

            // Left strip.
            let mut prev_group: Option<u8> = None;
            for (action, group, _) in &kept {
                if let Some(pg) = prev_group {
                    if pg != *group {
                        divider(ui, x + spacing, m);
                        x += div_w;
                    }
                }
                icon_button(ui, tex, *action, snap, [x, y], bs, icon_px, m, out);
                x += bs[0] + spacing;
                prev_group = Some(*group);
            }

            // The overflow "»", immediately after the left strip.
            if !dropped.is_empty() {
                if button(ui, tex, Icon::More, "##overflow", [x, y], bs, icon_px, true, false, m) {
                    out.overflow = dropped.clone();
                    out.menu = Some(MenuAnchor {
                        action: Action::Overflow,
                        x: x as i32,
                        y: (y + bs[1]) as i32,
                    });
                }
                if ui.is_item_hovered() {
                    ui.tooltip_text(snap.tooltip(Action::Overflow));
                }
            }

            // Right strip, right-aligned.
            let mut rx = w - edge - right_w;
            let mut prev_group: Option<u8> = None;
            for (action, group) in &right {
                if let Some(pg) = prev_group {
                    if pg != *group {
                        divider(ui, rx + spacing, m);
                        rx += div_w;
                    }
                }
                icon_button(ui, tex, *action, snap, [rx, y], bs, icon_px, m, out);
                rx += bs[0] + spacing;
                prev_group = Some(*group);
            }
        });
}

/// Width of a strip of buttons including the dividers between differing groups.
fn strip_width(slots: &[(Action, u8)], bw: f32, spacing: f32, div_w: f32) -> f32 {
    let mut total = 0.0;
    let mut prev: Option<u8> = None;
    for (_, g) in slots {
        if let Some(p) = prev {
            if p != *g {
                total += div_w;
            }
        }
        total += bw + spacing;
        prev = Some(*g);
    }
    total
}

/// Width of a run of text in the current font. (`calc_text_size` lives on `Font`, not `Ui`.)
fn text_w(ui: &Ui, s: &str) -> f32 {
    ui.current_font()
        .calc_text_size(ui.current_font_size(), f32::MAX, 0.0, s)[0]
}

/// The thin rule between toolbar groups. Its color is the style's `Separator` token, so it tracks
/// the theme without this module needing to know whether we're in dark mode.
fn divider(ui: &Ui, x: f32, m: &Metrics) {
    let c = ui.clone_style().color(StyleColor::Separator);
    let wp = ui.window_pos();
    let top = wp[1] + (m.toolbar_h * 0.28).round();
    let bot = wp[1] + (m.toolbar_h * 0.72).round();
    ui.get_window_draw_list()
        .add_line([wp[0] + x, top], [wp[0] + x, bot], c)
        .build();
}

#[allow(clippy::too_many_arguments)]
fn icon_button(
    ui: &Ui,
    tex: TextureId,
    action: Action,
    snap: &ViewSnapshot,
    pos: [f32; 2],
    bs: [f32; 2],
    icon_px: f32,
    m: &Metrics,
    out: &mut Frame,
) {
    let enabled = snap.enabled(action);
    let active = snap.active(action);
    let icon = snap.icon(action);
    let id = format!("##tb{}", action_id(action));

    if button(ui, tex, icon, &id, pos, bs, icon_px, enabled, active, m) {
        match action {
            // These two open Win32 popups, which need the button's rect; the shell does that.
            Action::OpenWithMenu | Action::Overflow => {
                out.menu = Some(MenuAnchor {
                    action,
                    x: pos[0] as i32,
                    y: (pos[1] + bs[1]) as i32,
                });
            }
            _ => out.actions.push(action),
        }
    }
    if ui.is_item_hovered() {
        ui.tooltip_text(snap.tooltip(action));
    }
}

/// One icon button. Latched buttons fill with the accent; disabled ones dim and stop responding.
#[allow(clippy::too_many_arguments)]
fn button(
    ui: &Ui,
    tex: TextureId,
    icon: Icon,
    id: &str,
    pos: [f32; 2],
    _bs: [f32; 2],
    icon_px: f32,
    enabled: bool,
    active: bool,
    _m: &Metrics,
) -> bool {
    ui.set_cursor_pos(pos);

    let style = ui.clone_style();
    let tint = if !enabled {
        style.color(StyleColor::TextDisabled)
    } else if active {
        // Text drawn *on* the accent fill.
        on_accent(style.color(StyleColor::ButtonActive))
    } else {
        style.color(StyleColor::Text)
    };

    let _dis = (!enabled).then(|| ui.begin_disabled());
    let _fill = active.then(|| {
        ui.push_style_color(StyleColor::Button, style.color(StyleColor::ButtonActive))
    });

    let (uv0, uv1) = icon.uv();
    ui.image_button_config(id, tex, [icon_px, icon_px])
        .uv0(uv0)
        .uv1(uv1)
        .bg_color([0.0, 0.0, 0.0, 0.0])
        .tint_color(tint)
        .build()
        // `bs` is the button's outer size; ImGui derives it from icon + frame padding, which the
        // theme keeps in lockstep with `button_size`. Nothing to do here but return the click.
        && enabled
}

/// Black or white, whichever stays readable on the accent. Mirrors `chrome::on_accent`, but in
/// linear-ish float space (ImGui colors), not COLORREF.
fn on_accent(c: [f32; 4]) -> [f32; 4] {
    let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    if l > 0.59 {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        [1.0, 1.0, 1.0, 1.0]
    }
}

fn status_bar(ui: &Ui, snap: &ViewSnapshot, m: &Metrics, dark: bool, w: f32, h: f32) {
    let _bg = ui.push_style_color(StyleColor::WindowBg, theme::status_bg(dark));
    ui.window("##status")
        .position([0.0, h - m.status_h], Condition::Always)
        .size([w, m.status_h], Condition::Always)
        .flags(BAR)
        .build(|| {
            let pad = (8.0 * m.scale).round();
            let y = ((m.status_h - ui.text_line_height()) * 0.5).round();
            ui.set_cursor_pos([pad, y]);
            ui.text(&snap.status_left);

            if !snap.status_right.is_empty() {
                let tw = text_w(ui, &snap.status_right);
                ui.set_cursor_pos([w - pad - tw, y]);
                ui.text(&snap.status_right);
            }
        });
}

fn empty_hint(ui: &Ui, image: (f32, f32, f32, f32)) {
    let (x, y, w, h) = image;
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let _bg = ui.push_style_color(StyleColor::WindowBg, [0.0, 0.0, 0.0, 0.0]);
    ui.window("##empty")
        .position([x, y], Condition::Always)
        .size([w, h], Condition::Always)
        .flags(OVERLAY)
        .build(|| {
            const LINE1: &str = "Drop an image here";
            const LINE2: &str = "or double-click to open";
            let lh = ui.text_line_height();
            let cy = (h * 0.5 - lh).round();
            for (i, s) in [LINE1, LINE2].iter().enumerate() {
                let tw = text_w(ui, s);
                ui.set_cursor_pos([((w - tw) * 0.5).round(), cy + i as f32 * lh * 1.5]);
                if i == 0 {
                    ui.text(*s);
                } else {
                    ui.text_disabled(*s);
                }
            }
        });
}

#[allow(clippy::too_many_arguments)]
fn transport_band(
    ui: &Ui,
    tex: TextureId,
    snap: &ViewSnapshot,
    t: &TransportSnapshot,
    m: &Metrics,
    icon_px: f32,
    w: f32,
    h: f32,
    dark: bool,
    out: &mut Frame,
) {
    let _ = (snap, dark);
    let bs = button_size(icon_px, m);
    let y0 = h - m.status_h - m.transport_h;

    ui.window("##transport")
        .position([0.0, y0], Condition::Always)
        .size([w, m.transport_h], Condition::Always)
        .flags(BAR)
        .build(|| {
            let pad = (8.0 * m.scale).round();
            let cy = ((m.transport_h - bs[1]) * 0.5).round();
            let field_w = (46.0 * m.scale).round();
            let mut x = pad;

            // Grid: cols x rows, and the frame count.
            let mut cols = t.cols as i32;
            let mut rows = t.rows as i32;
            let mut count = t.frame_count as i32;

            let text_y = ((m.transport_h - ui.text_line_height()) * 0.5).round();

            ui.set_cursor_pos([x, text_y]);
            ui.text("Grid");
            x += text_w(ui, "Grid") + pad;

            ui.set_cursor_pos([x, cy]);
            ui.set_next_item_width(field_w);
            if ui.input_int("##cols", &mut cols) {
                out.edits
                    .push(TransportEdit::SetCols(cols.clamp(1, t.grid_max as i32) as u32));
            }
            x += field_w + (4.0 * m.scale);

            ui.set_cursor_pos([x, text_y]);
            ui.text("x");
            x += text_w(ui, "x") + (4.0 * m.scale);

            ui.set_cursor_pos([x, cy]);
            ui.set_next_item_width(field_w);
            if ui.input_int("##rows", &mut rows) {
                out.edits
                    .push(TransportEdit::SetRows(rows.clamp(1, t.grid_max as i32) as u32));
            }
            x += field_w + pad;

            ui.set_cursor_pos([x, text_y]);
            ui.text("Frames");
            x += text_w(ui, "Frames") + pad;

            ui.set_cursor_pos([x, cy]);
            ui.set_next_item_width(field_w);
            if ui.input_int("##count", &mut count) {
                let max = (t.cols * t.rows).max(1) as i32;
                out.edits
                    .push(TransportEdit::SetCount(count.clamp(1, max) as u32));
            }
            x += field_w + pad;

            // Play / pause.
            let play_icon = if t.playing { Icon::Pause } else { Icon::Play };
            if button(ui, tex, play_icon, "##play", [x, cy], bs, icon_px, true, false, m) {
                out.edits.push(TransportEdit::TogglePlay);
            }
            if ui.is_item_hovered() {
                ui.tooltip_text(if t.playing { "Pause" } else { "Play" });
            }
            x += bs[0] + pad;

            // Blend + fps are right-docked; the slider takes everything in between.
            let mut blend = t.blend;
            let blend_w = text_w(ui, "Blend") + (26.0 * m.scale);
            let fps_label_w = text_w(ui, "fps");
            let right_w = blend_w + pad + field_w + (4.0 * m.scale) + fps_label_w + pad;
            let slider_w = (w - pad - right_w - x).max(40.0 * m.scale);

            let mut pos = t.frame_pos;
            let last = (t.frame_count.max(1) - 1) as f32;
            ui.set_cursor_pos([x, cy]);
            ui.set_next_item_width(slider_w);
            if ui.slider_f32("##pos", &mut pos, 0.0, last) {
                out.edits.push(TransportEdit::Scrub(pos));
            }

            let mut rx = w - pad - right_w + pad;
            ui.set_cursor_pos([rx, cy]);
            ui.set_next_item_width(field_w);
            let mut fps = t.fps;
            if ui.input_float("##fps", &mut fps) {
                out.edits.push(TransportEdit::SetFps(fps));
            }
            rx += field_w + (4.0 * m.scale);
            ui.set_cursor_pos([rx, text_y]);
            ui.text("fps");
            rx += fps_label_w + pad;

            ui.set_cursor_pos([rx, cy]);
            if ui.checkbox("Blend", &mut blend) {
                out.edits.push(TransportEdit::ToggleBlend);
            }
        });
}

/// A stable per-action id for ImGui's widget ids (two buttons must never collide).
fn action_id(a: Action) -> u32 {
    match a {
        Action::Prev => 1,
        Action::Next => 2,
        Action::ZoomOut => 3,
        Action::ZoomIn => 4,
        Action::ZoomToggle => 5,
        Action::Channel(Ch::Rgb) => 10,
        Action::Channel(Ch::R) => 11,
        Action::Channel(Ch::G) => 12,
        Action::Channel(Ch::B) => 13,
        Action::Channel(Ch::A) => 14,
        Action::ToggleTonemap => 20,
        Action::ExpUp => 21,
        Action::ExpReset => 22,
        Action::ExpDown => 23,
        Action::ToggleOutline => 30,
        Action::Background(Bg::Black) => 40,
        Action::Background(Bg::White) => 41,
        Action::Background(Bg::Grey) => 42,
        Action::Background(Bg::Checker) => 43,
        Action::ToggleFullscreen => 50,
        Action::ToggleFlipbook => 51,
        Action::OpenWithMenu => 60,
        Action::OpenSettings => 61,
        Action::Overflow => 62,
    }
}
