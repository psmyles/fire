//! The whole UI, rebuilt every frame in immediate mode: toolbar, status bar, flipbook transport, the
//! empty-state hint, the popup menus, and the settings window.
//!
//! This module is **pure UI**: no Win32, no COM, no GDI. It reads a [`ViewSnapshot`] (and, in
//! flipbook mode, a [`TransportSnapshot`]) and returns a [`Frame`] describing what the user asked
//! for. The win shell applies it. That separation is why the layout can be reasoned about at all —
//! and why scrolling, hit-testing, focus and hover are ImGui's problem now, not ours.
//!
//! Nothing here is Win32 any more. The last two holdouts — the *popup menus* — were `TrackPopupMenu`,
//! which is drawn by the system and therefore could only be dark-moded through three undocumented
//! `uxtheme.dll` ordinals. They are [`MenuState`] now, and those ordinals are gone with them: **the
//! app no longer calls a single undocumented API.**

pub mod settings;
pub mod theme;

use dear_imgui_rs::{Condition, StyleColor, TextureId, Ui, WindowFlags};

use crate::chrome::{Action, ViewSnapshot};
use crate::config::{Config, MenuEntry};
use crate::icons::Icon;
use crate::flipbook::{Grid, FPS_MAX};
use crate::render;
use crate::render::imgui::FormStyle;
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

/// A popup menu that is currently up.
///
/// This is the whole of what used to be `TrackPopupMenu` + `CreatePopupMenu` + `AppendMenuW` + a
/// command-id numbering scheme + `DestroyMenu` + the uxtheme ordinal hack that dark-moded it. A Win32
/// popup runs its own modal message pump, which is why the old one had to be *posted* out of
/// `WM_PAINT` and rebuilt from scratch on every show; an ImGui popup is drawn in the frame we were
/// already painting, so it is just state.
pub struct MenuState {
    pub kind: MenuKind,
    /// Client-coords top-left the popup drops from (the cursor, or the button's bottom edge).
    pub pos: (f32, f32),
    /// ImGui opens a popup by *event*, not by state, so this fires `open_popup` exactly once.
    requested: bool,
}

impl MenuState {
    pub fn new(kind: MenuKind, pos: (f32, f32)) -> Self {
        MenuState {
            kind,
            pos,
            requested: false,
        }
    }
}

pub enum MenuKind {
    /// The actions menu: right-click on the image, or the "Open in…" toolbar button.
    Actions,
    /// The "»" overflow menu — the toolbar buttons that didn't fit the window width. They carry their
    /// own enabled/checked state, so a dropped button behaves in the menu exactly as it would on the
    /// bar.
    Overflow(Vec<Action>),
}

/// Something chosen from the actions menu that isn't a view [`Action`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    ShowInExplorer,
    CopyFile,
    CopyPath,
    CopyFileName,
    OpenSettings,
    /// Launch the "Open in…" entry at this index path (from the root of the configured tree).
    OpenWith(Vec<usize>),
}

/// A button asking for a popup to be anchored under it, in client coords.
pub struct MenuAnchor {
    pub kind: MenuKind,
    pub pos: (f32, f32),
}

/// What one UI frame produced.
#[derive(Default)]
pub struct Frame {
    pub actions: Vec<Action>,
    pub edits: Vec<TransportEdit>,
    /// A toolbar button wants a popup opened under it.
    pub menu: Option<MenuAnchor>,
    /// Something was chosen from the actions menu.
    pub command: Option<Command>,
    /// The open popup went away (an item was chosen, or it was dismissed).
    pub menu_close: bool,
    /// The flipbook hint chip: enter flipbook mode / never ask again for this image.
    pub chip_accept: bool,
    pub chip_dismiss: bool,
    /// The settings window committed (OK or Apply): the edited config, for `App::apply_settings`.
    pub settings_apply: Option<Config>,
    /// The settings window closed (OK, Cancel, Esc, or the title bar's ×).
    pub settings_close: bool,
    /// The settings window's "Browse…": the shell runs the common file dialog, which pumps its own
    /// modal loop and so must not be entered from inside `WM_PAINT`.
    pub settings_browse: bool,
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
///
/// No gear: Settings is the last entry of the menu behind [`Action::OpenWithMenu`], which is also
/// where the viewport's right-click menu puts it. One place to look, not two.
const RIGHT: &[(Action, u8)] = &[
    (Action::ToggleOutline, 0),
    (Action::Background(Bg::Black), 1),
    (Action::Background(Bg::White), 1),
    (Action::Background(Bg::Grey), 1),
    (Action::Background(Bg::Checker), 1),
    (Action::ToggleFullscreen, 3),
    (Action::OpenWithMenu, 2),
];

use crate::render::view::{Background as Bg, Channel as Ch};

/// The HDR group: laid out only for float sources.
const HDR_GROUP: u8 = 3;

/// Everything one UI frame reads. Bundled because it is a dozen values and a positional argument
/// list that long is a bug waiting to happen (two `f32` pairs and three `bool`s, all interchangeable
/// to the compiler).
pub struct Inputs<'a> {
    pub snap: &'a ViewSnapshot,
    pub transport: Option<&'a TransportSnapshot>,
    pub chip: Option<Grid>,
    pub settings: Option<&'a mut settings::State>,
    pub menu: Option<&'a mut MenuState>,
    /// The live config — the actions menu is built straight from it (which items are shown, and the
    /// "Open in…" tree).
    pub cfg: &'a Config,
    /// The settings window's style (see `theme::form`) — composed once per frame, pushed only
    /// for the duration of that window.
    pub form: FormStyle,
    pub m: &'a Metrics,
    pub icon_px: f32,
    pub dark: bool,
    /// Window client size, physical px.
    pub client: (f32, f32),
    /// The sub-rect the image occupies (see `App::image_rect`).
    pub image: (f32, f32, f32, f32),
    pub fullscreen: bool,
}

/// Build the whole UI for one frame.
pub fn build(ui: &Ui, tex: TextureId, inp: Inputs<'_>) -> Frame {
    let mut out = Frame::default();
    let Inputs {
        snap,
        transport,
        chip,
        settings,
        menu,
        cfg,
        form,
        m,
        icon_px,
        dark,
        client,
        image,
        fullscreen,
    } = inp;
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

    // The popups last, and outside any window — which is where ImGui expects them to live.
    if let Some(st) = menu {
        menus(ui, st, cfg, snap, &mut out);
    }
    if let Some(st) = settings {
        settings::build(ui, st, form, dark, m.scale, client, &mut out);
    }

    out
}

// ---------------------------------------------------------------------------------------------
// Popup menus
// ---------------------------------------------------------------------------------------------

/// The popup's ImGui id. There is only ever one up at a time, so one id serves both kinds.
const MENU_ID: &str = "##menu";

fn menus(ui: &Ui, st: &mut MenuState, cfg: &Config, snap: &ViewSnapshot, out: &mut Frame) {
    if !st.requested {
        st.requested = true;
        ui.open_popup(MENU_ID);
    }
    render::imgui::position_next_window(st.pos);

    ui.popup(MENU_ID, || match &st.kind {
        MenuKind::Actions => actions_menu(ui, cfg, snap.has_image, out),
        MenuKind::Overflow(items) => overflow_menu(ui, snap, items, out),
    });

    // ImGui closes a popup on an outside click, on Esc, and when an item is picked. Whichever it was,
    // the shell drops the state.
    if !ui.is_popup_open(MENU_ID) {
        out.menu_close = true;
    }
}

/// Right-click on the image, and the toolbar's menu button: the fixed file actions, the configured
/// "Open in…" tree, and Settings.
///
/// Everything above Settings acts on the open image, so with nothing open the menu collapses to
/// Settings alone — which is fine, and necessary: this menu is now the only route to it.
fn actions_menu(ui: &Ui, cfg: &Config, has_image: bool, out: &mut Frame) {
    let cm = cfg.context_menu;
    let mut shown = false;

    if has_image {
        for (on, label, cmd) in [
            (cm.show_in_explorer, "Show in Explorer", Command::ShowInExplorer),
            (cm.copy_file, "Copy File", Command::CopyFile),
            (cm.copy_path, "Copy Path", Command::CopyPath),
            (cm.copy_file_name, "Copy File Name", Command::CopyFileName),
        ] {
            if !on {
                continue; // hidden in `[context-menu]`; all four can be off
            }
            if ui.menu_item(label) {
                out.command = Some(cmd);
            }
            shown = true;
        }

        if !cfg.open_with.is_empty() {
            if shown {
                ui.separator();
            }
            open_with(ui, &cfg.open_with, &mut Vec::new(), out);
            shown = true;
        }
    }

    // Settings always comes last, after a rule — it's the one entry that isn't about the image.
    if shown {
        ui.separator();
    }
    if ui.menu_item("Settings\u{2026}") {
        out.command = Some(Command::OpenSettings);
    }
}

/// The "Open in…" tree. A submenu nests; a leaf reports its index path, which the shell resolves back
/// to the entry — so there is no command-id numbering scheme to keep in step any more, and no way for
/// the menu and the launcher to disagree about which app a click meant.
///
/// A malformed entry (no program *and* no children) is skipped: the settings window creates an entry
/// before it has a program, and a half-filled one simply doesn't appear yet.
fn open_with(ui: &Ui, entries: &[MenuEntry], path: &mut Vec<usize>, out: &mut Frame) {
    for (i, e) in entries.iter().enumerate() {
        path.push(i);
        if e.is_submenu() {
            ui.menu(&e.name, || open_with(ui, &e.items, path, out));
        } else if e.path.as_deref().is_some_and(|p| !p.trim().is_empty()) && ui.menu_item(&e.name) {
            out.command = Some(Command::OpenWith(path.clone()));
        }
        path.pop();
    }
}

/// The "»" menu: the toolbar buttons that didn't fit, each carrying the enabled/checked state it
/// would have had on the bar, and each dispatching through the normal action path.
fn overflow_menu(ui: &Ui, snap: &ViewSnapshot, items: &[Action], out: &mut Frame) {
    for a in items {
        if ui.menu_item_enabled_selected_no_shortcut(
            snap.tooltip(*a),
            snap.active(*a),
            snap.enabled(*a),
        ) {
            out.actions.push(*a);
        }
    }
}

/// The flipbook detection chip: a small card floating over the top of the image when a sprite-sheet
/// grid was detected. Used to be its own layered popup HWND; it is an ImGui panel now.
///
/// The **wording** is the GDI chip's, deliberately: it read as a sentence with a button in it ("Looks
/// like a 6×6 flipbook · *View as flipbook*"), which is what makes an unprompted popup feel like an
/// offer rather than an error. The **layout** is not the GDI chip's — the window auto-sizes to its
/// content and the anchor centers it by *pivot*, so nothing here measures a width in order to halve
/// it, and the row is aligned by `align_text_to_frame_padding` rather than by hand.
fn hint_chip(ui: &Ui, g: Grid, m: &Metrics, image: (f32, f32, f32, f32), out: &mut Frame) {
    let (ix, iy, iw, _) = image;
    if iw <= 0.0 {
        return;
    }
    let label = format!("Looks like a {}\u{00d7}{} flipbook", g.cols, g.rows);
    let accent = ui.clone_style().color(StyleColor::ButtonActive);

    // Centered on the image, a little in from its top edge.
    render::imgui::anchor_next_window(
        (ix + iw * 0.5, iy + (12.0 * m.scale).round()),
        (0.5, 0.0),
    );

    ui.window("##chip")
        .flags(BAR | WindowFlags::ALWAYS_AUTO_RESIZE)
        .build(|| {
            // Drop the text by a frame's padding so it sits on the buttons' centre line instead of
            // riding at their top edge — which is exactly what was wrong before.
            ui.align_text_to_frame_padding();
            ui.text(&label);

            ui.same_line();
            // The one thing on screen asking to be clicked, so it wears the accent.
            let _fill = ui.push_style_color(StyleColor::Button, accent);
            let _hover = ui.push_style_color(StyleColor::ButtonHovered, accent);
            let _text = ui.push_style_color(StyleColor::Text, on_accent(accent));
            if ui.button("View as flipbook") {
                out.chip_accept = true;
            }
            drop((_text, _hover, _fill));

            ui.same_line();
            // A plain capital X. The dedicated close glyphs (U+2715 and friends) are not in Segoe UI
            // — a missing glyph renders as tofu, it does not fall back — and U+00D7 (×) is a
            // *multiplication sign*, drawn small to sit between operands, so it reads as a speck at
            // button size.
            if ui.button("X##chipclose") {
                out.chip_dismiss = true;
            }
            if ui.is_item_hovered() {
                ui.tooltip_text("Don't ask again for this image");
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
                    out.menu = Some(MenuAnchor {
                        kind: MenuKind::Overflow(dropped.clone()),
                        pos: (x, y + bs[1]),
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
            // Not an action — it drops a menu from under itself.
            Action::OpenWithMenu => {
                out.menu = Some(MenuAnchor {
                    kind: MenuKind::Actions,
                    pos: (pos[0], pos[1] + bs[1]),
                });
            }
            // The overflow button is handled where it is laid out (it needs the dropped list).
            Action::Overflow => {}
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

use theme::on_accent;

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

/// The flipbook transport: `cols × rows`, the frame count, play/pause, a scrub slider with its
/// `frame / total` readout, the frame rate, and the crossfade toggle.
///
/// It is one horizontal row. Every item flows from the last with `same_line`, and the only two
/// positions computed by hand are the ones that *have* to be: where the right-docked group starts
/// (so the slider knows how much room it has), and the readout's column (so the slider doesn't shuffle
/// left and right as the frame number gains and loses a digit). Both are derived from measured text,
/// not from a table of pixel constants.
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
    let _ = (snap, dark, icon_px);
    let y0 = h - m.status_h - m.transport_h;

    ui.window("##transport")
        .position([0.0, y0], Condition::Always)
        .size([w, m.transport_h], Condition::Always)
        .flags(BAR)
        .build(|| {
            let style = ui.clone_style();
            let spacing = style.item_spacing()[0];
            let inner = style.item_inner_spacing()[0];
            let pad = style.window_padding()[0];
            let fh = ui.frame_height();

            // The row is centred in the band; from here everything flows.
            ui.set_cursor_pos([pad, ((m.transport_h - fh) * 0.5).round()]);

            // Each field is as wide as the widest value it can actually hold, measured in the live
            // font — so it neither clips nor leaves a gutter, at any DPI, without a table of widths
            // to maintain.
            let field = |digits: usize| {
                text_w(ui, &"0".repeat(digits)) + style.frame_padding()[0] * 2.0
            };
            let digits = |n: u32| n.to_string().len();
            let grid_w = field(digits(t.grid_max));
            let count_w = field(digits((t.cols * t.rows).max(1)));
            let fps_w = field(digits(FPS_MAX as u32) + 2); // room for a ".5"

            let mut cols = t.cols as i32;
            let mut rows = t.rows as i32;
            let mut count = t.frame_count as i32;
            let mut fps = t.fps;
            let mut blend = t.blend;

            // --- left: the grid, the frame count, play/pause ---------------------------------
            ui.set_next_item_width(grid_w);
            if int_field(ui, "##cols", &mut cols) {
                out.edits
                    .push(TransportEdit::SetCols(cols.clamp(1, t.grid_max as i32) as u32));
            }
            ui.same_line();
            ui.align_text_to_frame_padding();
            ui.text("\u{00d7}");
            ui.same_line();
            ui.set_next_item_width(grid_w);
            if int_field(ui, "##rows", &mut rows) {
                out.edits
                    .push(TransportEdit::SetRows(rows.clamp(1, t.grid_max as i32) as u32));
            }

            ui.same_line_with_spacing(0.0, spacing * 3.0); // a group break, not a gap
            ui.set_next_item_width(count_w);
            if int_field(ui, "##count", &mut count) {
                let max = (t.cols * t.rows).max(1) as i32;
                out.edits.push(TransportEdit::SetCount(count.clamp(1, max) as u32));
            }
            if ui.is_item_hovered() {
                ui.tooltip_text("Frames used from the sheet");
            }

            ui.same_line_with_spacing(0.0, spacing * 3.0);
            // Sized to the text beside it, so the button is exactly as tall as the fields it sits in
            // line with. (The toolbar's icons are bigger; there they stand alone.)
            let glyph = ui.text_line_height().round();
            let play = if t.playing { Icon::Pause } else { Icon::Play };
            if icon_button_inline(ui, tex, play, "##play", glyph) {
                out.edits.push(TransportEdit::TogglePlay);
            }
            if ui.is_item_hovered() {
                ui.tooltip_text(if t.playing { "Pause" } else { "Play" });
            }

            // --- right: the readout, the frame rate, blend -----------------------------------
            let total = t.frame_count.max(1);
            let frame = (t.frame_pos.floor() as u32 + 1).min(total);
            let readout = format!("{frame} / {total}");
            // Reserve the *widest* readout this clip can produce, or the slider would resize under
            // the cursor every time the frame number crossed a power of ten.
            let readout_w = text_w(ui, &format!("{total} / {total}"));
            let blend_w = fh + inner + text_w(ui, "Blend");
            let right_w =
                readout_w + spacing + fps_w + inner + text_w(ui, "fps") + spacing * 2.0 + blend_w;
            let right_x = w - pad - right_w;

            // --- middle: the slider takes what's left ----------------------------------------
            let mut pos = t.frame_pos;
            let last = (total - 1) as f32;
            // `same_line` *first*: until it runs, the cursor is parked at the start of the next line,
            // and measuring from there would size the slider as if the left group weren't in front of
            // it — it would then overshoot `right_x` and paint its own frame under the fps field and
            // the checkbox, which is exactly what it did.
            ui.same_line();
            let slider_w = (right_x - spacing - ui.cursor_pos_x()).max(fh * 2.0);
            ui.set_next_item_width(slider_w);
            // The number lives in the readout, so the track shows none: an empty format, not a
            // hidden label.
            if ui
                .slider_config("##pos", 0.0f32, last)
                .display_format("")
                .build(&mut pos)
            {
                // With crossfade off there is nothing between frames to land on.
                out.edits
                    .push(TransportEdit::Scrub(if t.blend { pos } else { pos.floor() }));
            }

            ui.same_line_with_pos(right_x);
            ui.align_text_to_frame_padding();
            ui.text_disabled(&readout);

            ui.same_line_with_pos(right_x + readout_w + spacing);
            ui.set_next_item_width(fps_w);
            if fps_field(ui, "##fps", &mut fps) {
                out.edits.push(TransportEdit::SetFps(fps));
            }
            ui.same_line_with_spacing(0.0, inner);
            ui.align_text_to_frame_padding();
            ui.text("fps");

            ui.same_line_with_spacing(0.0, spacing * 2.0);
            if ui.checkbox("Blend", &mut blend) {
                out.edits.push(TransportEdit::ToggleBlend);
            }
            if ui.is_item_hovered() {
                ui.tooltip_text("Crossfade between frames");
            }
        });
}

/// A plain numeric box. `step(0)` is the whole point: ImGui's `InputInt` ships as a *stepper*, with
/// `−`/`+` buttons that eat the field and leave the value with nowhere to render.
fn int_field(ui: &Ui, id: &str, v: &mut i32) -> bool {
    ui.input_int_config(id).step(0).step_fast(0).build(v)
}

/// The frame-rate box. `%g` prints `24`, not `24.000`, and still gives `12.5` when it needs to.
fn fps_field(ui: &Ui, id: &str, v: &mut f32) -> bool {
    ui.input_float_config(id)
        .step(0.0)
        .step_fast(0.0)
        .format("%g")
        .build(v)
}

/// An icon button at the current cursor, sized to `px` — for a row that *flows* (the toolbar
/// positions its buttons instead, so it uses [`button`]).
fn icon_button_inline(ui: &Ui, tex: TextureId, icon: Icon, id: &str, px: f32) -> bool {
    let (uv0, uv1) = icon.uv();
    let tint = ui.clone_style().color(StyleColor::Text);
    ui.image_button_config(id, tex, [px, px])
        .uv0(uv0)
        .uv1(uv1)
        .bg_color([0.0, 0.0, 0.0, 0.0])
        .tint_color(tint)
        .build()
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
        Action::Overflow => 62,
    }
}
