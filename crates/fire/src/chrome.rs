//! GDI-painted window chrome — a docked toolbar (top) and status bar (bottom) that the
//! frame window draws itself, plus the light/dark palette, DPI-scaled metrics, and the
//! dark-mode/title-bar plumbing.
//!
//! Why custom-paint instead of the common controls the original plan named
//! (`ToolbarWindow32`/`msctls_statusbar32`): those have **no documented dark-mode support**,
//! so a coherent dark look would need undocumented `uxtheme.dll` ordinal calls. Painting the
//! chrome ourselves (still native GDI + the system font) gives full color control for both
//! themes with zero unsupported APIs, and scales cleanly with DPI because we own every metric.
//! The image view is a separate child window (D3D11 swapchain), so it is unaffected by all of this.

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND, RECT, SIZE};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, GetTextExtentPoint32W,
    SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, TRANSPARENT,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

use crate::render::view::{Channel, Tonemap};

/// A toolbar command, produced by hit-testing a click and consumed by the win shell.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Set/toggle channel isolation (RGB resets; R/G/B/A solo-toggle).
    Channel(Channel),
    Fit,
    OneToOne,
    /// Reinhard ↔ ACES (HDR only).
    ToggleTonemap,
    /// Exposure +/- one step (HDR only).
    ExpUp,
    ExpDown,
}

/// A toolbar button: a label, the action it performs, and a visual group (groups are
/// separated by a thin divider).
struct Button {
    action: Action,
    label: &'static str,
    group: u8,
}

const BUTTONS: &[Button] = &[
    Button { action: Action::Channel(Channel::Rgb), label: "RGB", group: 0 },
    Button { action: Action::Channel(Channel::R), label: "R", group: 0 },
    Button { action: Action::Channel(Channel::G), label: "G", group: 0 },
    Button { action: Action::Channel(Channel::B), label: "B", group: 0 },
    Button { action: Action::Channel(Channel::A), label: "A", group: 0 },
    Button { action: Action::Fit, label: "Fit", group: 1 },
    Button { action: Action::OneToOne, label: "1:1", group: 1 },
    Button { action: Action::ToggleTonemap, label: "ACES", group: 2 },
    Button { action: Action::ExpDown, label: "EV −", group: 2 },
    Button { action: Action::ExpUp, label: "EV +", group: 2 },
];

/// A live read-only view of display state, built by the win shell each paint so the chrome
/// can render the correct button states and status text without reaching into the surface.
pub struct ViewSnapshot {
    pub channel: Channel,
    pub fit: bool,
    pub zoom_pct: u32,
    pub tonemap: Tonemap,
    pub is_hdr: bool,
    pub has_image: bool,
    pub status_left: String,
    pub status_right: String,
}

impl ViewSnapshot {
    /// Whether a button is interactive: channel/fit/1:1 need an image; the HDR controls need
    /// a float source.
    fn enabled(&self, a: Action) -> bool {
        match a {
            Action::Channel(_) | Action::Fit | Action::OneToOne => self.has_image,
            Action::ToggleTonemap | Action::ExpUp | Action::ExpDown => self.is_hdr,
        }
    }

    /// Whether a toggle button is in its "on" state (drawn highlighted).
    fn active(&self, a: Action) -> bool {
        match a {
            Action::Channel(c) => self.channel == c,
            Action::Fit => self.fit,
            Action::OneToOne => !self.fit && self.zoom_pct == 100,
            Action::ToggleTonemap => self.tonemap == Tonemap::Aces,
            Action::ExpUp | Action::ExpDown => false,
        }
    }
}

/// Light/dark color set. All values are GDI `COLORREF` (`0x00BBGGRR`).
#[derive(Clone, Copy)]
struct Palette {
    toolbar_bg: u32,
    status_bg: u32,
    text: u32,
    text_dim: u32,
    btn_hover: u32,
    btn_active: u32,
    btn_active_text: u32,
    separator: u32,
    border: u32,
    /// Letterbox / no-image backdrop, also a COLORREF; converted to `0x00RRGGBB` packing on use.
    view_clear: u32,
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

impl Palette {
    fn for_mode(dark: bool) -> Self {
        if dark {
            Palette {
                toolbar_bg: rgb(43, 43, 43),
                status_bg: rgb(30, 30, 30),
                text: rgb(224, 224, 224),
                text_dim: rgb(128, 128, 128),
                btn_hover: rgb(60, 60, 60),
                btn_active: rgb(14, 99, 156),
                btn_active_text: rgb(255, 255, 255),
                separator: rgb(60, 60, 60),
                border: rgb(20, 20, 20),
                view_clear: rgb(32, 32, 34),
            }
        } else {
            Palette {
                toolbar_bg: rgb(240, 240, 240),
                status_bg: rgb(230, 230, 230),
                text: rgb(20, 20, 20),
                text_dim: rgb(110, 110, 110),
                btn_hover: rgb(214, 214, 214),
                btn_active: rgb(0, 120, 215),
                btn_active_text: rgb(255, 255, 255),
                separator: rgb(200, 200, 200),
                border: rgb(200, 200, 200),
                view_clear: rgb(150, 150, 150),
            }
        }
    }

    /// The backdrop as `0x00RRGGBB` packing (COLORREF is `0x00BBGGRR`).
    fn view_clear_packed(&self) -> u32 {
        let c = self.view_clear;
        ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
    }
}

/// DPI-scaled chrome metrics (logical values are 96-dpi pixels). Owns the UI font.
pub struct Metrics {
    pub dpi: u32,
    pub toolbar_h: i32,
    pub status_h: i32,
    btn_h: i32,
    btn_pad_x: i32,
    btn_min_w: i32,
    gap: i32,
    sep: i32,
    margin: i32,
    font: HFONT,
}

impl Metrics {
    fn new(dpi: u32) -> Self {
        let s = |v: i32| v * dpi as i32 / 96;
        Metrics {
            dpi,
            toolbar_h: s(36),
            status_h: s(24),
            btn_h: s(26),
            btn_pad_x: s(10),
            btn_min_w: s(26),
            gap: s(3),
            sep: s(12),
            margin: s(8),
            font: create_ui_font(dpi),
        }
    }
}

/// A laid-out toolbar button (rect in frame-client coords + index into [`BUTTONS`]).
struct LaidButton {
    rect: RECT,
    idx: usize,
}

/// The frame's chrome: metrics, palette, the laid-out toolbar, and the hovered button.
pub struct Chrome {
    pub metrics: Metrics,
    pub dark: bool,
    palette: Palette,
    buttons: Vec<LaidButton>,
    seps: Vec<i32>,
    pub hover: Option<usize>,
}

impl Chrome {
    pub fn new(dpi: u32, dark: bool) -> Self {
        Chrome {
            metrics: Metrics::new(dpi),
            dark,
            palette: Palette::for_mode(dark),
            buttons: Vec::new(),
            seps: Vec::new(),
            hover: None,
        }
    }

    /// Rebuild metrics + font for a new DPI (after a `WM_DPICHANGED`). No-op if unchanged.
    pub fn set_dpi(&mut self, dpi: u32) {
        if dpi == self.metrics.dpi {
            return;
        }
        unsafe { DeleteObject(self.metrics.font) };
        self.metrics = Metrics::new(dpi);
    }

    /// Switch palettes when the system theme changes.
    pub fn set_dark(&mut self, dark: bool) {
        self.dark = dark;
        self.palette = Palette::for_mode(dark);
    }

    /// The backdrop color for the surface, in `0x00RRGGBB` packing.
    pub fn view_clear_packed(&self) -> u32 {
        self.palette.view_clear_packed()
    }

    /// Recompute button rects for the current font (call on size/DPI change). Needs an HDC to
    /// measure text; the caller provides one with our font selectable.
    pub fn relayout(&mut self, hdc: HDC) {
        self.buttons.clear();
        self.seps.clear();
        let m = &self.metrics;
        let prev = unsafe { SelectObject(hdc, m.font) };
        let btn_y = (m.toolbar_h - m.btn_h) / 2;
        let mut x = m.margin;
        let mut prev_group: Option<u8> = None;
        for (i, b) in BUTTONS.iter().enumerate() {
            if prev_group.is_some_and(|g| g != b.group) {
                self.seps.push(x + m.sep / 2);
                x += m.sep;
            }
            prev_group = Some(b.group);
            let tw = text_width(hdc, b.label);
            let bw = (tw + 2 * m.btn_pad_x).max(m.btn_min_w);
            self.buttons.push(LaidButton {
                rect: RECT { left: x, top: btn_y, right: x + bw, bottom: btn_y + m.btn_h },
                idx: i,
            });
            x += bw + m.gap;
        }
        unsafe { SelectObject(hdc, prev) };
    }

    /// Map a point (frame-client coords) to the button action under it, if any and enabled.
    pub fn hit_test(&self, x: i32, y: i32, snap: &ViewSnapshot) -> Option<Action> {
        let idx = self.hover_index(x, y)?;
        let action = BUTTONS[idx].action;
        snap.enabled(action).then_some(action)
    }

    /// The index of the button under a point (regardless of enabled state) — for hover.
    pub fn hover_index(&self, x: i32, y: i32) -> Option<usize> {
        self.buttons
            .iter()
            .find(|lb| x >= lb.rect.left && x < lb.rect.right && y >= lb.rect.top && y < lb.rect.bottom)
            .map(|lb| lb.idx)
    }

    /// Paint the toolbar across the top of the frame client area (`width` px wide).
    pub fn paint_toolbar(&self, hdc: HDC, width: i32, snap: &ViewSnapshot) {
        let m = &self.metrics;
        let p = &self.palette;
        fill(hdc, &RECT { left: 0, top: 0, right: width, bottom: m.toolbar_h }, p.toolbar_bg);

        let prev = unsafe { SelectObject(hdc, m.font) };
        unsafe { SetBkMode(hdc, TRANSPARENT as i32) };

        for &sx in &self.seps {
            let r = RECT {
                left: sx,
                top: m.toolbar_h / 2 - m.btn_h / 3,
                right: sx + 1,
                bottom: m.toolbar_h / 2 + m.btn_h / 3,
            };
            fill(hdc, &r, p.separator);
        }

        for lb in &self.buttons {
            let action = BUTTONS[lb.idx].action;
            let enabled = snap.enabled(action);
            let active = enabled && snap.active(action);
            let hovered = enabled && self.hover == Some(lb.idx);
            let mut r = lb.rect;
            let color = if active {
                fill(hdc, &r, p.btn_active);
                p.btn_active_text
            } else if hovered {
                fill(hdc, &r, p.btn_hover);
                p.text
            } else if enabled {
                p.text
            } else {
                p.text_dim
            };
            unsafe { SetTextColor(hdc, color) };
            draw_text(hdc, BUTTONS[lb.idx].label, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        }

        fill(hdc, &RECT { left: 0, top: m.toolbar_h - 1, right: width, bottom: m.toolbar_h }, p.border);
        unsafe { SelectObject(hdc, prev) };
    }

    /// Paint the status bar across the bottom of the frame client area.
    pub fn paint_status(&self, hdc: HDC, width: i32, height: i32, snap: &ViewSnapshot) {
        let m = &self.metrics;
        let p = &self.palette;
        let top = height - m.status_h;
        fill(hdc, &RECT { left: 0, top, right: width, bottom: height }, p.status_bg);
        fill(hdc, &RECT { left: 0, top, right: width, bottom: top + 1 }, p.border);

        let prev = unsafe { SelectObject(hdc, m.font) };
        unsafe { SetBkMode(hdc, TRANSPARENT as i32) };

        // Right side first so we know how much room the (ellipsized) left side gets.
        let rw = text_width(hdc, &snap.status_right);
        unsafe { SetTextColor(hdc, p.text_dim) };
        let mut rr = RECT { left: width - m.margin - rw, top, right: width - m.margin, bottom: height };
        draw_text(hdc, &snap.status_right, &mut rr, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

        unsafe { SetTextColor(hdc, p.text) };
        let mut lr = RECT { left: m.margin, top, right: width - m.margin - rw - m.margin, bottom: height };
        draw_text(
            hdc,
            &snap.status_left,
            &mut lr,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        unsafe { SelectObject(hdc, prev) };
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        unsafe { DeleteObject(self.metrics.font) };
    }
}

// --- dark mode / title bar --------------------------------------------------

/// Read the system "apps use dark theme" preference (`AppsUseLightTheme == 0` → dark).
pub fn system_uses_dark_mode() -> bool {
    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = wide("AppsUseLightTheme");
    let mut data: u32 = 1;
    let mut size: u32 = 4;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            ptr::null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    rc == ERROR_SUCCESS && data == 0
}

/// Switch the title bar between the light and dark non-client themes (documented DWM path,
/// available on Windows 10 20H1+ / 11). Best-effort: ignored on older builds.
pub fn apply_dark_titlebar(hwnd: HWND, dark: bool) {
    let on: i32 = dark as i32;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &on as *const i32 as *const c_void,
            4,
        );
    }
}

// --- GDI helpers ------------------------------------------------------------

fn create_ui_font(dpi: u32) -> HFONT {
    // 9pt at the window's DPI; negative height = character height (excludes leading).
    let height = -(9 * dpi as i32 / 72);
    let face = wide("Segoe UI");
    unsafe {
        CreateFontW(
            height, 0, 0, 0, 400, // FW_NORMAL
            0, 0, 0, // italic / underline / strikeout
            1, // DEFAULT_CHARSET
            0, // OUT_DEFAULT_PRECIS
            0, // CLIP_DEFAULT_PRECIS
            5, // CLEARTYPE_QUALITY
            0, // DEFAULT_PITCH | FF_DONTCARE
            face.as_ptr(),
        )
    }
}

/// Fill a rect with a solid color (creates and frees a one-shot brush).
fn fill(hdc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(hdc, rect, brush);
        DeleteObject(brush);
    }
}

/// Draw a (null-terminated) string into `rect` with the given DrawText flags.
fn draw_text(hdc: HDC, s: &str, rect: &mut RECT, fmt: u32) {
    let w = wide(s);
    unsafe { DrawTextW(hdc, w.as_ptr(), -1, rect, fmt) };
}

/// Width in px of `s` using the currently-selected font.
fn text_width(hdc: HDC, s: &str) -> i32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut sz = SIZE { cx: 0, cy: 0 };
    unsafe { GetTextExtentPoint32W(hdc, w.as_ptr(), w.len() as i32, &mut sz) };
    sz.cx
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
