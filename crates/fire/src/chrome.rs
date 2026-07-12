//! The UI's shared model and theme tokens, plus the Win32 dark-mode plumbing.
//!
//! This module used to *paint* the toolbar and status bar with GDI. It no longer does: the whole UI
//! is Dear ImGui now (see [`crate::ui`]), which owns layout, hit-testing, hover, focus and
//! scrolling. What survives here is the part that was never really about GDI:
//!
//! * [`Action`] and [`ViewSnapshot`] — the command vocabulary and the read-only state the UI renders
//!   from. Pure data; [`crate::ui`] reads them, the win shell applies them.
//! * [`Palette`] — the light/dark color tokens, whose highlight is the user's **system accent**
//!   ([`system_accent`], read from `COLOR_HIGHLIGHT`: documented, no registry poking).
//! * The dark title bar / dark menus plumbing, which is a window-manager concern, not a paint one.
//!
//! The GDI text helpers at the bottom are the last of it: they exist only for the settings dialog
//! ([`crate::settings`]), which is still a hand-painted Win32 window. They go away with it.

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows_sys::Win32::Graphics::Gdi::{GetSysColor, COLOR_HIGHLIGHT};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

use crate::icons::Icon;
use crate::keybinds::{KeyAction, ShortcutLabels};
use crate::render::view::{Background, Channel, Tonemap};

/// A UI command, produced by the toolbar (or a menu) and consumed by the win shell.
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
    /// Exposure +/- one step, or reset to 0 EV (HDR only).
    ExpUp,
    ExpReset,
    ExpDown,
    /// Toggle the 1px image-boundary outline (right-side group).
    ToggleOutline,
    /// Choose the viewport backdrop (right-side group).
    Background(Background),
    /// Enter/leave borderless full-screen (Esc or middle-click over the viewport also toggle it).
    ToggleFullscreen,
    /// Enter/leave flipbook (sprite-sheet) viewer mode (K); disabled for animated sources.
    ToggleFlipbook,
    /// The far-right button: open the actions popup menu — file actions (show in folder, copy
    /// file / path / name) plus any configured "Open in…" external apps.
    OpenWithMenu,
    /// Open the settings dialog ([`crate::settings`]).
    OpenSettings,
    /// A synthetic "»" button that only appears when the left group can't fit the window width: it
    /// opens a popup listing the buttons that were dropped.
    Overflow,
}

/// A live read-only view of display state, built by the win shell each frame so the UI can render
/// the correct button states and status text without reaching into the surface.
pub struct ViewSnapshot {
    pub channel: Channel,
    pub fit: bool,
    pub tonemap: Tonemap,
    pub is_hdr: bool,
    pub has_image: bool,
    /// True between an open request and its decode landing. With `!has_image` it distinguishes
    /// "still loading" (show nothing) from "empty" (show the drop hint).
    pub loading: bool,
    /// Source carries a real alpha channel (drives the RGB↔RGBA icon).
    pub has_alpha: bool,
    pub background: Background,
    /// Image-boundary outline is on (drives the toggle button's highlight).
    pub outline: bool,
    /// A folder cursor with more than one image exists (enables ←/→).
    pub can_navigate: bool,
    /// The window is currently in borderless full-screen (drives the toggle's highlight).
    pub fullscreen: bool,
    /// Flipbook viewer mode is active for the current image (drives the toggle's highlight).
    pub flipbook: bool,
    /// The current image is an animated source (GIF) — flipbook mode is disabled for it.
    pub has_animation: bool,
    /// The live keyboard shortcuts, so a button's tooltip shows the key that *currently* drives it
    /// rather than a literal baked into the string (the settings dialog can rebind any of them).
    pub shortcuts: ShortcutLabels,
    pub status_left: String,
    pub status_right: String,
}

impl ViewSnapshot {
    /// Whether a button is interactive in the current state (others are drawn dimmed).
    pub(crate) fn enabled(&self, a: Action) -> bool {
        match a {
            Action::Prev | Action::Next => self.can_navigate,
            Action::ZoomOut | Action::ZoomIn | Action::ZoomToggle => self.has_image,
            Action::Channel(_) | Action::Background(_) | Action::ToggleOutline => self.has_image,
            Action::ToggleTonemap | Action::ExpUp | Action::ExpReset | Action::ExpDown => {
                self.is_hdr
            }
            // The actions menu (copy / show in folder / open in app) needs an image to act on.
            Action::OpenWithMenu => self.has_image,
            // Full-screen is a window mode, independent of whether an image is loaded. So are the
            // settings — you can configure Fire from an empty window.
            Action::ToggleFullscreen | Action::OpenSettings => true,
            // Flipbook needs a still image (a GIF is already an animation, not a sprite sheet).
            Action::ToggleFlipbook => self.has_image && !self.has_animation,
            // The overflow button is only ever laid out when it holds dropped controls; opening its
            // menu is always allowed (the individual entries carry their own enabled state).
            Action::Overflow => true,
        }
    }

    /// Whether a toggle button is in its "on" state (drawn filled with the accent). Momentary
    /// buttons (navigation, zoom steps, the fit/1:1 toggle, exposure) never latch.
    pub(crate) fn active(&self, a: Action) -> bool {
        match a {
            Action::Channel(c) => self.channel == c,
            Action::ToggleTonemap => self.tonemap == Tonemap::Aces,
            Action::Background(b) => self.background == b,
            Action::ToggleOutline => self.outline,
            Action::ToggleFullscreen => self.fullscreen,
            Action::ToggleFlipbook => self.flipbook,
            Action::Overflow => false,
            _ => false,
        }
    }

    /// Hover-tooltip text for a button. State-aware where the button is (the zoom toggle describes
    /// the mode a click switches *to*); the parenthetical is the button's *current* keyboard
    /// shortcut, read from [`Self::shortcuts`] — a rebound key relabels its button, and an unbound
    /// action shows no parenthetical at all.
    pub(crate) fn tooltip(&self, a: Action) -> String {
        let k = |ka: KeyAction| self.shortcuts.suffix(ka);
        match a {
            Action::Prev => format!("Previous image{}", k(KeyAction::PrevImage)),
            Action::Next => format!("Next image{}", k(KeyAction::NextImage)),
            Action::ZoomOut => format!("Zoom out{}", k(KeyAction::ZoomOut)),
            Action::ZoomIn => format!("Zoom in{}", k(KeyAction::ZoomIn)),
            Action::ZoomToggle => {
                if self.fit {
                    format!("Actual size 1:1{}", k(KeyAction::ActualSize))
                } else {
                    format!("Fit to window{}", k(KeyAction::Fit))
                }
            }
            Action::Channel(Channel::Rgb) => format!("All channels{}", k(KeyAction::ChannelRgb)),
            Action::Channel(Channel::R) => format!("Red channel{}", k(KeyAction::ChannelR)),
            Action::Channel(Channel::G) => format!("Green channel{}", k(KeyAction::ChannelG)),
            Action::Channel(Channel::B) => format!("Blue channel{}", k(KeyAction::ChannelB)),
            Action::Channel(Channel::A) => format!("Alpha channel{}", k(KeyAction::ChannelA)),
            Action::ToggleTonemap => format!(
                "Tone map: Reinhard \u{2194} ACES{}",
                k(KeyAction::ToggleTonemap)
            ),
            Action::ExpUp => format!("Increase exposure{}", k(KeyAction::ExposureUp)),
            Action::ExpReset => format!("Reset exposure{}", k(KeyAction::ExposureReset)),
            Action::ExpDown => format!("Decrease exposure{}", k(KeyAction::ExposureDown)),
            Action::ToggleOutline => {
                format!("Image boundary outline{}", k(KeyAction::ToggleOutline))
            }
            Action::Background(Background::Black) => "Black backdrop".into(),
            Action::Background(Background::White) => "White backdrop".into(),
            Action::Background(Background::Grey) => "Grey backdrop".into(),
            Action::Background(Background::Checker) => "Checkerboard backdrop".into(),
            Action::OpenWithMenu => "Copy, show in folder, or open in app\u{2026}".into(),
            Action::ToggleFullscreen => format!("Full screen{}", k(KeyAction::ToggleFullscreen)),
            Action::ToggleFlipbook => format!("Flipbook mode{}", k(KeyAction::ToggleFlipbook)),
            Action::Overflow => "More controls\u{2026}".into(),
            Action::OpenSettings => "Settings\u{2026}".into(),
        }
    }

    /// The icon for a button — a couple of which depend on live state: the zoom toggle shows the
    /// mode a click switches *to*, and the all-channels button reflects alpha presence.
    pub(crate) fn icon(&self, a: Action) -> Icon {
        match a {
            Action::Prev => Icon::Left,
            Action::Next => Icon::Right,
            Action::ZoomOut => Icon::ZoomOut,
            Action::ZoomIn => Icon::ZoomIn,
            Action::ZoomToggle => {
                if self.fit {
                    Icon::OneToOne
                } else {
                    Icon::Fit
                }
            }
            Action::Channel(Channel::Rgb) => {
                if self.has_alpha {
                    Icon::Rgba
                } else {
                    Icon::Rgb
                }
            }
            Action::Channel(Channel::R) => Icon::R,
            Action::Channel(Channel::G) => Icon::G,
            Action::Channel(Channel::B) => Icon::B,
            Action::Channel(Channel::A) => Icon::A,
            Action::ToggleTonemap => Icon::Aces,
            Action::ExpUp => Icon::EvUp,
            Action::ExpReset => Icon::EvReset,
            Action::ExpDown => Icon::EvDown,
            Action::ToggleOutline => Icon::Outline,
            Action::Background(Background::Black) => Icon::B,
            Action::Background(Background::White) => Icon::White,
            Action::Background(Background::Grey) => Icon::G,
            Action::Background(Background::Checker) => Icon::Checker,
            Action::OpenWithMenu => Icon::OpenWith,
            Action::ToggleFullscreen => Icon::Fullscreen,
            Action::ToggleFlipbook => Icon::Flipbook,
            Action::OpenSettings => Icon::Settings,
            Action::Overflow => Icon::More,
        }
    }
}

// --- palette ----------------------------------------------------------------

/// Light/dark color set for fire's **chrome** (the toolbar, status bar and transport). Values are GDI
/// `COLORREF` (`0x00BBGGRR`); [`crate::ui::theme`] is the only consumer and converts them to ImGui's
/// float RGBA. The settings window deliberately does *not* use this — it wears ImGui's stock style
/// (see [`crate::ui::settings`]).
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) toolbar_bg: u32,
    pub(crate) status_bg: u32,
    pub(crate) text: u32,
    pub(crate) text_dim: u32,
    pub(crate) btn_hover: u32,
    pub(crate) btn_active: u32,
    pub(crate) separator: u32,
    pub(crate) border: u32,
    /// Letterbox / no-image backdrop.
    view_clear: u32,
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// The blue we fall back to when the system highlight looks unusable (see [`system_accent`]).
const DEFAULT_ACCENT: u32 = rgb(0, 120, 215);

/// The user's accent color, as a GDI `COLORREF`.
///
/// `COLOR_HIGHLIGHT` is the *documented* way to read it: Windows 10/11 set the highlight (selection)
/// color from the accent the user picks in Settings, so this tracks it with no undocumented API and
/// no registry poking. It also does the right thing under a high-contrast theme, where the highlight
/// is whatever that theme says it is.
///
/// A near-black or near-white highlight would make our accent-filled buttons unreadable against
/// their own text, so those degenerate values fall back to the shipped blue.
pub fn system_accent() -> u32 {
    let c = unsafe { GetSysColor(COLOR_HIGHLIGHT) };
    let l = luminance(c);
    if !(24.0..=232.0).contains(&l) {
        return DEFAULT_ACCENT;
    }
    c
}

/// Perceptual-ish brightness of a `COLORREF`, 0..255. Rec. 709 weights on the raw sRGB bytes.
fn luminance(c: u32) -> f32 {
    let (r, g, b) = (
        (c & 0xFF) as f32,
        ((c >> 8) & 0xFF) as f32,
        ((c >> 16) & 0xFF) as f32,
    );
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

impl Palette {
    pub(crate) fn for_mode(dark: bool) -> Self {
        let accent = system_accent();
        if dark {
            Palette {
                toolbar_bg: rgb(43, 43, 43),
                status_bg: rgb(30, 30, 30),
                text: rgb(224, 224, 224),
                text_dim: rgb(140, 140, 140),
                btn_hover: rgb(60, 60, 60),
                btn_active: accent,
                separator: rgb(60, 60, 60),
                border: rgb(70, 70, 70),
                view_clear: rgb(0, 0, 0),
            }
        } else {
            Palette {
                toolbar_bg: rgb(240, 240, 240),
                status_bg: rgb(230, 230, 230),
                text: rgb(20, 20, 20),
                text_dim: rgb(110, 110, 110),
                btn_hover: rgb(214, 214, 214),
                btn_active: accent,
                separator: rgb(200, 200, 200),
                border: rgb(170, 170, 170),
                view_clear: rgb(150, 150, 150),
            }
        }
    }

    /// The no-image backdrop as `0x00RRGGBB` packing (COLORREF is `0x00BBGGRR`), which is what the
    /// GPU surface's clear color wants.
    pub(crate) fn view_clear_packed(&self) -> u32 {
        let c = self.view_clear;
        ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
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

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// There used to be an `apply_dark_menus` here: three undocumented `uxtheme.dll` ordinals (133/135/136
// — `AllowDarkModeForWindow` / `SetPreferredAppMode` / `FlushMenuThemes`), resolved by `GetProcAddress`
// and `transmute`d into function pointers. It was the only undocumented API in the codebase, and it
// existed for exactly one reason: a `TrackPopupMenu` menu is drawn by the system — border, gutter,
// rounded corners and all — so nothing short of that hack could make it follow the app's dark theme.
// The menus are ImGui popups now. We draw them; they are whatever color we say.
