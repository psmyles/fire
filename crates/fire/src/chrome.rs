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
    SelectObject, SetBkMode, SetTextColor, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, HDC, HFONT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

use crate::icons::{Icon, Icons};
use crate::render::view::{Background, Channel, Tonemap};

/// A toolbar command, produced by hit-testing a click and consumed by the win shell.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Previous / next sibling image in the folder (←/→).
    Prev,
    Next,
    ZoomOut,
    ZoomIn,
    /// Single button toggling fit-to-window ↔ 1:1 (the icon shows what a click will do).
    ZoomToggle,
    /// Set/toggle channel isolation (RGB resets; R/G/B/A solo-toggle).
    Channel(Channel),
    /// Reinhard ↔ ACES (HDR only).
    ToggleTonemap,
    /// Exposure +/- one step (HDR only).
    ExpUp,
    ExpDown,
    /// Toggle the 1px image-boundary outline (right-side group).
    ToggleOutline,
    /// Choose the viewport backdrop (right-side group).
    Background(Background),
    /// Enter/leave borderless full-screen (Esc or middle-click over the viewport also toggle it).
    ToggleFullscreen,
    /// The far-right button: open the actions popup menu — file actions (show in folder, copy
    /// file / path / name) plus any configured "Open in…" external apps. The menu itself is built
    /// and tracked by the win shell (it needs the button's screen rect, and the chosen entry is
    /// performed there directly — no per-entry `Action`).
    OpenWithMenu,
}

/// A toolbar slot: the action it performs and a visual group (groups are separated by a thin
/// divider). Slots are laid out into [`LaidButton`]s by [`Chrome::relayout`].
struct Slot {
    action: Action,
    group: u8,
}

/// The HDR group (tonemap + exposure). Laid out only for float sources; hidden otherwise so the
/// toolbar doesn't carry inert controls for LDR images.
const HDR_GROUP: u8 = 3;

/// Left-docked controls, in order: navigate · zoom · channel isolation · HDR (float only).
const LEFT: &[Slot] = &[
    Slot { action: Action::Prev, group: 0 },
    Slot { action: Action::Next, group: 0 },
    Slot { action: Action::ZoomOut, group: 1 },
    Slot { action: Action::ZoomToggle, group: 1 },
    Slot { action: Action::ZoomIn, group: 1 },
    Slot { action: Action::Channel(Channel::Rgb), group: 2 },
    Slot { action: Action::Channel(Channel::R), group: 2 },
    Slot { action: Action::Channel(Channel::G), group: 2 },
    Slot { action: Action::Channel(Channel::B), group: 2 },
    Slot { action: Action::Channel(Channel::A), group: 2 },
    Slot { action: Action::ToggleTonemap, group: HDR_GROUP },
    Slot { action: Action::ExpUp, group: HDR_GROUP },
    Slot { action: Action::ExpDown, group: HDR_GROUP },
];

/// Right-docked controls: the outline toggle, then the viewport backdrop group. Drawn far-right,
/// in this visual (left→right) order; a divider separates the groups like the left side.
const RIGHT: &[Slot] = &[
    Slot { action: Action::ToggleOutline, group: 0 },
    Slot { action: Action::Background(Background::Black), group: 1 },
    Slot { action: Action::Background(Background::White), group: 1 },
    Slot { action: Action::Background(Background::Grey), group: 1 },
    Slot { action: Action::Background(Background::Checker), group: 1 },
    // The full-screen toggle sits just left of the "Open in…" button, in its own group (dividers
    // on both sides).
    Slot { action: Action::ToggleFullscreen, group: 3 },
    // Laid first (RIGHT is walked in reverse), so the "Open in…" button hugs the far-right corner;
    // its own group gives it a divider from the backdrop controls.
    Slot { action: Action::OpenWithMenu, group: 2 },
];

/// A live read-only view of display state, built by the win shell each paint so the chrome
/// can render the correct button states and status text without reaching into the surface.
pub struct ViewSnapshot {
    pub channel: Channel,
    pub fit: bool,
    pub tonemap: Tonemap,
    pub is_hdr: bool,
    pub has_image: bool,
    /// Source carries a real alpha channel (drives the RGB↔RGBA icon).
    pub has_alpha: bool,
    pub background: Background,
    /// Image-boundary outline is on (drives the toggle button's highlight).
    pub outline: bool,
    /// A folder cursor with more than one image exists (enables ←/→).
    pub can_navigate: bool,
    /// The window is currently in borderless full-screen (drives the toggle's highlight).
    pub fullscreen: bool,
    pub status_left: String,
    pub status_right: String,
}

impl ViewSnapshot {
    /// Whether a button is interactive in the current state (others are drawn dimmed).
    fn enabled(&self, a: Action) -> bool {
        match a {
            Action::Prev | Action::Next => self.can_navigate,
            Action::ZoomOut | Action::ZoomIn | Action::ZoomToggle => self.has_image,
            Action::Channel(_) | Action::Background(_) | Action::ToggleOutline => self.has_image,
            Action::ToggleTonemap | Action::ExpUp | Action::ExpDown => self.is_hdr,
            // The actions menu (copy / show in folder / open in app) needs an image to act on; the
            // file actions are always available, so a configured app list is no longer required.
            Action::OpenWithMenu => self.has_image,
            // Full-screen is a window mode, independent of whether an image is loaded.
            Action::ToggleFullscreen => true,
        }
    }

    /// Whether a toggle button is in its "on" state (drawn highlighted). Momentary buttons
    /// (navigation, zoom steps, the fit/1:1 toggle, exposure) never latch.
    fn active(&self, a: Action) -> bool {
        match a {
            Action::Channel(c) => self.channel == c,
            Action::ToggleTonemap => self.tonemap == Tonemap::Aces,
            Action::Background(b) => self.background == b,
            Action::ToggleOutline => self.outline,
            Action::ToggleFullscreen => self.fullscreen,
            _ => false,
        }
    }

    /// Hover-tooltip text for a button. State-aware where the button is (the zoom toggle
    /// describes the mode a click switches *to*); the parenthetical is the keyboard shortcut.
    fn tooltip(&self, a: Action) -> &'static str {
        match a {
            Action::Prev => "Previous image  (\u{2190})",
            Action::Next => "Next image  (\u{2192})",
            Action::ZoomOut => "Zoom out  (\u{2212})",
            Action::ZoomIn => "Zoom in  (+)",
            Action::ZoomToggle => if self.fit { "Actual size 1:1  (1)" } else { "Fit to window  (F)" },
            Action::Channel(Channel::Rgb) => "All channels  (C)",
            Action::Channel(Channel::R) => "Red channel  (R)",
            Action::Channel(Channel::G) => "Green channel  (G)",
            Action::Channel(Channel::B) => "Blue channel  (B)",
            Action::Channel(Channel::A) => "Alpha channel  (A)",
            Action::ToggleTonemap => "Tone map: Reinhard \u{2194} ACES  (T)",
            Action::ExpUp => "Increase exposure  (])",
            Action::ExpDown => "Decrease exposure  ([)",
            Action::ToggleOutline => "Image boundary outline",
            Action::Background(Background::Black) => "Black backdrop",
            Action::Background(Background::White) => "White backdrop",
            Action::Background(Background::Grey) => "Grey backdrop",
            Action::Background(Background::Checker) => "Checkerboard backdrop",
            Action::OpenWithMenu => "Copy, show in folder, or open in app\u{2026}",
            Action::ToggleFullscreen => "Full screen  (F11)",
        }
    }

    /// The icon to draw for a button — a couple of which depend on live state: the zoom toggle
    /// shows the mode a click switches *to*, and the all-channels button reflects alpha presence.
    fn icon(&self, a: Action) -> Icon {
        match a {
            Action::Prev => Icon::Left,
            Action::Next => Icon::Right,
            Action::ZoomOut => Icon::ZoomOut,
            Action::ZoomIn => Icon::ZoomIn,
            Action::ZoomToggle => if self.fit { Icon::OneToOne } else { Icon::Fit },
            Action::Channel(Channel::Rgb) => if self.has_alpha { Icon::Rgba } else { Icon::Rgb },
            Action::Channel(Channel::R) => Icon::R,
            Action::Channel(Channel::G) => Icon::G,
            Action::Channel(Channel::B) => Icon::B,
            Action::Channel(Channel::A) => Icon::A,
            Action::ToggleTonemap => Icon::Aces,
            Action::ExpUp => Icon::EvUp,
            Action::ExpDown => Icon::EvDown,
            Action::ToggleOutline => Icon::Outline,
            Action::Background(Background::Black) => Icon::B,
            Action::Background(Background::White) => Icon::White,
            Action::Background(Background::Grey) => Icon::G,
            Action::Background(Background::Checker) => Icon::Checker,
            Action::OpenWithMenu => Icon::OpenWith,
            Action::ToggleFullscreen => Icon::Fullscreen,
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
                view_clear: rgb(0, 0, 0),
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

/// DPI-scaled chrome metrics (logical values are 96-dpi pixels). Owns the UI font (status bar).
pub struct Metrics {
    pub dpi: u32,
    pub toolbar_h: i32,
    pub status_h: i32,
    /// Square button edge.
    btn: i32,
    /// Icon edge (centered in the button); also the icon-mask render size.
    icon: i32,
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
            btn: s(28),
            icon: s(20),
            gap: s(4),
            sep: s(14),
            margin: s(8),
            font: create_ui_font(dpi),
        }
    }
}

/// A laid-out toolbar button (rect in frame-client coords + the action it fires).
struct LaidButton {
    rect: RECT,
    action: Action,
}

/// The frame's chrome: metrics, palette, the per-DPI icon renderer, the laid-out toolbar, and
/// the hovered button.
pub struct Chrome {
    pub metrics: Metrics,
    pub dark: bool,
    palette: Palette,
    icons: Icons,
    buttons: Vec<LaidButton>,
    seps: Vec<i32>,
    /// Index into `buttons` of the hovered button.
    pub hover: Option<usize>,
}

impl Chrome {
    pub fn new(dpi: u32, dark: bool) -> Self {
        let metrics = Metrics::new(dpi);
        let icons = Icons::new(metrics.icon);
        Chrome {
            metrics,
            dark,
            palette: Palette::for_mode(dark),
            icons,
            buttons: Vec::new(),
            seps: Vec::new(),
            hover: None,
        }
    }

    /// Rebuild metrics + font + icon masks for a new DPI (after a `WM_DPICHANGED`). No-op if
    /// unchanged.
    pub fn set_dpi(&mut self, dpi: u32) {
        if dpi == self.metrics.dpi {
            return;
        }
        unsafe { DeleteObject(self.metrics.font) };
        self.metrics = Metrics::new(dpi);
        self.icons.set_size(self.metrics.icon);
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

    /// Recompute button rects for the current metrics + visible button set (call on size/DPI
    /// change, and whenever HDR-ness changes so the float-only group appears/disappears). `width`
    /// is the frame client width, used to right-dock the background group. Clears the hover, which
    /// a relayout can invalidate.
    pub fn relayout(&mut self, width: i32, snap: &ViewSnapshot) {
        self.buttons.clear();
        self.seps.clear();
        self.hover = None;
        let m = &self.metrics;
        let btn_y = (m.toolbar_h - m.btn) / 2;

        // Left group: pack left→right, a divider between visual groups. The HDR group is laid out
        // only for float sources.
        let mut x = m.margin;
        let mut prev_group: Option<u8> = None;
        for slot in LEFT {
            if slot.group == HDR_GROUP && !snap.is_hdr {
                continue;
            }
            // The alpha-isolation button only makes sense for a source that has an alpha channel.
            if slot.action == Action::Channel(Channel::A) && !snap.has_alpha {
                continue;
            }
            if prev_group.is_some_and(|g| g != slot.group) {
                self.seps.push(x + m.sep / 2);
                x += m.sep;
            }
            prev_group = Some(slot.group);
            self.buttons.push(LaidButton {
                rect: RECT { left: x, top: btn_y, right: x + m.btn, bottom: btn_y + m.btn },
                action: slot.action,
            });
            x += m.btn + m.gap;
        }

        // Right group: pack right→left from the far edge so the backdrop controls hug the corner.
        // Dividers go between visual groups, mirroring the left side (here the gap is opened to
        // the *left* of each new group as we walk leftward).
        let mut rx = width - m.margin;
        let mut prev_group: Option<u8> = None;
        for slot in RIGHT.iter().rev() {
            if prev_group.is_some_and(|g| g != slot.group) {
                rx -= m.sep;
                self.seps.push(rx + m.sep / 2);
            }
            prev_group = Some(slot.group);
            let left = rx - m.btn;
            self.buttons.push(LaidButton {
                rect: RECT { left, top: btn_y, right: rx, bottom: btn_y + m.btn },
                action: slot.action,
            });
            rx = left - m.gap;
        }
    }

    /// Map a point (frame-client coords) to the button action under it, if any and enabled.
    pub fn hit_test(&self, x: i32, y: i32, snap: &ViewSnapshot) -> Option<Action> {
        let idx = self.hover_index(x, y)?;
        let action = self.buttons[idx].action;
        snap.enabled(action).then_some(action)
    }

    /// The rect (frame-client coords) of the laid button for `action`, if it's currently visible —
    /// used to anchor the "Open in…" popup menu under its button.
    pub fn button_rect_for(&self, action: Action) -> Option<RECT> {
        self.buttons.iter().find(|b| b.action == action).map(|b| b.rect)
    }

    /// The index (into `buttons`) of the button under a point, regardless of enabled state — for
    /// hover.
    pub fn hover_index(&self, x: i32, y: i32) -> Option<usize> {
        self.buttons
            .iter()
            .position(|lb| x >= lb.rect.left && x < lb.rect.right && y >= lb.rect.top && y < lb.rect.bottom)
    }

    /// The button rect (frame-client coords) and tooltip text for the button at `idx` — used to
    /// position and fill the hover tooltip. `None` if the index is stale (e.g. a relayout shrank
    /// the button set). Shown for disabled buttons too: a greyed control's label is still useful.
    pub fn button_tooltip(&self, idx: usize, snap: &ViewSnapshot) -> Option<(RECT, &'static str)> {
        let lb = self.buttons.get(idx)?;
        Some((lb.rect, snap.tooltip(lb.action)))
    }

    /// Paint the toolbar across the top of the frame client area (`width` px wide).
    pub fn paint_toolbar(&self, hdc: HDC, width: i32, snap: &ViewSnapshot) {
        let m = &self.metrics;
        let p = &self.palette;
        fill(hdc, &RECT { left: 0, top: 0, right: width, bottom: m.toolbar_h }, p.toolbar_bg);

        for &sx in &self.seps {
            let r = RECT {
                left: sx,
                top: m.toolbar_h / 2 - m.btn / 3,
                right: sx + 1,
                bottom: m.toolbar_h / 2 + m.btn / 3,
            };
            fill(hdc, &r, p.separator);
        }

        for (i, lb) in self.buttons.iter().enumerate() {
            let action = lb.action;
            let enabled = snap.enabled(action);
            let active = enabled && snap.active(action);
            let hovered = enabled && self.hover == Some(i);
            let r = lb.rect;
            // The icon tint mirrors the old text color: active (on accent), hover, normal, dimmed.
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
            let cx = (r.left + r.right) / 2;
            let cy = (r.top + r.bottom) / 2;
            self.icons.draw(hdc, snap.icon(action), cx, cy, color);
        }

        fill(hdc, &RECT { left: 0, top: m.toolbar_h - 1, right: width, bottom: m.toolbar_h }, p.border);
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

/// Make Win32 popup menus (the toolbar actions menu / the view's right-click menu) follow the
/// dark theme. Unlike the toolbar — which we GDI-paint precisely *because* common controls have no
/// documented dark mode — a `TrackPopupMenu` menu is system-drawn (border, gutter, rounded corners
/// and all), so owner-drawing only its item rects would still leave a light frame around them. The
/// one thing that darkens the whole menu is the same undocumented `uxtheme.dll` ordinal path
/// File Explorer / Terminal use: opt the process into the dark app mode (`SetPreferredAppMode`,
/// ordinal 135), mark the owner window dark-allowed (`AllowDarkModeForWindow`, ordinal 133), then
/// flush the cached menu theme (`FlushMenuThemes`, ordinal 136). Process-global and persistent, so
/// callers invoke it on theme setup / change rather than per menu. Strictly best-effort: if any
/// ordinal can't be resolved (older Windows) the menu just stays light, exactly as before.
pub fn apply_dark_menus(hwnd: HWND, dark: bool) {
    // uxtheme.dll private ordinals (stable since Win10 1809; SetPreferredAppMode since 1903).
    const ALLOW_DARK_MODE_FOR_WINDOW: usize = 133;
    const SET_PREFERRED_APP_MODE: usize = 135;
    const FLUSH_MENU_THEMES: usize = 136;
    // PreferredAppMode: Default = 0, AllowDark = 1, ForceDark = 2, ForceLight = 3.
    const APP_MODE_FORCE_DARK: i32 = 2;
    const APP_MODE_DEFAULT: i32 = 0;

    // BOOL is plain i32 in windows-sys; spell it out to avoid an import that varies by version.
    type AllowDarkModeForWindowFn = unsafe extern "system" fn(HWND, i32) -> i32;
    type SetPreferredAppModeFn = unsafe extern "system" fn(i32) -> i32;
    type FlushMenuThemesFn = unsafe extern "system" fn();

    unsafe {
        let lib = LoadLibraryW(wide("uxtheme.dll").as_ptr());
        if lib.is_null() {
            return;
        }
        // GetProcAddress by ordinal: the ordinal is passed as the pointer value (MAKEINTRESOURCE).
        if let Some(p) = GetProcAddress(lib, ALLOW_DARK_MODE_FOR_WINDOW as *const u8) {
            let f: AllowDarkModeForWindowFn = std::mem::transmute(p);
            f(hwnd, dark as i32);
        }
        if let Some(p) = GetProcAddress(lib, SET_PREFERRED_APP_MODE as *const u8) {
            let f: SetPreferredAppModeFn = std::mem::transmute(p);
            f(if dark { APP_MODE_FORCE_DARK } else { APP_MODE_DEFAULT });
        }
        if let Some(p) = GetProcAddress(lib, FLUSH_MENU_THEMES as *const u8) {
            let f: FlushMenuThemesFn = std::mem::transmute(p);
            f();
        }
        // Deliberately no FreeLibrary: uxtheme is already loaded process-wide and stays resident.
    }
}

// --- GDI helpers ------------------------------------------------------------

pub(crate) fn create_ui_font(dpi: u32) -> HFONT {
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
