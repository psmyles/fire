//! A custom-painted hover tooltip for the toolbar buttons.
//!
//! Like the rest of the chrome ([`crate::chrome`]), this deliberately avoids the common-control
//! tooltip (`TOOLTIPS_CLASS`): that has no dark-mode support, so it would pop a light bubble over
//! the dark toolbar. Instead this is a tiny owned `WS_POPUP` window we GDI-paint ourselves, which
//! gives full color control for both themes and scales with DPI. It must be a separate top-level
//! window (not painted into the frame) because it floats *below* the toolbar, over the D3D11 view
//! child — `WS_CLIPCHILDREN` would clip anything the frame tried to draw there.
//!
//! The frame drives it: a hover-delay timer in the frame wndproc calls [`Tooltip::show`] with the
//! hovered button's screen rect and label; any hover change / mouse-leave / click calls
//! [`Tooltip::hide`]. The window never activates (so it can't steal the frame's focus) and is
//! click-through.

use std::sync::Once;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetTextExtentPoint32W, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, SelectObject,
    SetBkMode, SetTextColor, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, GetDC, HDC, HFONT,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, ReleaseDC, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW, LoadCursorW,
    RegisterClassW, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWLP_USERDATA, HWND_TOPMOST,
    IDC_ARROW, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::chrome::create_ui_font;

const CLASS_NAME: &str = "FireTooltipClass";
static REGISTER: Once = Once::new();

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// Tooltip colors (background, 1px border, text) for the current theme. Tooltips conventionally
/// pop a touch off the toolbar rather than matching it exactly.
fn colors(dark: bool) -> (u32, u32, u32) {
    if dark {
        (rgb(45, 45, 45), rgb(90, 90, 90), rgb(240, 240, 240))
    } else {
        (rgb(255, 255, 255), rgb(160, 160, 160), rgb(30, 30, 30))
    }
}

/// Heap state the popup wndproc reads (via `GWLP_USERDATA`) to paint itself. Owned by [`Tooltip`],
/// kept at a stable address so the raw pointer stays valid for the window's whole life.
struct TipState {
    /// Null-terminated wide label (drawn with `DrawTextW(.., -1, ..)`).
    text: Vec<u16>,
    font: HFONT,
    bg: u32,
    border: u32,
    fg: u32,
}

/// An owned popup tooltip window plus its paint state.
pub struct Tooltip {
    hwnd: HWND,
    state: *mut TipState,
    dpi: u32,
    visible: bool,
}

impl Tooltip {
    /// Create the (hidden) tooltip window owned by `owner` (the frame). Registers the window class
    /// once per process.
    pub fn new(owner: isize, dpi: u32, dark: bool) -> Self {
        let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = wide(CLASS_NAME);
        REGISTER.call_once(|| unsafe {
            RegisterClassW(&WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(tip_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            });
        });

        let (bg, border, fg) = colors(dark);
        let state = Box::into_raw(Box::new(TipState {
            text: vec![0],
            font: create_ui_font(dpi),
            bg,
            border,
            fg,
        }));

        // WS_EX_NOACTIVATE: never take focus from the frame. TOPMOST: float above the view child.
        // TRANSPARENT: click-through (the cursor sits on the toolbar above us, but stay safe).
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
                class.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                owner as HWND,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            )
        };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };

        Tooltip { hwnd, state, dpi, visible: false }
    }

    /// Rebuild the font for a new DPI (after `WM_DPICHANGED`); hides the tip.
    pub fn set_dpi(&mut self, dpi: u32) {
        if dpi == self.dpi {
            return;
        }
        self.dpi = dpi;
        self.hide();
        unsafe {
            let s = &mut *self.state;
            DeleteObject(s.font);
            s.font = create_ui_font(dpi);
        }
    }

    /// Re-skin for a theme change; hides the tip.
    pub fn set_dark(&mut self, dark: bool) {
        self.hide();
        let (bg, border, fg) = colors(dark);
        unsafe {
            let s = &mut *self.state;
            s.bg = bg;
            s.border = border;
            s.fg = fg;
        }
    }

    /// Show `text` anchored at the screen point `(x, y)` (the desired top-left), clamped to the
    /// containing monitor's work area. Sizes the window to the text plus padding.
    pub fn show(&mut self, text: &str, x: i32, y: i32) {
        let s = unsafe { &mut *self.state };
        s.text = wide(text);

        let pad_x = 8 * self.dpi as i32 / 96;
        let pad_y = 5 * self.dpi as i32 / 96;
        let (tw, th) = unsafe { measure(self.hwnd, s.font, text) };
        let w = tw + 2 * pad_x;
        let h = th + 2 * pad_y;

        let (mut x, mut y) = (x, y);
        if let Some(work) = monitor_work_area(x, y) {
            if x + w > work.right {
                x = work.right - w;
            }
            x = x.max(work.left);
            if y + h > work.bottom {
                y = work.bottom - h;
            }
            y = y.max(work.top);
        }

        unsafe {
            SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            // Force a repaint of the new label even when the size didn't change.
            InvalidateRect(self.hwnd, std::ptr::null(), 1);
        }
        self.visible = true;
    }

    /// Hide the tooltip (no-op if already hidden).
    pub fn hide(&mut self) {
        if self.visible {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            self.visible = false;
        }
    }
}

impl Drop for Tooltip {
    fn drop(&mut self) {
        unsafe {
            // Destroy the window first (no more wndproc calls), then reclaim the paint state.
            DestroyWindow(self.hwnd);
            let s = Box::from_raw(self.state);
            DeleteObject(s.font);
        }
    }
}

/// Measure `text` in `font` (px), using a screen DC for `hwnd`.
unsafe fn measure(hwnd: HWND, font: HFONT, text: &str) -> (i32, i32) {
    let hdc = GetDC(hwnd);
    let prev = SelectObject(hdc, font);
    let w: Vec<u16> = text.encode_utf16().collect();
    let mut sz = SIZE { cx: 0, cy: 0 };
    GetTextExtentPoint32W(hdc, w.as_ptr(), w.len() as i32, &mut sz);
    SelectObject(hdc, prev);
    ReleaseDC(hwnd, hdc);
    (sz.cx, sz.cy)
}

/// The work area (rcWork) of the monitor containing screen point `(x, y)`.
fn monitor_work_area(x: i32, y: i32) -> Option<RECT> {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        if mon.is_null() {
            return None;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        (GetMonitorInfoW(mon, &mut mi) != 0).then_some(mi.rcWork)
    }
}

/// Popup wndproc: paint the stored label (a bordered filled rect with centered text). Everything
/// else falls through — the window never activates, takes input, or owns a caret.
unsafe extern "system" fn tip_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_PAINT {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const TipState;
        if !state.is_null() {
            let s = &*state;
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            // 1px border = border-color fill, then the interior re-filled with the background.
            fill(ps.hdc, &rc, s.border);
            let inner = RECT { left: rc.left + 1, top: rc.top + 1, right: rc.right - 1, bottom: rc.bottom - 1 };
            fill(ps.hdc, &inner, s.bg);
            let prev = SelectObject(ps.hdc, s.font);
            SetBkMode(ps.hdc, TRANSPARENT as i32);
            SetTextColor(ps.hdc, s.fg);
            let mut tr = rc;
            DrawTextW(ps.hdc, s.text.as_ptr(), -1, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(ps.hdc, prev);
            EndPaint(hwnd, &ps);
        }
        return 0;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Fill a rect with a solid color (one-shot brush).
unsafe fn fill(hdc: HDC, rect: &RECT, color: u32) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, rect, brush);
    DeleteObject(brush);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
