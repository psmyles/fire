//! The settings window — a modal ImGui popup over the viewer.
//!
//! This replaced a 2,150-line hand-painted Win32 dialog: its own window class, its own message pump,
//! its own focus/hover/scroll/hit-test layer, three subclassed `EDIT` children, and a `TrackPopupMenu`
//! standing in for a combo box. All of that is now four ImGui calls — a real `TabBar`, a real
//! `BeginChild` scroll region, a real `InputText`, a real `Combo` — which is the whole argument for
//! the migration: a scrollbar that doesn't drag is not a bug you can *have* here.
//!
//! Three consequences worth knowing:
//!
//! * **No nested message pump, so no `&mut App` hazard.** The old dialog ran its own `GetMessageW`
//!   loop, which re-entered the frame's wndproc and would have aliased `&mut App` for as long as the
//!   window was open — hence the clone-and-`PostMessage` dance it was built around. A popup is drawn
//!   *inside* the frame we were already painting, so that whole class of problem is gone: the state
//!   lives in `App`, and the shell applies what this returns.
//! * **It wears ImGui's stock style, not fire's chrome theme** (see
//!   [`StockStyle`](crate::render::imgui::StockStyle)). The chrome is a toolbar — flat, transparent,
//!   built to sit over an image. A settings window is a form, and stock ImGui already knows what a
//!   form looks like.
//! * **Two things still reach out to Win32**, and both must leave `WM_PAINT` first: "Browse…" (the
//!   common file dialog pumps its own modal loop) and key *capture* on the Keybinds tab (chords are
//!   virtual-key codes, which only the wndproc sees). Both are reported to the shell rather than done
//!   here — this module stays pure UI.
//!
//! [`model`] is unchanged from the Win32 dialog: the field accessors and the open-with tree edits
//! were always pure logic, and they port across untouched.

pub mod model;

use dear_imgui_rs::{StyleColor, TabBarFlags, Ui};

use crate::config::Config;
use crate::keybinds::{KeyAction, KeyChord, Keybinds, ALL_ACTIONS};
use crate::render::imgui::{center_next_window, size_next_window, FormStyle};

use model::{BoolField, ChoiceField, NumField, TextField, {self as m}};

use super::{text_w, Frame};

/// The popup's ImGui id *and* its title bar.
const TITLE: &str = "Settings";

/// The window's opening size as a fraction of the viewport. It is resizable from there, and ImGui
/// clamps it to the screen — so this is a proportion, not a size, and there is no pixel width
/// anywhere in this module for a font or a DPI to invalidate.
const OPEN_FRACTION: (f32, f32) = (0.62, 0.78);

/// Esc, read raw from the wndproc during a key capture (see [`State::capture_key`]).
const VK_ESCAPE: u32 = 0x1B;

/// The Context-menu tab's text boxes, in tab order; the index is also the index into
/// [`State::fields`].
const TEXT_FIELDS: [TextField; 3] = [TextField::Name, TextField::Program, TextField::Args];

/// Everything the settings window is editing, across frames.
///
/// An immediate-mode UI redraws; it does not remember. So the draft config, the selection, and the
/// text being typed live here, owned by `App` for as long as the window is open.
pub struct State {
    /// The edited config. `applied` is the last committed state — **Apply** is live exactly when
    /// they differ.
    draft: Config,
    applied: Config,
    /// The edited keyboard table, mirrored into `draft.keybinds` on every change so the dirty check
    /// stays a single `draft != applied`.
    keys: Keybinds,
    /// The keybind row waiting for a key press. While this is armed the shell routes **every** key
    /// here — including Esc, which ImGui would otherwise take as "close the modal".
    capture: Option<KeyAction>,
    /// The line under the keybind list: a conflict report, or the capture prompt.
    note: String,
    /// The selected open-with entry, by index path from the root.
    sel: Option<Vec<usize>>,
    /// The detail form's text. Owned here rather than re-read from `draft` each frame, or the
    /// `InputText` would be rewritten under its own caret on every keystroke.
    fields: [String; 3],
    /// The selection [`Self::fields`] currently holds the text of.
    seeded: Option<Vec<usize>>,
    /// ImGui opens a popup by *event*, not by state, so this fires `open_popup` exactly once.
    requested: bool,
}

impl State {
    pub fn new(cfg: &Config) -> Self {
        State {
            keys: Keybinds::from_config(&cfg.keybinds),
            applied: cfg.clone(),
            draft: cfg.clone(),
            capture: None,
            note: String::new(),
            sel: None,
            fields: Default::default(),
            seeded: None,
            requested: false,
        }
    }

    /// Whether a keybind row is armed, i.e. the next key press is a binding and not a command.
    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }

    /// Take a key press for the armed row (the shell hands us the raw virtual key and the live
    /// modifier state — this module never touches Win32).
    ///
    /// Esc cancels, since a dialog you can't escape is a trap; that does mean Esc itself is only
    /// bindable by hand in `config.toml`. A bare modifier is ignored: we're waiting for the key it
    /// modifies.
    pub fn capture_key(&mut self, vk: u32, ctrl: bool, alt: bool, shift: bool) {
        let Some(action) = self.capture else { return };
        if vk == VK_ESCAPE {
            self.capture = None;
            self.note.clear();
            return;
        }
        let chord = KeyChord { vk, ctrl, alt, shift };
        if chord.is_reserved() {
            return;
        }
        let loser = self.keys.rebind(action, chord);
        self.sync_keys();
        self.capture = None;
        // What the key does now and — the part that must not be missed — what it was taken from.
        self.note = match loser {
            Some(l) => format!(
                "{} \u{2192} {}.  {} is now unbound.",
                chord.display(),
                action.label(),
                l.label()
            ),
            None => format!("{} \u{2192} {}.", chord.display(), action.label()),
        };
    }

    /// Adopt the program the shell's "Browse…" dialog returned.
    pub fn set_program(&mut self, path: &str) {
        let Some(p) = self.sel.clone() else { return };
        self.fields[1] = path.to_string();
        if let Some(e) = m::entry_at(&mut self.draft.open_with, &p) {
            TextField::Program.set(e, path);
        }
    }

    /// Push the edited keyboard table into the draft, so the dirty check sees it.
    fn sync_keys(&mut self) {
        self.draft.keybinds = self.keys.to_config();
    }

    /// Commit the draft: clamp it, make it the new baseline (so Apply greys out), and hand a copy to
    /// the shell to apply and persist. Also the Enter key's action (see `App::settings_key`).
    pub fn commit(&mut self) -> Config {
        self.draft.sanitize();
        self.applied = self.draft.clone();
        self.draft.clone()
    }

    fn dirty(&self) -> bool {
        self.draft != self.applied
    }
}

/// Build the settings window for one frame.
///
/// `client` is the viewer's client size in physical px — the window centers on it and opens
/// proportioned to it.
pub fn build(
    ui: &Ui,
    st: &mut State,
    mut base: FormStyle,
    dark: bool,
    scale: f32,
    client: (f32, f32),
    out: &mut Frame,
) {
    // Everything from here to the end of the function is drawn in the settings window's own style —
    // fire's colors on ImGui's *form* geometry, not the chrome's toolbar geometry.
    super::theme::form(base.style_mut(), dark, scale);
    let _style = base.push();

    if !st.requested {
        ui.open_popup(TITLE);
        st.requested = true;
    }
    center_next_window(client);
    size_next_window((client.0 * OPEN_FRACTION.0, client.1 * OPEN_FRACTION.1));

    // `opened` gives the title bar its × — ImGui clears it and closes the popup in one go, so we
    // don't read it back; `is_popup_open` below is the single source of "ImGui closed it".
    //
    // Esc and Enter are *not* ImGui's: it deliberately leaves modals open on Escape. The shell binds
    // them (`App::settings_key`) and simply drops this state.
    let mut opened = true;
    ui.modal_popup_with_opened(TITLE, &mut opened, || {
        // The footer is *pinned to the bottom* and the tabs scroll above it. That is what the child's
        // negative height buys: a `BeginChild` sized `-footer` takes all the remaining height except
        // that much, so OK/Cancel/Apply sit on the window's bottom edge at any size, and the
        // scrollbar covers the settings rather than the buttons.
        let style = ui.clone_style();
        let footer = ui.frame_height() + style.item_spacing()[1] * 2.0 + style.separator_size();
        tabs(ui, st, footer, out);

        ui.separator();
        buttons(ui, st, out);
        if out.settings_close {
            ui.close_current_popup();
        }
    });

    if !ui.is_popup_open(TITLE) {
        out.settings_close = true;
    }
}

fn tabs(ui: &Ui, st: &mut State, footer: f32, out: &mut Frame) {
    // Full width, and everything down to the footer. Every tab is the same box, so the window doesn't
    // jump when you switch between them.
    let size = [0.0, -footer];
    // The accent rule along the selected tab's top edge is opt-in — `TabSelectedOverline` is a color
    // ImGui won't draw at all without this flag.
    let Some(_bar) = ui.tab_bar_with_flags("##tabs", TabBarFlags::DRAW_SELECTED_OVERLINE) else {
        return;
    };

    if let Some(_tab) = ui.tab_item("General") {
        ui.child_window("##general")
            .size(size)
            .build(ui, || general(ui, st));
    }
    if let Some(_tab) = ui.tab_item("Flipbook") {
        ui.child_window("##flipbook")
            .size(size)
            .build(ui, || flipbook(ui, st));
    }
    if let Some(_tab) = ui.tab_item("Keybinds") {
        ui.child_window("##keybinds")
            .size(size)
            .build(ui, || keybinds(ui, st));
    }
    if let Some(_tab) = ui.tab_item("Context menu") {
        ui.child_window("##context")
            .size(size)
            .build(ui, || context_menu(ui, st, out));
    }
}

// ---------------------------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------------------------

/// The width of a tab's label column: its longest label, plus a gutter. Measured in the live font, so
/// it follows the font, the DPI and nothing else.
fn label_col(ui: &Ui, labels: &[&str]) -> f32 {
    let widest = labels.iter().map(|s| text_w(ui, s)).fold(0.0f32, f32::max);
    widest + ui.clone_style().item_spacing()[0] * 2.0
}

/// Start a form row: **label on the left**, control filling the rest of the line. Returns the width
/// the control gets.
///
/// ImGui's native order is the reverse — it draws a widget's label *after* the widget — which reads
/// as "New window ▼ Opening an image" and puts the labels in a ragged column on the right. So the
/// label is drawn here and each control below is given a hidden `##id` instead.
fn row(ui: &Ui, label: &str, label_w: f32) -> f32 {
    ui.align_text_to_frame_padding();
    ui.text(label);
    ui.same_line_with_pos(label_w);
    ui.content_region_avail()[0]
}

/// Explanatory text under a labelled control, aligned with the control rather than the margin — so it
/// reads as a footnote to *that* setting and not to the section.
fn row_note(ui: &Ui, label_w: f32, text: &str) {
    ui.indent_by(label_w);
    note(ui, text);
    ui.unindent_by(label_w);
}

fn general(ui: &Ui, st: &mut State) {
    let lw = label_col(
        ui,
        &[
            "Opening an image",
            "Images open",
            "Backdrop",
            "HDR tone map",
            "Zoom step",
            "Exposure step",
        ],
    );

    ui.separator_with_text("Window");
    choice(ui, st, lw, ChoiceField::InstanceMode, "Opening an image");
    row_note(ui, lw, "Takes effect for images opened from now on.");
    check(ui, st, BoolField::HotReload, "Reload the image when the file changes on disk");

    ui.spacing();
    ui.separator_with_text("View");
    choice(ui, st, lw, ChoiceField::DefaultFit, "Images open");
    choice(ui, st, lw, ChoiceField::Background, "Backdrop");
    choice(ui, st, lw, ChoiceField::DefaultTonemap, "HDR tone map");
    check(
        ui,
        st,
        BoolField::FitUpscale,
        "\"Fit to window\" also enlarges small images",
    );

    ui.spacing();
    ui.separator_with_text("Input");
    num(ui, st, lw, NumField::ZoomStep, "Zoom step");
    row_note(ui, lw, "Zoom factor per wheel notch or key press.");
    num(ui, st, lw, NumField::ExposureStep, "Exposure step");
    row_note(ui, lw, "Stops per press of the exposure keys (HDR images).");
}

fn flipbook(ui: &Ui, st: &mut State) {
    let lw = label_col(ui, &["Frame rate"]);

    ui.separator_with_text("Detection");
    check(
        ui,
        st,
        BoolField::FlipbookAutoDetect,
        "Offer flipbook mode when an image looks like a sprite sheet",
    );
    note(ui, "Off skips the scan entirely; flipbook mode still works by hand.");

    ui.spacing();
    ui.separator_with_text("Playback defaults");
    note(
        ui,
        "Applied when flipbook mode is switched on for an image. The transport bar under the \
         image still changes the one you are watching.",
    );
    ui.spacing();
    num(ui, st, lw, NumField::FlipbookFps, "Frame rate");
    check(ui, st, BoolField::FlipbookAutoplay, "Start playing immediately");
    check(ui, st, BoolField::FlipbookBlend, "Crossfade between frames");
}

/// The rebind editor: every action, its chord, and a per-row reset. Clicking the chord arms a
/// capture — from then until the next key press, the shell routes keys to [`State::capture_key`]
/// rather than to the viewer.
fn keybinds(ui: &Ui, st: &mut State) {
    let defaults = Keybinds::defaults();
    let style = ui.clone_style();
    let spacing = style.item_spacing()[0];

    // One label column, sized to the longest action name; the chord button then takes the whole row
    // except what Reset needs.
    let label_w = ALL_ACTIONS
        .iter()
        .map(|a| text_w(ui, a.label()))
        .fold(0.0f32, f32::max)
        + spacing * 2.0;
    let reset_w = text_w(ui, "Reset") + style.frame_padding()[0] * 2.0;

    let mut group = "";
    for action in ALL_ACTIONS.iter().copied() {
        if action.group() != group {
            group = action.group();
            ui.separator_with_text(group);
        }

        // Everything read out of `st` up front: the buttons below take `&mut st`.
        let capturing = st.capture == Some(action);
        let chords = st.keys.chords(action);
        let is_default = defaults.chords(action) == chords;
        let chord_text = if capturing {
            "Press a key\u{2026}".to_string()
        } else if chords.is_empty() {
            "Unbound".to_string()
        } else {
            chords
                .iter()
                .map(|c| c.display())
                .collect::<Vec<_>>()
                .join(", ")
        };

        ui.align_text_to_frame_padding();
        ui.text(action.label());
        ui.same_line_with_pos(label_w);

        let chord_w = (ui.content_region_avail()[0] - reset_w - spacing).max(ui.frame_height());
        // The label *is* the chord, so the id has to come from the action — two actions bound to the
        // same key would otherwise collide into one button.
        if ui.button_with_size(
            format!("{chord_text}##bind-{}", action.name()),
            [chord_w, 0.0],
        ) {
            st.capture = Some(action);
            st.note = format!("Press a key for {}\u{2026}  (Esc cancels)", action.label());
        }

        ui.same_line();
        let _dis = is_default.then(|| ui.begin_disabled());
        if ui.button_with_size(format!("Reset##reset-{}", action.name()), [reset_w, 0.0]) {
            st.keys.reset(action);
            st.sync_keys();
            st.capture = None;
            st.note = format!("{}: default restored.", action.label());
        }
    }

    ui.spacing();
    ui.separator();
    if ui.button("Restore all defaults") {
        st.keys = Keybinds::defaults();
        st.sync_keys();
        st.capture = None;
        st.note = "All shortcuts restored to their defaults.".into();
    }
    if !st.note.is_empty() {
        ui.same_line();
        ui.text_disabled(&st.note);
    }
}

fn context_menu(ui: &Ui, st: &mut State, out: &mut Frame) {
    ui.separator_with_text("Built-in items");
    check(ui, st, BoolField::CtxShowInExplorer, "Show in Explorer");
    check(ui, st, BoolField::CtxCopyFile, "Copy File");
    check(ui, st, BoolField::CtxCopyPath, "Copy Path");
    check(ui, st, BoolField::CtxCopyFileName, "Copy File Name");

    ui.spacing();
    ui.separator_with_text("\"Open in\u{2026}\" entries");
    note(ui, "Programs to open the current image with. Nest entries to make submenus.");

    // The tree, as a scrolling list of indented rows: full width, and six rows tall — measured in
    // rows, so it holds six of them whatever the font and the DPI happen to be.
    let row_h = ui.frame_height();
    let tree = m::flatten(&st.draft.open_with);
    ui.child_window("##tree")
        .size([0.0, row_h * 6.0])
        .border(true)
        .build(ui, || {
            if tree.is_empty() {
                ui.text_disabled("No entries yet \u{2014} \"Add item\" creates one.");
            }
            for row in &tree {
                let indent = row.depth as f32 * ui.clone_style().indent_spacing();
                if indent > 0.0 {
                    ui.indent_by(indent);
                }
                let label = if row.submenu {
                    format!("{}  \u{25b8}##row-{:?}", row.name, row.path)
                } else {
                    format!("{}##row-{:?}", row.name, row.path)
                };
                let selected = st.sel.as_deref() == Some(row.path.as_slice());
                // `close_popups(false)`: a Selectable inside a popup closes it by default, which
                // would shut the whole settings window on a click in this list.
                if ui
                    .selectable_config(label)
                    .selected(selected)
                    .close_popups(false)
                    .build()
                {
                    st.sel = Some(row.path.clone());
                }
                if indent > 0.0 {
                    ui.unindent_by(indent);
                }
            }
        });

    // The tree tools. Each edit returns the path its entry ended up at, so the selection follows the
    // thing the user was working on.
    let sel = st.sel.clone();
    let has_sel = sel.is_some();

    if ui.button("Add item") {
        st.sel = Some(m::insert_after(
            &mut st.draft.open_with,
            sel.as_deref(),
            m::new_item(),
        ));
    }
    ui.same_line();
    if ui.button("Add submenu") {
        st.sel = Some(m::insert_after(
            &mut st.draft.open_with,
            sel.as_deref(),
            m::new_submenu(),
        ));
    }
    ui.same_line();

    let _dis = (!has_sel).then(|| ui.begin_disabled());
    if ui.button("Remove") {
        if let Some(p) = &sel {
            st.sel = m::remove_at(&mut st.draft.open_with, p);
        }
    }
    for (label, op) in [
        ("\u{2191}", Move::Up),
        ("\u{2193}", Move::Down),
        ("\u{2192}|", Move::Indent),
        ("|\u{2190}", Move::Outdent),
    ] {
        ui.same_line();
        if ui.button(label) {
            if let Some(p) = &sel {
                let moved = match op {
                    Move::Up => m::move_sibling(&mut st.draft.open_with, p, -1),
                    Move::Down => m::move_sibling(&mut st.draft.open_with, p, 1),
                    Move::Indent => m::indent(&mut st.draft.open_with, p),
                    Move::Outdent => m::outdent(&mut st.draft.open_with, p),
                };
                if let Some(np) = moved {
                    st.sel = Some(np);
                }
            }
        }
    }
    drop(_dis);

    detail_form(ui, st, out);
}

/// A tree-tool button that moves the selected entry.
#[derive(Clone, Copy)]
enum Move {
    Up,
    Down,
    Indent,
    Outdent,
}

/// The selected entry's name / program / arguments.
fn detail_form(ui: &Ui, st: &mut State, out: &mut Frame) {
    let Some(path) = st.sel.clone() else { return };

    // Refill the text boxes when the *selection* moves — never on a plain redraw, or every frame
    // would rewrite the box under the caret.
    if st.seeded.as_deref() != Some(path.as_slice()) {
        st.seeded = Some(path.clone());
        st.fields = match m::entry_at(&mut st.draft.open_with, &path) {
            Some(e) => TEXT_FIELDS.map(|f| f.get(e)),
            None => Default::default(),
        };
    }
    let is_submenu = m::entry_at(&mut st.draft.open_with, &path).is_some_and(|e| e.is_submenu());

    let labels: Vec<&str> = TEXT_FIELDS.iter().map(|f| f.label()).collect();
    let lw = label_col(ui, &labels);
    let style = ui.clone_style();
    let browse_w = text_w(ui, "Browse\u{2026}") + style.frame_padding()[0] * 2.0;

    ui.spacing();
    let w = row(ui, TEXT_FIELDS[0].label(), lw);
    ui.set_next_item_width(w);
    if ui.input_text("##name", &mut st.fields[0]).build() {
        write_field(st, &path, 0);
    }

    if is_submenu {
        row_note(
            ui,
            lw,
            "A submenu \u{2014} its program and arguments are unused while it has children.",
        );
        return;
    }

    // A path is the one value here that is never short, so the box takes the row — minus Browse.
    let w = row(ui, TEXT_FIELDS[1].label(), lw);
    ui.set_next_item_width(w - browse_w - style.item_spacing()[0]);
    if ui.input_text("##program", &mut st.fields[1]).build() {
        write_field(st, &path, 1);
    }
    ui.same_line();
    // The file picker pumps its own modal loop, so the shell runs it once this paint has finished.
    if ui.button_with_size("Browse\u{2026}", [browse_w, 0.0]) {
        out.settings_browse = true;
    }

    let w = row(ui, TEXT_FIELDS[2].label(), lw);
    ui.set_next_item_width(w);
    if ui.input_text("##args", &mut st.fields[2]).build() {
        write_field(st, &path, 2);
    }
    row_note(ui, lw, "{path} is replaced with the image's full path.");
}

/// Push the text box's contents into the selected entry.
fn write_field(st: &mut State, path: &[usize], i: usize) {
    let text = st.fields[i].clone();
    if let Some(e) = m::entry_at(&mut st.draft.open_with, path) {
        TEXT_FIELDS[i].set(e, &text);
    }
}

/// OK / Cancel / Apply, right-aligned on the window's bottom edge. Apply is live exactly while there
/// is something to apply.
fn buttons(ui: &Ui, st: &mut State, out: &mut Frame) {
    let style = ui.clone_style();
    let spacing = style.item_spacing()[0];
    // All three get the width of the widest, so they read as one set of choices rather than three
    // buttons that happen to be adjacent.
    let w = ["OK", "Cancel", "Apply"]
        .iter()
        .map(|s| text_w(ui, s))
        .fold(0.0f32, f32::max)
        + style.frame_padding()[0] * 4.0;
    let total = w * 3.0 + spacing * 2.0;
    let avail = ui.content_region_avail()[0];
    ui.set_cursor_pos_x(ui.cursor_pos_x() + (avail - total).max(0.0));

    if ui.button_with_size("OK", [w, 0.0]) {
        out.settings_apply = Some(st.commit());
        out.settings_close = true;
    }
    ui.same_line();
    if ui.button_with_size("Cancel", [w, 0.0]) {
        out.settings_close = true;
    }
    ui.same_line();
    let _dis = (!st.dirty()).then(|| ui.begin_disabled());
    if ui.button_with_size("Apply", [w, 0.0]) {
        out.settings_apply = Some(st.commit());
    }
}

// ---------------------------------------------------------------------------------------------
// Widgets over `model`'s field accessors
// ---------------------------------------------------------------------------------------------

fn check(ui: &Ui, st: &mut State, f: BoolField, label: &str) {
    let mut v = f.get(&st.draft);
    if ui.checkbox(label, &mut v) {
        f.set(&mut st.draft, v);
    }
}

fn choice(ui: &Ui, st: &mut State, label_w: f32, f: ChoiceField, label: &str) {
    let w = row(ui, label, label_w);
    let mut i = f.get(&st.draft);
    ui.set_next_item_width(w);
    // `##` hides ImGui's own label (which would land on the *right*) while keeping it as the widget's
    // id. The visible text was drawn by `row`, on the left, where a form's label belongs.
    if ui.combo_simple_string(format!("##{label}"), &mut i, f.options()) {
        f.set(&mut st.draft, i);
    }
}

/// A numeric field. The slider's range *is* [`crate::config::Config::sanitize`]'s clamp (they are
/// checked against each other in `model`'s tests), so a value out of range can't be produced here in
/// the first place. Ctrl+click still lets you type one.
fn num(ui: &Ui, st: &mut State, label_w: f32, f: NumField, label: &str) {
    let w = row(ui, label, label_w);
    let (min, max, _, dp) = f.spec();
    let mut v = f.get(&st.draft);
    ui.set_next_item_width(w);
    if ui
        .slider_config(format!("##{label}"), min, max)
        .display_format(format!("%.{dp}f"))
        .build(&mut v)
    {
        f.set(&mut st.draft, v);
    }
}

/// Dim explanatory text under a control. **Wrapped**, because the window is resizable now — a note
/// that ran off the edge would simply be cut in half.
fn note(ui: &Ui, text: &str) {
    let dim = ui.clone_style().color(StyleColor::TextDisabled);
    let _c = ui.push_style_color(StyleColor::Text, dim);
    ui.text_wrapped(text);
}
