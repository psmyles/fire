//! The settings dialog — a modal, tabbed, hand-painted Win32 window.
//!
//! Custom-drawn for the same reason the toolbar is (see [`crate::chrome`]): the Win32 common
//! controls have no dark mode, and a settings window that ignores the app's theme is worse than no
//! settings window. So there are **no child controls at all** — the dialog is one HWND, every widget
//! is a rect in [`Dialog::widgets`], and paint/hit-test/focus walk that list. It borrows the
//! chrome's [`Palette`](crate::chrome) and its GDI helpers, so it tracks the app's theme and DPI for
//! free.
//!
//! ## The `&mut App` rule
//!
//! [`run_modal`] runs a **nested message pump** (with the owner disabled — the standard Win32 modal
//! idiom, and the same posture as the `TrackPopupMenu` / `GetOpenFileNameW` calls the app already
//! makes). That pump re-enters the frame's wndproc, which takes its own `&mut App` out of
//! `GWLP_USERDATA` on every message. So the dialog **must never hold an `App` borrow**: it is handed
//! a *clone* of the [`Config`] and hands the edited copy back by `PostMessage`-ing
//! [`WM_APP_SETTINGS_APPLY`] to the frame, which reclaims the box and applies it under a fresh
//! borrow. Do not "simplify" this by passing `&mut App` in — it would alias for as long as the
//! dialog is open.
//!
//! ## Apply model
//!
//! Edits go to a `draft` config; `applied` is the last-committed baseline, so **Apply** is enabled
//! exactly when they differ. OK applies and closes, Apply applies and stays, Cancel discards. What
//! each field does on apply (live vs. next-image vs. next-launch) is [`crate::win::App::apply_settings`].

mod model;

use std::path::PathBuf;
use std::ptr;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC,
    DeleteObject, EndPaint, GetDC, IntersectClipRect, InvalidateRect, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_PATH_ELLIPSIS,
    DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_PATHMUSTEXIST,
    OPENFILENAMEW,
};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect, LoadCursorW,
    LoadIconW, PostMessageW, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TrackPopupMenu, TranslateMessage, GWLP_USERDATA, IDC_ARROW, MF_CHECKED, MF_STRING,
    MSG, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW, TPM_LEFTALIGN, TPM_LEFTBUTTON, TPM_RETURNCMD,
    TPM_TOPALIGN, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETTINGCHANGE, WM_SYSKEYDOWN, WNDCLASSW, WS_CAPTION,
    WS_EX_DLGMODALFRAME, WS_POPUP, WS_SYSMENU,
};

use crate::chrome::{self, create_ui_font, draw_text, fill, text_width, Palette};
use crate::config::Config;
use crate::keybinds::{KeyAction, KeyChord, Keybinds, ALL_ACTIONS};
use crate::win::WM_APP_SETTINGS_APPLY;

use model::{
    BoolField, ChoiceField, NumField, TextField, TreeRow, {self as m},
};

/// `WM_MOUSELEAVE` — see the identical note in [`crate::win`].
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Modifier / non-chord virtual keys (the dialog reads them live to build a captured chord).
const VK_TAB: u32 = 0x09;
const VK_RETURN: u32 = 0x0D;
const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
const VK_LEFT: u32 = 0x25;
const VK_RIGHT: u32 = 0x27;
const VK_HOME: u32 = 0x24;
const VK_END: u32 = 0x23;
const VK_DELETE: u32 = 0x2E;
const VK_BACK: u32 = 0x08;

const CLASS_NAME: &str = "FireSettingsClass";
static REGISTER: std::sync::Once = std::sync::Once::new();

/// Dialog client size in logical (96-dpi) px. Fixed: the content is a single column, and a resizable
/// settings window buys nothing but layout code.
const DLG_W: i32 = 580;
const DLG_H: i32 = 520;

/// The tabs, in strip order.
const TABS: &[&str] = &["General", "Flipbook", "Keybinds", "Context menu"];
const TAB_GENERAL: usize = 0;
const TAB_FLIPBOOK: usize = 1;
const TAB_KEYBINDS: usize = 2;
const TAB_CONTEXT: usize = 3;

// ---------------------------------------------------------------------------------------------
// Theme + metrics
// ---------------------------------------------------------------------------------------------

/// The dialog's colors: the chrome's [`Palette`] plus the two an input field needs (the chrome has
/// no text inputs, so it has no tokens for them).
struct Theme {
    p: Palette,
    /// Text-input / dropdown fill — sunken relative to the body.
    field_bg: u32,
    /// The dialog body (behind the tab content).
    body_bg: u32,
    /// The tab strip and the button bar, a shade off the body so the content reads as a panel.
    bar_bg: u32,
}

impl Theme {
    fn new(dark: bool) -> Self {
        let p = Palette::for_mode(dark);
        Theme {
            field_bg: if dark { rgb(30, 30, 30) } else { rgb(255, 255, 255) },
            body_bg: p.toolbar_bg,
            bar_bg: p.status_bg,
            p,
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// DPI-scaled dialog metrics (all inputs are 96-dpi logical px).
struct Metrics {
    dpi: u32,
    font: HFONT,
    pad: i32,
    tabstrip_h: i32,
    buttonbar_h: i32,
    row_h: i32,
    ctl_h: i32,
    label_w: i32,
    drop_w: i32,
    step_w: i32,
    check: i32,
    head_h: i32,
    note_h: i32,
    gap: i32,
    btn_w: i32,
    /// Width of the chord box on a keybind row.
    key_w: i32,
}

impl Metrics {
    fn new(dpi: u32) -> Self {
        let s = |v: i32| v * dpi as i32 / 96;
        Metrics {
            dpi,
            font: create_ui_font(dpi),
            pad: s(16),
            tabstrip_h: s(36),
            buttonbar_h: s(52),
            row_h: s(30),
            ctl_h: s(24),
            label_w: s(170),
            drop_w: s(190),
            step_w: s(110),
            check: s(16),
            head_h: s(32),
            note_h: s(20),
            gap: s(8),
            btn_w: s(84),
            key_w: s(150),
        }
    }

    fn scale(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
    }
}

impl Drop for Metrics {
    fn drop(&mut self) {
        unsafe { DeleteObject(self.font as HGDIOBJ) };
    }
}

// ---------------------------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------------------------

/// A dialog push-button (the bottom bar's, and the Context-menu tab's tree tools).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Ok,
    Cancel,
    Apply,
    AddItem,
    AddSubmenu,
    Remove,
    MoveUp,
    MoveDown,
    Indent,
    Outdent,
    Browse,
    ResetKeys,
}

/// What a widget *is*. Non-interactive kinds (headings, notes, labels) are laid out and painted like
/// any other widget but are skipped by focus and hit-testing.
enum Ctl {
    Heading,
    Note,
    Label,
    Check(BoolField),
    Drop(ChoiceField),
    Step(NumField),
    Button(Btn),
    /// One rebindable action: its name, the chord box, and a reset glyph.
    KeyRow(KeyAction),
    /// One row of the flattened open-with tree (index into [`Dialog::tree`]).
    TreeRow(usize),
    Text(TextField),
}

impl Ctl {
    fn interactive(&self) -> bool {
        !matches!(self, Ctl::Heading | Ctl::Note | Ctl::Label)
    }
}

struct Widget {
    rect: RECT,
    ctl: Ctl,
    text: String,
    /// Whether it lives in the scrolling content area (vs. the fixed button bar).
    scrolls: bool,
    enabled: bool,
}

// ---------------------------------------------------------------------------------------------
// The dialog
// ---------------------------------------------------------------------------------------------

struct Dialog {
    hwnd: HWND,
    owner: HWND,
    th: Theme,
    m: Metrics,
    dark: bool,

    /// The edited config. `applied` is the last committed state — Apply is live iff they differ.
    draft: Config,
    applied: Config,
    /// The edited keyboard table. Mirrored into `draft.keybinds` on every change, so the dirty check
    /// stays a single `draft != applied`.
    keys: Keybinds,

    tab: usize,
    tab_rects: Vec<RECT>,
    widgets: Vec<Widget>,
    focus: Option<usize>,
    hover: Option<usize>,
    tracking_leave: bool,

    /// Scroll offset of the content area, and its full laid-out height.
    scroll: i32,
    content_h: i32,

    /// The keybind row waiting for a key press, if any.
    capture: Option<KeyAction>,
    /// The message under the keybind list (a conflict report, or the capture prompt).
    key_note: String,

    /// The flattened open-with tree and the selected entry's path.
    tree: Vec<TreeRow>,
    tree_sel: Option<Vec<usize>>,

    /// The text box being typed into (its field, buffer and caret). Held separately from the draft
    /// so normalizing writes (`args` splitting) can't fight the keystrokes.
    edit: Option<(TextField, String, usize)>,

    alive: bool,
}

/// Open the settings dialog modally over `owner`, seeded with `cfg`. Returns when it closes; any
/// Apply/OK has been posted to `owner` as [`WM_APP_SETTINGS_APPLY`] by then (the frame applies it
/// from its own message loop, under its own `&mut App` — see the module docs).
pub fn run_modal(owner: isize, cfg: Config, dark: bool) {
    let owner = owner as HWND;
    let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
    let class = wide(CLASS_NAME);
    REGISTER.call_once(|| unsafe {
        #[allow(clippy::manual_dangling_ptr)]
        let icon = LoadIconW(hinstance, 1 as *const u16);
        RegisterClassW(&WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: icon,
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: class.as_ptr(),
        });
    });

    // Disable the owner *before* the dialog appears: that is what makes this modal (input is
    // blocked; posted messages — decode results, animation timers — still dispatch, so the image
    // behind the dialog keeps living).
    unsafe { EnableWindow(owner, 0) };

    let dpi = unsafe { GetDpiForWindow(owner) }.max(96);
    let (w, h) = window_size(dpi);
    let (x, y) = center_on(owner, w, h);
    let title = wide(&format!("{} \u{2014} Settings", crate::product::NAME));
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            w,
            h,
            owner,
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        eprintln!("fire: CreateWindowExW(settings) failed");
        unsafe { EnableWindow(owner, 1) };
        return;
    }
    chrome::apply_dark_titlebar(hwnd, dark);
    chrome::apply_dark_menus(hwnd, dark);

    let keys = Keybinds::from_config(&cfg.keybinds);
    let mut dlg = Box::new(Dialog {
        hwnd,
        owner,
        th: Theme::new(dark),
        m: Metrics::new(dpi),
        dark,
        applied: cfg.clone(),
        draft: cfg,
        keys,
        tab: TAB_GENERAL,
        tab_rects: Vec::new(),
        widgets: Vec::new(),
        focus: None,
        hover: None,
        tracking_leave: false,
        scroll: 0,
        content_h: 0,
        capture: None,
        key_note: String::new(),
        tree: Vec::new(),
        tree_sel: None,
        edit: None,
        alive: true,
    });
    dlg.relayout();

    let dlg_raw = Box::into_raw(dlg);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, dlg_raw as isize);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);

        // The modal pump. It dispatches everything, so the frame still repaints and its timers still
        // tick behind us — `EnableWindow` blocks *input*, not messages.
        let mut msg: MSG = std::mem::zeroed();
        while (*dlg_raw).alive && GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Re-enable the owner *before* destroying the dialog. The other order hands activation to
        // whatever window is next in the z-order — usually another application.
        EnableWindow(owner, 1);
        DestroyWindow(hwnd);
        SetForegroundWindow(owner);
        drop(Box::from_raw(dlg_raw));
    }
}

/// The outer window size for a `DLG_W`×`DLG_H` client at `dpi`.
fn window_size(dpi: u32) -> (i32, i32) {
    let s = |v: i32| v * dpi as i32 / 96;
    let mut r = RECT {
        left: 0,
        top: 0,
        right: s(DLG_W),
        bottom: s(DLG_H),
    };
    unsafe { AdjustWindowRectExForDpi(&mut r, WS_POPUP | WS_CAPTION | WS_SYSMENU, 0, WS_EX_DLGMODALFRAME, dpi) };
    (r.right - r.left, r.bottom - r.top)
}

/// Top-left that centers a `w`×`h` window on `owner`.
fn center_on(owner: HWND, w: i32, h: i32) -> (i32, i32) {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(owner, &mut r) } == 0 {
        return (100, 100);
    }
    (
        r.left + ((r.right - r.left) - w) / 2,
        r.top + ((r.bottom - r.top) - h) / 2,
    )
}

impl Dialog {
    // --- layout -------------------------------------------------------------

    fn client(&self) -> (i32, i32) {
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut rc) };
        (rc.right.max(0), rc.bottom.max(0))
    }

    /// The scrolling content region (between the tab strip and the button bar).
    fn content_rect(&self) -> RECT {
        let (w, h) = self.client();
        RECT {
            left: 0,
            top: self.m.tabstrip_h,
            right: w,
            bottom: h - self.m.buttonbar_h,
        }
    }

    /// Rebuild every rect for the current tab, DPI, and scroll offset. Cheap (a few dozen widgets),
    /// so it runs on any change rather than being patched incrementally.
    fn relayout(&mut self) {
        let (w, h) = self.client();
        self.widgets.clear();
        self.tab_rects.clear();

        // Tab strip: labels sized to their text.
        let hdc = unsafe { GetDC(self.hwnd) };
        let prev = unsafe { SelectObject(hdc, self.m.font as HGDIOBJ) };
        let mut x = self.m.pad;
        for name in TABS {
            let tw = text_width(hdc, name) + 2 * self.m.pad;
            self.tab_rects.push(RECT {
                left: x,
                top: 0,
                right: x + tw,
                bottom: self.m.tabstrip_h,
            });
            x += tw;
        }
        unsafe {
            SelectObject(hdc, prev);
            ReleaseDC(self.hwnd, hdc);
        }

        // Tab content, laid out in a single column from the top of the content area. `y` is a
        // content-space cursor; `push_*` converts to client coords by subtracting the scroll.
        self.tree = m::flatten(&self.draft.open_with);
        let mut y = self.m.pad;
        match self.tab {
            TAB_GENERAL => self.layout_general(&mut y),
            TAB_FLIPBOOK => self.layout_flipbook(&mut y),
            TAB_KEYBINDS => self.layout_keybinds(&mut y),
            TAB_CONTEXT => self.layout_context(&mut y),
            _ => {}
        }
        self.content_h = y + self.m.pad;

        // Clamp the scroll to the freshly measured content, then (if that moved it) re-lay-out so
        // every rect reflects the final offset.
        let view_h = (h - self.m.tabstrip_h - self.m.buttonbar_h).max(0);
        let max_scroll = (self.content_h - view_h).max(0);
        let clamped = self.scroll.clamp(0, max_scroll);
        if clamped != self.scroll {
            self.scroll = clamped;
            return self.relayout();
        }

        // Button bar: OK / Cancel / Apply, right-aligned.
        let by = h - self.m.buttonbar_h + (self.m.buttonbar_h - self.m.ctl_h) / 2;
        let dirty = self.dirty();
        let mut bx = w - self.m.pad;
        for (btn, label, enabled) in [
            (Btn::Apply, "Apply", dirty),
            (Btn::Cancel, "Cancel", true),
            (Btn::Ok, "OK", true),
        ] {
            bx -= self.m.btn_w;
            self.widgets.push(Widget {
                rect: RECT {
                    left: bx,
                    top: by,
                    right: bx + self.m.btn_w,
                    bottom: by + self.m.ctl_h,
                },
                ctl: Ctl::Button(btn),
                text: label.into(),
                scrolls: false,
                enabled,
            });
            bx -= self.m.gap;
        }

        // Focus can't survive a tab switch or a tree edit that removed its widget.
        if self.focus.is_some_and(|f| {
            f >= self.widgets.len() || !self.widgets[f].ctl.interactive() || !self.widgets[f].enabled
        }) {
            self.focus = None;
        }
    }

    /// The client-coords rect for a content row at content-space `y`.
    fn row(&self, y: i32, h: i32) -> RECT {
        let top = self.content_rect().top + y - self.scroll;
        RECT {
            left: 0,
            top,
            right: self.client().0,
            bottom: top + h,
        }
    }

    fn push(&mut self, rect: RECT, ctl: Ctl, text: impl Into<String>) {
        self.widgets.push(Widget {
            rect,
            ctl,
            text: text.into(),
            scrolls: true,
            enabled: true,
        });
    }

    fn heading(&mut self, y: &mut i32, text: &str) {
        let r = self.inset(self.row(*y, self.m.head_h), self.m.pad);
        self.push(r, Ctl::Heading, text);
        *y += self.m.head_h;
    }

    /// Dim explanatory text under a *control*, aligned with the control column.
    fn note(&mut self, y: &mut i32, text: &str) {
        let r = self.inset(self.row(*y, self.m.note_h), self.m.pad + self.m.label_w);
        self.push(r, Ctl::Note, text);
        *y += self.m.note_h;
    }

    /// Dim explanatory text for a whole *section*, at the left margin so it has the full width.
    fn section_note(&mut self, y: &mut i32, text: &str) {
        let r = self.inset(self.row(*y, self.m.note_h), self.m.pad);
        self.push(r, Ctl::Note, text);
        *y += self.m.note_h;
    }

    /// `row` with `left` moved in and `right` pulled back to the page margin.
    fn inset(&self, row: RECT, left: i32) -> RECT {
        RECT {
            left,
            right: row.right - self.m.pad,
            ..row
        }
    }

    /// A `label: <control>` row. Returns the control's rect (the caller sizes its width).
    fn labeled(&mut self, y: &mut i32, label: &str, ctl_w: i32) -> RECT {
        let row = self.row(*y, self.m.row_h);
        let lr = RECT {
            left: self.m.pad,
            right: self.m.pad + self.m.label_w,
            ..row
        };
        self.push(lr, Ctl::Label, label);
        let cx = self.m.pad + self.m.label_w;
        let cy = row.top + (self.m.row_h - self.m.ctl_h) / 2;
        *y += self.m.row_h;
        RECT {
            left: cx,
            top: cy,
            right: cx + ctl_w,
            bottom: cy + self.m.ctl_h,
        }
    }

    fn dropdown(&mut self, y: &mut i32, label: &str, f: ChoiceField) {
        let r = self.labeled(y, label, self.m.drop_w);
        self.push(r, Ctl::Drop(f), "");
    }

    fn stepper(&mut self, y: &mut i32, label: &str, f: NumField) {
        let r = self.labeled(y, label, self.m.step_w);
        self.push(r, Ctl::Step(f), "");
    }

    /// A checkbox, aligned with the control column so a mixed column still reads as one.
    fn checkbox(&mut self, y: &mut i32, f: BoolField, label: &str) {
        let row = self.row(*y, self.m.row_h);
        let cy = row.top + (self.m.row_h - self.m.ctl_h) / 2;
        let left = self.m.pad + self.m.label_w;
        let w = self.m.check + self.m.gap + 2 + text_w(self.hwnd, self.m.font, label);
        self.push(
            RECT {
                left,
                top: cy,
                right: left + w,
                bottom: cy + self.m.ctl_h,
            },
            Ctl::Check(f),
            label,
        );
        *y += self.m.row_h;
    }

    fn layout_general(&mut self, y: &mut i32) {
        self.heading(y, "Window");
        self.dropdown(y, "Opening an image", ChoiceField::InstanceMode);
        self.note(y, "Takes effect for images opened from now on.");
        self.checkbox(
            y,
            BoolField::HotReload,
            "Reload the image when the file changes on disk",
        );

        *y += self.m.gap;
        self.heading(y, "View");
        self.dropdown(y, "Images open", ChoiceField::DefaultFit);
        self.dropdown(y, "Backdrop", ChoiceField::Background);
        self.checkbox(
            y,
            BoolField::FitUpscale,
            "\"Fit to window\" also enlarges small images",
        );
        self.dropdown(y, "HDR tone map", ChoiceField::DefaultTonemap);

        *y += self.m.gap;
        self.heading(y, "Input");
        self.stepper(y, "Zoom step", NumField::ZoomStep);
        self.note(y, "Zoom factor per wheel notch or key press.");
        self.stepper(y, "Exposure step", NumField::ExposureStep);
        self.note(y, "Stops per press of the exposure keys (HDR images).");
    }

    fn layout_flipbook(&mut self, y: &mut i32) {
        self.heading(y, "Detection");
        self.checkbox(
            y,
            BoolField::FlipbookAutoDetect,
            "Offer flipbook mode when an image looks like a sprite sheet",
        );
        self.note(
            y,
            "Off skips the scan entirely; flipbook mode still works by hand.",
        );

        *y += self.m.gap;
        self.heading(y, "Playback defaults");
        self.section_note(
            y,
            "Applied when flipbook mode is switched on for an image. The transport bar under",
        );
        self.section_note(y, "the image still changes the one you are watching.");
        *y += self.m.gap / 2;
        self.stepper(y, "Frame rate", NumField::FlipbookFps);
        self.checkbox(y, BoolField::FlipbookAutoplay, "Start playing immediately");
        self.checkbox(y, BoolField::FlipbookBlend, "Crossfade between frames");
    }

    fn layout_keybinds(&mut self, y: &mut i32) {
        let mut group = "";
        for action in ALL_ACTIONS.iter().copied() {
            if action.group() != group {
                group = action.group();
                self.heading(y, group);
            }
            let row = self.row(*y, self.m.row_h);
            self.push(row, Ctl::KeyRow(action), action.label());
            *y += self.m.row_h;
        }
        *y += self.m.gap;
        let left = self.m.pad + self.m.label_w;
        let r = RECT {
            left,
            top: self.row(*y, self.m.ctl_h).top,
            right: left + self.m.scale(130),
            bottom: self.row(*y, self.m.ctl_h).top + self.m.ctl_h,
        };
        self.push(r, Ctl::Button(Btn::ResetKeys), "Restore defaults");
        *y += self.m.row_h;
    }

    fn layout_context(&mut self, y: &mut i32) {
        self.heading(y, "Built-in items");
        for (f, label) in [
            (BoolField::CtxShowInExplorer, "Show in Explorer"),
            (BoolField::CtxCopyFile, "Copy File"),
            (BoolField::CtxCopyPath, "Copy Path"),
            (BoolField::CtxCopyFileName, "Copy File Name"),
        ] {
            self.checkbox(y, f, label);
        }

        *y += self.m.gap;
        self.heading(y, "\"Open in\u{2026}\" entries");
        self.section_note(
            y,
            "Programs to open the current image with. Nest entries to make submenus.",
        );
        *y += self.m.gap / 2;

        // The tree: one row per entry (they share a fill, so they read as one list panel), then the
        // tools beneath it.
        let sel = self.tree_sel.clone();
        let row_h = self.m.row_h * 4 / 5;
        for i in 0..self.tree.len() {
            let row = self.inset(self.row(*y, row_h), self.m.pad);
            let name = self.tree[i].name.clone();
            self.push(row, Ctl::TreeRow(i), name);
            *y += row_h;
        }
        if self.tree.is_empty() {
            self.section_note(y, "No entries yet \u{2014} \"Add item\" creates one.");
        }
        *y += self.m.gap;

        let has_sel = sel.is_some();
        let mut bx = self.m.pad;
        let by = self.row(*y, self.m.ctl_h).top;
        for (btn, label, on) in [
            (Btn::AddItem, "Add item", true),
            (Btn::AddSubmenu, "Add submenu", true),
            (Btn::Remove, "Remove", has_sel),
            (Btn::MoveUp, "\u{2191}", has_sel),
            (Btn::MoveDown, "\u{2193}", has_sel),
            (Btn::Indent, "\u{2192}|", has_sel),
            (Btn::Outdent, "|\u{2190}", has_sel),
        ] {
            let w = text_w(self.hwnd, self.m.font, label) + 2 * self.m.gap + self.m.scale(6);
            self.widgets.push(Widget {
                rect: RECT {
                    left: bx,
                    top: by,
                    right: bx + w,
                    bottom: by + self.m.ctl_h,
                },
                ctl: Ctl::Button(btn),
                text: label.into(),
                scrolls: true,
                enabled: on,
            });
            bx += w + self.m.gap / 2;
        }
        *y += self.m.row_h + self.m.gap;

        // The selected entry's detail form.
        let Some(path) = sel else { return };
        let is_submenu =
            m::entry_at(&mut self.draft.open_with, &path).is_some_and(|e| e.is_submenu());
        let r = self.labeled(y, TextField::Name.label(), self.m.scale(300));
        self.push(r, Ctl::Text(TextField::Name), "");
        if is_submenu {
            self.section_note(
                y,
                "A submenu \u{2014} its program and arguments are unused while it has children.",
            );
            return;
        }
        let path_w = self.m.scale(300) - self.m.btn_w - self.m.gap;
        let r = self.labeled(y, TextField::Program.label(), path_w);
        self.push(r, Ctl::Text(TextField::Program), "");
        let browse = RECT {
            left: r.right + self.m.gap,
            right: r.right + self.m.gap + self.m.btn_w,
            ..r
        };
        self.push(browse, Ctl::Button(Btn::Browse), "Browse\u{2026}");
        let r = self.labeled(y, TextField::Args.label(), self.m.scale(300));
        self.push(r, Ctl::Text(TextField::Args), "");
        self.note(y, "{path} is replaced with the image's full path.");
    }

    // --- state --------------------------------------------------------------

    fn dirty(&self) -> bool {
        self.draft != self.applied
    }

    /// The selected open-with entry, if any.
    fn selected_entry(&mut self) -> Option<&mut crate::config::MenuEntry> {
        let path = self.tree_sel.clone()?;
        m::entry_at(&mut self.draft.open_with, &path)
    }

    /// Push the edited keyboard table into the draft, so the dirty check sees it.
    fn sync_keys(&mut self) {
        self.draft.keybinds = self.keys.to_config();
    }

    /// Commit the draft: hand it to the frame (which applies it live and saves it) and make it the
    /// new baseline, so Apply greys out until something else changes.
    fn apply(&mut self) {
        self.flush_edit();
        self.draft.sanitize();
        self.applied = self.draft.clone();
        let payload = Box::new(self.draft.clone());
        let lparam = Box::into_raw(payload) as isize;
        // SAFETY: the box outlives the post; the frame's wndproc reclaims it. If the post fails
        // (owner gone), reclaim here rather than leak.
        let posted =
            unsafe { PostMessageW(self.owner, WM_APP_SETTINGS_APPLY, 0, lparam) };
        if posted == 0 {
            drop(unsafe { Box::from_raw(lparam as *mut Config) });
        }
    }

    fn close(&mut self) {
        self.alive = false;
    }

    fn invalidate(&self) {
        unsafe { InvalidateRect(self.hwnd, ptr::null(), 0) };
    }

    /// Re-lay-out and repaint — the tail of every state change.
    fn refresh(&mut self) {
        self.relayout();
        self.invalidate();
    }

    // --- text editing -------------------------------------------------------

    /// Write the in-progress text box back to the draft (also called on every keystroke, so the tree
    /// row's name tracks what you type).
    fn flush_edit(&mut self) {
        let Some((field, text, _)) = self.edit.clone() else {
            return;
        };
        if let Some(e) = self.selected_entry() {
            field.set(e, &text);
        }
    }

    /// Seed the text buffer when focus lands on a text box (and flush the one we're leaving).
    fn sync_edit_focus(&mut self) {
        let target = self
            .focus
            .and_then(|f| match self.widgets.get(f).map(|w| &w.ctl) {
                Some(Ctl::Text(field)) => Some(*field),
                _ => None,
            });
        if self.edit.as_ref().map(|(f, _, _)| *f) == target {
            return;
        }
        self.flush_edit();
        self.edit = target.and_then(|field| {
            let entry = self.selected_entry()?;
            let text = field.get(entry);
            let caret = text.chars().count();
            Some((field, text, caret))
        });
    }

    /// Apply a text-box editing key. Returns whether it was consumed.
    fn edit_key(&mut self, vk: u32) -> bool {
        let Some((_, text, caret)) = &mut self.edit else {
            return false;
        };
        let len = text.chars().count();
        match vk {
            VK_LEFT => *caret = caret.saturating_sub(1),
            VK_RIGHT => *caret = (*caret + 1).min(len),
            VK_HOME => *caret = 0,
            VK_END => *caret = len,
            VK_BACK if *caret > 0 => {
                let at = byte_at(text, *caret - 1);
                text.remove(at);
                *caret -= 1;
            }
            VK_DELETE if *caret < len => {
                let at = byte_at(text, *caret);
                text.remove(at);
            }
            _ => return false,
        }
        self.flush_edit();
        true
    }

    fn edit_char(&mut self, ch: char) -> bool {
        if ch.is_control() {
            return false;
        }
        let Some((_, text, caret)) = &mut self.edit else {
            return false;
        };
        let at = byte_at(text, *caret);
        text.insert(at, ch);
        *caret += 1;
        self.flush_edit();
        true
    }

    // --- input --------------------------------------------------------------

    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let content = self.content_rect();
        self.widgets.iter().position(|w| {
            w.ctl.interactive()
                && w.enabled
                && inside(&w.rect, x, y)
                // A scrolled-out content widget must not be clickable through the button bar.
                && (!w.scrolls || (y >= content.top && y < content.bottom))
        })
    }

    fn click(&mut self, x: i32, y: i32) {
        // Tab strip.
        if let Some(i) = self.tab_rects.iter().position(|r| inside(r, x, y)) {
            if i != self.tab {
                self.set_tab(i);
            }
            return;
        }
        let Some(i) = self.hit(x, y) else {
            // A click on dead space cancels an armed capture and drops focus.
            self.capture = None;
            self.focus = None;
            self.sync_edit_focus();
            self.refresh();
            return;
        };
        self.focus = Some(i);
        self.sync_edit_focus();
        self.activate(i, x, y);
    }

    /// Act on widget `i`. `(x, y)` locates the click within it (a stepper's ± cells, a keybind row's
    /// reset glyph); a keyboard activation passes the widget's own center.
    fn activate(&mut self, i: usize, x: i32, _y: i32) {
        let Some(w) = self.widgets.get(i) else { return };
        let rect = w.rect;
        match w.ctl {
            Ctl::Check(f) => {
                let on = f.get(&self.draft);
                f.set(&mut self.draft, !on);
            }
            Ctl::Drop(f) => self.open_dropdown(f, rect),
            Ctl::Step(f) => {
                // The ± cells sit at either end of the field.
                let cell = self.m.ctl_h;
                if x < rect.left + cell {
                    f.nudge(&mut self.draft, -1);
                } else if x >= rect.right - cell {
                    f.nudge(&mut self.draft, 1);
                }
            }
            Ctl::KeyRow(action) => {
                // The reset glyph is the trailing cell; anywhere else arms the capture.
                if x >= rect.right - self.m.row_h && !self.is_default_binding(action) {
                    self.keys.reset(action);
                    self.sync_keys();
                    self.capture = None;
                    self.key_note = format!("{}: default restored.", action.label());
                } else {
                    self.capture = Some(action);
                    self.key_note =
                        format!("Press a key for {}\u{2026}  (Esc cancels)", action.label());
                }
            }
            Ctl::TreeRow(idx) => {
                self.tree_sel = self.tree.get(idx).map(|r| r.path.clone());
                self.focus = None;
                self.edit = None;
            }
            Ctl::Text(_) => {} // focus alone starts editing
            Ctl::Button(b) => return self.press(b),
            _ => {}
        }
        self.refresh();
    }

    fn press(&mut self, b: Btn) {
        match b {
            Btn::Ok => {
                self.apply();
                return self.close();
            }
            Btn::Cancel => return self.close(),
            Btn::Apply => self.apply(),
            Btn::ResetKeys => {
                self.keys = Keybinds::defaults();
                self.sync_keys();
                self.capture = None;
                self.key_note = "All shortcuts restored to their defaults.".into();
            }
            Btn::AddItem | Btn::AddSubmenu => {
                let entry = if b == Btn::AddItem {
                    m::new_item()
                } else {
                    m::new_submenu()
                };
                let sel = self.tree_sel.clone();
                let at = m::insert_after(&mut self.draft.open_with, sel.as_deref(), entry);
                self.tree_sel = Some(at);
                self.edit = None;
            }
            Btn::Remove => {
                if let Some(p) = self.tree_sel.clone() {
                    self.tree_sel = m::remove_at(&mut self.draft.open_with, &p);
                    self.edit = None;
                }
            }
            Btn::MoveUp | Btn::MoveDown | Btn::Indent | Btn::Outdent => {
                if let Some(p) = self.tree_sel.clone() {
                    let moved = match b {
                        Btn::MoveUp => m::move_sibling(&mut self.draft.open_with, &p, -1),
                        Btn::MoveDown => m::move_sibling(&mut self.draft.open_with, &p, 1),
                        Btn::Indent => m::indent(&mut self.draft.open_with, &p),
                        _ => m::outdent(&mut self.draft.open_with, &p),
                    };
                    if let Some(np) = moved {
                        self.tree_sel = Some(np);
                    }
                }
            }
            Btn::Browse => {
                if let Some(path) = browse_for_program(self.hwnd) {
                    let text = path.to_string_lossy().into_owned();
                    if let Some(e) = self.selected_entry() {
                        TextField::Program.set(e, &text);
                    }
                    // Re-seed the box if it is the one being typed into.
                    if let Some((TextField::Program, buf, caret)) = &mut self.edit {
                        *caret = text.chars().count();
                        *buf = text;
                    }
                }
            }
        }
        self.refresh();
    }

    /// The dropdown's list, as a themed popup menu — `TrackPopupMenu` already follows the app's
    /// dark mode (see `chrome::apply_dark_menus`), so this is a real list with none of the code.
    fn open_dropdown(&mut self, f: ChoiceField, rect: RECT) {
        let cur = f.get(&self.draft);
        let mut pt = POINT {
            x: rect.left,
            y: rect.bottom,
        };
        let chosen = unsafe {
            ClientToScreen(self.hwnd, &mut pt);
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            for (i, opt) in f.options().iter().enumerate() {
                let flags = MF_STRING | if i == cur { MF_CHECKED } else { 0 };
                let label = wide(opt);
                AppendMenuW(menu, flags, i + 1, label.as_ptr());
            }
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_LEFTBUTTON,
                pt.x,
                pt.y,
                0,
                self.hwnd,
                ptr::null(),
            );
            DestroyMenu(menu);
            (cmd as usize).checked_sub(1)
        };
        if let Some(i) = chosen {
            f.set(&mut self.draft, i);
        }
    }

    fn set_tab(&mut self, tab: usize) {
        self.flush_edit();
        self.tab = tab.min(TABS.len() - 1);
        self.scroll = 0;
        self.focus = None;
        self.capture = None;
        self.edit = None;
        self.key_note.clear();
        self.refresh();
    }

    fn scroll_by(&mut self, dy: i32) {
        let before = self.scroll;
        self.scroll += dy;
        self.relayout(); // clamps
        if self.scroll != before {
            self.invalidate();
        }
    }

    /// Whether `action` still holds its shipped chords (drives the reset glyph).
    fn is_default_binding(&self, action: KeyAction) -> bool {
        Keybinds::defaults().chords(action) == self.keys.chords(action)
    }

    /// Take a key press for the armed keybind row. Modifier-only presses are ignored (we're waiting
    /// for the key they modify); Esc cancels, since a dialog you can't escape is a trap — which does
    /// mean Esc itself can only be bound by hand in `config.toml`.
    fn capture_key(&mut self, vk: u32) {
        let Some(action) = self.capture else { return };
        if vk == VK_ESCAPE {
            self.capture = None;
            self.key_note.clear();
            return self.refresh();
        }
        let chord = KeyChord {
            vk,
            ctrl: key_down(VK_CONTROL),
            alt: key_down(VK_MENU),
            shift: key_down(VK_SHIFT),
        };
        if chord.is_reserved() {
            return; // a bare modifier — keep waiting
        }
        let loser = self.keys.rebind(action, chord);
        self.sync_keys();
        self.capture = None;
        // Short enough to fit the footer: what the key does now, and — the part that must not be
        // missed — which action it was taken away from.
        self.key_note = match loser {
            Some(l) => format!(
                "{} \u{2192} {}. {} is now unbound.",
                chord.display(),
                action.label(),
                l.label()
            ),
            None => format!("{} \u{2192} {}.", chord.display(), action.label()),
        };
        self.refresh();
    }

    fn key(&mut self, vk: u32) {
        if self.capture.is_some() {
            return self.capture_key(vk);
        }
        // A focused text box owns its editing keys before anything else looks at them.
        if self.edit.is_some() && self.edit_key(vk) {
            return self.invalidate();
        }
        match vk {
            VK_TAB => {
                let back = key_down(VK_SHIFT);
                if key_down(VK_CONTROL) {
                    let n = TABS.len();
                    let next = if back {
                        (self.tab + n - 1) % n
                    } else {
                        (self.tab + 1) % n
                    };
                    return self.set_tab(next);
                }
                self.move_focus(back);
            }
            VK_RETURN => {
                self.apply();
                self.close();
            }
            VK_ESCAPE => self.close(),
            VK_SPACE => {
                if let Some(i) = self.focus {
                    let c = center(&self.widgets[i].rect);
                    self.activate(i, c.0, c.1);
                }
            }
            VK_LEFT | VK_RIGHT => {
                let dir = if vk == VK_LEFT { -1 } else { 1 };
                let Some(i) = self.focus else { return };
                match self.widgets[i].ctl {
                    Ctl::Step(f) => f.nudge(&mut self.draft, dir),
                    Ctl::Drop(f) => {
                        let n = f.options().len() as i32;
                        let cur = f.get(&self.draft) as i32;
                        f.set(&mut self.draft, (cur + dir).clamp(0, n - 1) as usize);
                    }
                    _ => return,
                }
                self.refresh();
            }
            _ => {}
        }
    }

    /// Move focus to the next (or previous) interactive widget, scrolling it into view.
    fn move_focus(&mut self, back: bool) {
        let items: Vec<usize> = (0..self.widgets.len())
            .filter(|&i| self.widgets[i].ctl.interactive() && self.widgets[i].enabled)
            .collect();
        if items.is_empty() {
            return;
        }
        let pos = self
            .focus
            .and_then(|f| items.iter().position(|&i| i == f))
            .map(|p| {
                let n = items.len();
                if back {
                    (p + n - 1) % n
                } else {
                    (p + 1) % n
                }
            })
            .unwrap_or(if back { items.len() - 1 } else { 0 });
        self.focus = Some(items[pos]);
        self.scroll_into_view();
        self.sync_edit_focus();
        self.refresh();
    }

    /// Nudge the scroll so the focused widget is fully inside the content area.
    fn scroll_into_view(&mut self) {
        let Some(i) = self.focus else { return };
        let w = &self.widgets[i];
        if !w.scrolls {
            return;
        }
        let (top, bottom) = (w.rect.top, w.rect.bottom);
        let c = self.content_rect();
        if top < c.top + self.m.pad {
            self.scroll -= (c.top + self.m.pad) - top;
        } else if bottom > c.bottom - self.m.pad {
            self.scroll += bottom - (c.bottom - self.m.pad);
        }
    }

    fn set_hover(&mut self, x: i32, y: i32) {
        let h = self.hit(x, y);
        if h != self.hover {
            self.hover = h;
            self.invalidate();
        }
        if !self.tracking_leave {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            unsafe { TrackMouseEvent(&mut tme) };
            self.tracking_leave = true;
        }
    }

    // --- paint --------------------------------------------------------------

    fn paint(&self, hdc: HDC) {
        let (w, h) = self.client();
        let body = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        fill(hdc, &body, self.th.body_bg);

        unsafe {
            SelectObject(hdc, self.m.font as HGDIOBJ);
            SetBkMode(hdc, TRANSPARENT as i32);
        }

        // Content first, clipped to its region so scrolled rows can't bleed into the bars.
        let content = self.content_rect();
        unsafe {
            let saved = windows_sys::Win32::Graphics::Gdi::SaveDC(hdc);
            IntersectClipRect(hdc, content.left, content.top, content.right, content.bottom);
            for (i, widget) in self.widgets.iter().enumerate() {
                if widget.scrolls && widget.rect.bottom > content.top && widget.rect.top < content.bottom
                {
                    self.paint_widget(hdc, i, widget);
                }
            }
            windows_sys::Win32::Graphics::Gdi::RestoreDC(hdc, saved);
        }
        self.paint_scrollbar(hdc, &content);

        // Tab strip.
        let strip = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: self.m.tabstrip_h,
        };
        fill(hdc, &strip, self.th.bar_bg);
        for (i, r) in self.tab_rects.iter().enumerate() {
            let active = i == self.tab;
            if active {
                fill(hdc, r, self.th.body_bg);
                let underline = RECT {
                    top: r.bottom - self.m.scale(2),
                    ..*r
                };
                fill(hdc, &underline, self.th.p.btn_active);
            }
            let color = if active {
                self.th.p.text
            } else {
                self.th.p.text_dim
            };
            text_in(hdc, TABS[i], r, color, DT_CENTER_VC);
        }

        // Button bar.
        let bar = RECT {
            left: 0,
            top: h - self.m.buttonbar_h,
            right: w,
            bottom: h,
        };
        fill(hdc, &bar, self.th.bar_bg);
        let sep = RECT {
            bottom: bar.top + 1,
            ..bar
        };
        fill(hdc, &sep, self.th.p.separator);
        for (i, widget) in self.widgets.iter().enumerate() {
            if !widget.scrolls {
                self.paint_widget(hdc, i, widget);
            }
        }

        // The keybind status line ("X is now …, it was …") lives in the footer, not next to the row
        // that caused it: the row can be scrolled off, and the *consequence* of a steal — some other
        // action just lost its key — is exactly the thing the user must not miss.
        if !self.key_note.is_empty() {
            let buttons_left = self
                .widgets
                .iter()
                .filter(|w| !w.scrolls)
                .map(|w| w.rect.left)
                .min()
                .unwrap_or(w);
            let r = RECT {
                left: self.m.pad,
                right: buttons_left - self.m.gap,
                ..bar
            };
            text_in(
                hdc,
                &self.key_note,
                &r,
                self.th.p.text_dim,
                DT_LEFT_VC | DT_END_ELLIPSIS,
            );
        }
    }

    /// A thin thumb on the right edge, drawn only when the content overflows.
    fn paint_scrollbar(&self, hdc: HDC, content: &RECT) {
        let view_h = content.bottom - content.top;
        if self.content_h <= view_h || view_h <= 0 {
            return;
        }
        let track_w = self.m.scale(4);
        let x = content.right - track_w - self.m.scale(2);
        let thumb_h = (view_h * view_h / self.content_h).max(self.m.scale(24));
        let span = view_h - thumb_h;
        let max_scroll = (self.content_h - view_h).max(1);
        let top = content.top + span * self.scroll / max_scroll;
        fill(
            hdc,
            &RECT {
                left: x,
                top,
                right: x + track_w,
                bottom: top + thumb_h,
            },
            self.th.p.separator,
        );
    }

    fn paint_widget(&self, hdc: HDC, i: usize, w: &Widget) {
        let focused = self.focus == Some(i);
        let hovered = self.hover == Some(i);
        let p = &self.th.p;
        match w.ctl {
            Ctl::Heading => {
                text_in(hdc, &w.text, &w.rect, p.text, DT_LEFT_VC);
                let line = RECT {
                    top: w.rect.bottom - self.m.scale(6),
                    bottom: w.rect.bottom - self.m.scale(6) + 1,
                    ..w.rect
                };
                fill(hdc, &line, p.separator);
            }
            Ctl::Note => text_in(hdc, &w.text, &w.rect, p.text_dim, DT_LEFT_VC | DT_END_ELLIPSIS),
            Ctl::Label => text_in(hdc, &w.text, &w.rect, p.text, DT_LEFT_VC),
            Ctl::Check(f) => {
                let on = f.get(&self.draft);
                let box_r = RECT {
                    left: w.rect.left,
                    top: w.rect.top + (self.m.ctl_h - self.m.check) / 2,
                    right: w.rect.left + self.m.check,
                    bottom: w.rect.top + (self.m.ctl_h - self.m.check) / 2 + self.m.check,
                };
                if on {
                    fill(hdc, &box_r, p.btn_active);
                    text_in(hdc, "\u{2713}", &box_r, p.btn_active_text, DT_CENTER_VC);
                } else {
                    fill(hdc, &box_r, self.th.field_bg);
                    frame(hdc, &box_r, if hovered { p.text_dim } else { p.border });
                }
                let text_r = RECT {
                    left: box_r.right + self.m.gap,
                    ..w.rect
                };
                text_in(hdc, &w.text, &text_r, p.text, DT_LEFT_VC);
                if focused {
                    frame(hdc, &w.rect, p.btn_active);
                }
            }
            Ctl::Drop(f) => {
                fill(hdc, &w.rect, self.th.field_bg);
                frame(hdc, &w.rect, border_of(p, focused, hovered));
                let label = f.options()[f.get(&self.draft)];
                let text_r = RECT {
                    left: w.rect.left + self.m.gap,
                    right: w.rect.right - self.m.row_h,
                    ..w.rect
                };
                text_in(hdc, label, &text_r, p.text, DT_LEFT_VC);
                let chev = RECT {
                    left: w.rect.right - self.m.row_h,
                    ..w.rect
                };
                text_in(hdc, "\u{25be}", &chev, p.text_dim, DT_CENTER_VC);
            }
            Ctl::Step(f) => {
                let cell = self.m.ctl_h;
                fill(hdc, &w.rect, self.th.field_bg);
                frame(hdc, &w.rect, border_of(p, focused, hovered));
                let minus = RECT {
                    right: w.rect.left + cell,
                    ..w.rect
                };
                let plus = RECT {
                    left: w.rect.right - cell,
                    ..w.rect
                };
                text_in(hdc, "\u{2212}", &minus, p.text, DT_CENTER_VC);
                text_in(hdc, "+", &plus, p.text, DT_CENTER_VC);
                let value = f.format(f.get(&self.draft));
                let mid = RECT {
                    left: minus.right,
                    right: plus.left,
                    ..w.rect
                };
                text_in(hdc, &value, &mid, p.text, DT_CENTER_VC);
            }
            Ctl::Button(b) => {
                let bg = if !w.enabled {
                    None
                } else if hovered {
                    Some(p.btn_hover)
                } else if b == Btn::Ok {
                    Some(p.btn_active)
                } else {
                    None
                };
                if let Some(c) = bg {
                    fill(hdc, &w.rect, c);
                }
                frame(hdc, &w.rect, border_of(p, focused, false));
                let fg = if !w.enabled {
                    p.text_dim
                } else if b == Btn::Ok && !hovered {
                    p.btn_active_text
                } else {
                    p.text
                };
                text_in(hdc, &w.text, &w.rect, fg, DT_CENTER_VC);
            }
            Ctl::KeyRow(action) => {
                if hovered || focused {
                    fill(hdc, &w.rect, p.btn_hover);
                }
                let label_r = RECT {
                    left: self.m.pad,
                    right: self.m.pad + self.m.label_w,
                    ..w.rect
                };
                text_in(hdc, &w.text, &label_r, p.text, DT_LEFT_VC);

                let capturing = self.capture == Some(action);
                let bx = self.m.pad + self.m.label_w;
                let box_r = RECT {
                    left: bx,
                    top: w.rect.top + (self.m.row_h - self.m.ctl_h) / 2,
                    right: bx + self.m.key_w,
                    bottom: w.rect.top + (self.m.row_h - self.m.ctl_h) / 2 + self.m.ctl_h,
                };
                fill(hdc, &box_r, self.th.field_bg);
                frame(
                    hdc,
                    &box_r,
                    if capturing {
                        p.btn_active
                    } else {
                        border_of(p, focused, hovered)
                    },
                );
                let chords = self.keys.chords(action);
                let (text, color) = if capturing {
                    ("Press a key\u{2026}".to_string(), p.text_dim)
                } else if chords.is_empty() {
                    ("\u{2014}".to_string(), p.text_dim)
                } else {
                    (
                        chords
                            .iter()
                            .map(|c| c.display())
                            .collect::<Vec<_>>()
                            .join(", "),
                        p.text,
                    )
                };
                let tr = RECT {
                    left: box_r.left + self.m.gap,
                    right: box_r.right - self.m.gap / 2,
                    ..box_r
                };
                text_in(hdc, &text, &tr, color, DT_LEFT_VC);

                // Reset glyph, only when this row differs from the shipped binding.
                if !self.is_default_binding(action) {
                    let reset = RECT {
                        left: w.rect.right - self.m.row_h,
                        right: w.rect.right,
                        ..w.rect
                    };
                    text_in(hdc, "\u{21ba}", &reset, p.text_dim, DT_CENTER_VC);
                }
            }
            Ctl::TreeRow(idx) => {
                let Some(row) = self.tree.get(idx) else { return };
                let selected = self.tree_sel.as_ref() == Some(&row.path);
                // Adjacent rows share the field fill, so the list reads as one sunken panel.
                if selected {
                    fill(hdc, &w.rect, p.btn_active);
                } else if hovered || focused {
                    fill(hdc, &w.rect, p.btn_hover);
                } else {
                    fill(hdc, &w.rect, self.th.field_bg);
                }
                let indent = w.rect.left + self.m.gap + row.depth as i32 * self.m.scale(18);
                let r = RECT {
                    left: indent,
                    right: w.rect.right - self.m.gap,
                    ..w.rect
                };
                let fg = if selected {
                    p.btn_active_text
                } else {
                    p.text
                };
                let label = if row.submenu {
                    format!("{}  \u{25b8}", w.text)
                } else {
                    w.text.clone()
                };
                text_in(hdc, &label, &r, fg, DT_LEFT_VC);
            }
            Ctl::Text(f) => {
                fill(hdc, &w.rect, self.th.field_bg);
                frame(hdc, &w.rect, border_of(p, focused, hovered));
                let editing = self.edit.as_ref().filter(|(ef, _, _)| *ef == f);
                let owned;
                let text: &str = match editing {
                    Some((_, buf, _)) => buf,
                    None => {
                        owned = self
                            .tree_sel
                            .clone()
                            .and_then(|path| {
                                // `paint` is &self, so read the entry immutably.
                                entry_ref(&self.draft.open_with, &path).map(|e| f.get(e))
                            })
                            .unwrap_or_default();
                        &owned
                    }
                };
                let tr = RECT {
                    left: w.rect.left + self.m.gap,
                    right: w.rect.right - self.m.gap,
                    ..w.rect
                };
                let flags = if f == TextField::Program {
                    DT_LEFT_VC | DT_PATH_ELLIPSIS
                } else {
                    DT_LEFT_VC | DT_END_ELLIPSIS
                };
                text_in(hdc, text, &tr, p.text, flags);
                // Caret: a 1px bar after the typed prefix (no selection support — Browse… covers the
                // long-path case that would need it).
                if let Some((_, buf, caret)) = editing {
                    let upto: String = buf.chars().take(*caret).collect();
                    let cx = (tr.left + text_width(hdc, &upto)).min(tr.right);
                    fill(
                        hdc,
                        &RECT {
                            left: cx,
                            top: tr.top + self.m.scale(4),
                            right: cx + 1,
                            bottom: tr.bottom - self.m.scale(4),
                        },
                        p.text,
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Win32 plumbing
// ---------------------------------------------------------------------------------------------

/// Same panic firewall as the frame/view procs: a panic must never unwind into the Win32 dispatcher.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        wndproc_impl(hwnd, msg, wp, lp)
    })) {
        Ok(lr) => lr,
        Err(_) => {
            eprintln!("fire: recovered from a panic in the settings wndproc (msg {msg:#06x})");
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

unsafe fn wndproc_impl(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Dialog;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let d = &mut *ptr;

    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            let (w, h) = d.client();
            if w > 0 && h > 0 {
                // Double-buffered, like the frame's chrome: the dialog paints back-to-front.
                let mem = CreateCompatibleDC(ps.hdc);
                let bmp = CreateCompatibleBitmap(ps.hdc, w, h);
                let old = SelectObject(mem, bmp as HGDIOBJ);
                d.paint(mem);
                BitBlt(ps.hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
                SelectObject(mem, old);
                DeleteObject(bmp as HGDIOBJ);
                DeleteDC(mem);
            }
            EndPaint(hwnd, &ps);
            0
        }
        WM_LBUTTONDOWN => {
            d.click(lo_i16(lp), hi_i16(lp));
            0
        }
        WM_MOUSEMOVE => {
            d.set_hover(lo_i16(lp), hi_i16(lp));
            0
        }
        WM_MOUSELEAVE => {
            d.tracking_leave = false;
            if d.hover.take().is_some() {
                d.invalidate();
            }
            0
        }
        WM_MOUSEWHEEL => {
            let notches = ((wp >> 16) & 0xffff) as u16 as i16 as i32 / 120;
            d.scroll_by(-notches * d.m.row_h * 2);
            0
        }
        WM_KEYDOWN => {
            d.key(wp as u32);
            0
        }
        // Alt chords never arrive as WM_KEYDOWN — the keybind capture needs them here.
        WM_SYSKEYDOWN => {
            if d.capture.is_some() {
                d.capture_key(wp as u32);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_CHAR => {
            if let Some(ch) = char::from_u32(wp as u32) {
                if d.edit.is_some() && d.edit_char(ch) {
                    d.invalidate();
                    return 0;
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_DPICHANGED => {
            let new_dpi = (wp & 0xffff) as u32;
            let prc = lp as *const RECT;
            if !prc.is_null() {
                let r = *prc;
                SetWindowPos(
                    hwnd,
                    ptr::null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            d.m = Metrics::new(new_dpi.max(96));
            d.refresh();
            0
        }
        // The frame re-skins itself on a theme flip; do the same rather than sit there in the old
        // colors for as long as the dialog is open.
        WM_SETTINGCHANGE => {
            let dark = chrome::system_uses_dark_mode();
            if dark != d.dark {
                d.dark = dark;
                d.th = Theme::new(dark);
                chrome::apply_dark_titlebar(hwnd, dark);
                chrome::apply_dark_menus(hwnd, dark);
                d.invalidate();
            }
            0
        }
        WM_CLOSE => {
            d.close();
            0
        }
        WM_DESTROY => 0,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

const DT_LEFT_VC: u32 = DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
const DT_CENTER_VC: u32 =
    windows_sys::Win32::Graphics::Gdi::DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;

fn text_in(hdc: HDC, s: &str, r: &RECT, color: u32, flags: u32) {
    let mut rc = *r;
    unsafe { SetTextColor(hdc, color) };
    draw_text(hdc, s, &mut rc, flags);
}

/// A 1px border (four fills — no pen/brush juggling).
fn frame(hdc: HDC, r: &RECT, color: u32) {
    let top = RECT {
        bottom: r.top + 1,
        ..*r
    };
    let bottom = RECT {
        top: r.bottom - 1,
        ..*r
    };
    let left = RECT {
        right: r.left + 1,
        ..*r
    };
    let right = RECT {
        left: r.right - 1,
        ..*r
    };
    for e in [top, bottom, left, right] {
        fill(hdc, &e, color);
    }
}

fn border_of(p: &Palette, focused: bool, hovered: bool) -> u32 {
    if focused {
        p.btn_active
    } else if hovered {
        p.text_dim
    } else {
        p.border
    }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn center(r: &RECT) -> (i32, i32) {
    ((r.left + r.right) / 2, (r.top + r.bottom) / 2)
}

/// The immutable twin of [`model::entry_at`], for the paint path.
fn entry_ref<'a>(
    root: &'a [crate::config::MenuEntry],
    path: &[usize],
) -> Option<&'a crate::config::MenuEntry> {
    let mut cur = root.get(*path.first()?)?;
    for &i in &path[1..] {
        cur = cur.items.get(i)?;
    }
    Some(cur)
}

/// The byte offset of character `n` (the caret's index is in chars, the buffer's in bytes).
fn byte_at(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

fn key_down(vk: i32) -> bool {
    (unsafe { GetKeyState(vk) } as u16 & 0x8000) != 0
}

/// Text width in the dialog font (layout runs before a paint DC exists, so it takes its own).
fn text_w(hwnd: HWND, font: HFONT, s: &str) -> i32 {
    unsafe {
        let hdc = GetDC(hwnd);
        if hdc.is_null() {
            return 0;
        }
        let prev = SelectObject(hdc, font as HGDIOBJ);
        let w = text_width(hdc, s);
        SelectObject(hdc, prev);
        ReleaseDC(hwnd, hdc);
        w
    }
}

fn lo_i16(lp: LPARAM) -> i32 {
    (lp & 0xffff) as u16 as i16 as i32
}

fn hi_i16(lp: LPARAM) -> i32 {
    ((lp >> 16) & 0xffff) as u16 as i16 as i32
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The common Open dialog, filtered to executables — the "Browse…" button behind an open-with
/// entry's program path. Pumps its own modal loop inside ours, which is fine (so does the actions
/// popup inside the main loop).
fn browse_for_program(owner: HWND) -> Option<PathBuf> {
    let mut filter: Vec<u16> = Vec::new();
    for s in ["Programs", "*.exe;*.com;*.bat;*.cmd", "All files", "*.*"] {
        filter.extend(s.encode_utf16());
        filter.push(0);
    }
    filter.push(0);

    let mut buf = vec![0u16; 4096];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY;
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(std::ffi::OsString::from(
        String::from_utf16_lossy(&buf[..end]),
    )))
}
