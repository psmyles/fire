//! The flipbook detection hint chip: a small floating popup over the top of the image view that
//! offers to enter flipbook mode when the decoder auto-detects a sprite-sheet grid.
//!
//! Like [`crate::tooltip`] it is a separate owned `WS_POPUP` window (it floats over the D3D11 view
//! child, which `WS_CLIPCHILDREN` would otherwise clip), GDI-painted for dark-mode control. Unlike
//! the tooltip it is **interactive** — a "View as flipbook" button and a `✕` dismiss — so it is not
//! click-through (`WS_EX_TRANSPARENT` is omitted). Keeping a click from stealing the frame's focus
//! takes *both* `WS_EX_NOACTIVATE` and answering `WM_MOUSEACTIVATE` with `MA_NOACTIVATE` (see
//! [`chip_wndproc`]) — the style alone governs being shown, not being clicked.
//! Clicks post [`crate::win::WM_APP_FLIPBOOK_CHIP`] to the
//! owner frame with [`CHIP_ACCEPT`] / [`CHIP_DISMISS`] in WPARAM; the win shell owns all state
//! (which grid, whether dismissed) and just calls [`HintChip::show`] / [`HintChip::hide`].

use std::sync::Once;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, PAINTSTRUCT,
    TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW, LoadCursorW,
    PostMessageW, RegisterClassW, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWLP_USERDATA,
    HWND_TOPMOST, IDC_ARROW, MA_NOACTIVATE, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE,
    WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::chrome::create_ui_font;
use crate::flipbook::Grid;
use crate::win::WM_APP_FLIPBOOK_CHIP;

/// WPARAM values posted with [`WM_APP_FLIPBOOK_CHIP`].
pub const CHIP_ACCEPT: usize = 1;
pub const CHIP_DISMISS: usize = 2;

const CLASS_NAME: &str = "FireHintChipClass";
static REGISTER: Once = Once::new();

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// Chip colors (background, border, text, accent-button fill, accent-button text) per theme.
fn colors(dark: bool) -> (u32, u32, u32, u32, u32) {
    if dark {
        (
            rgb(45, 45, 45),
            rgb(90, 90, 90),
            rgb(235, 235, 235),
            rgb(14, 99, 156),
            rgb(255, 255, 255),
        )
    } else {
        (
            rgb(250, 250, 250),
            rgb(160, 160, 160),
            rgb(30, 30, 30),
            rgb(0, 120, 215),
            rgb(255, 255, 255),
        )
    }
}

/// Heap paint/hit state the chip wndproc reads via `GWLP_USERDATA`.
struct ChipState {
    label: Vec<u16>,
    button: Vec<u16>,
    font: HFONT,
    bg: u32,
    border: u32,
    fg: u32,
    accent: u32,
    accent_text: u32,
    owner: HWND,
    /// Hit rects in client coords (set by `show`), read by the wndproc's click handling + paint.
    btn_rect: RECT,
    close_rect: RECT,
}

/// An owned popup chip window plus its paint state.
pub struct HintChip {
    hwnd: HWND,
    state: *mut ChipState,
    dpi: u32,
    visible: bool,
}

impl HintChip {
    /// Create the (hidden) chip window owned by `owner` (the frame).
    pub fn new(owner: isize, dpi: u32, dark: bool) -> Self {
        let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = wide(CLASS_NAME);
        REGISTER.call_once(|| unsafe {
            RegisterClassW(&WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(chip_wndproc),
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

        let (bg, border, fg, accent, accent_text) = colors(dark);
        let state = Box::into_raw(Box::new(ChipState {
            label: vec![0],
            button: wide("View as flipbook"),
            font: create_ui_font(dpi),
            bg,
            border,
            fg,
            accent,
            accent_text,
            owner: owner as HWND,
            btn_rect: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            close_rect: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
        }));

        // Interactive (no WS_EX_TRANSPARENT), but NOACTIVATE so it never steals the frame's focus.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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

        HintChip {
            hwnd,
            state,
            dpi,
            visible: false,
        }
    }

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

    pub fn set_dark(&mut self, dark: bool) {
        let (bg, border, fg, accent, accent_text) = colors(dark);
        unsafe {
            let s = &mut *self.state;
            s.bg = bg;
            s.border = border;
            s.fg = fg;
            s.accent = accent;
            s.accent_text = accent_text;
        }
        if self.visible {
            unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 1) };
        }
    }

    /// Show the chip for `grid`, horizontally centered on the screen point `(center_x, top_y)`
    /// (the win shell passes the top-center of the view rect). Lays out the label / button / close
    /// hit rects and sizes the window to fit.
    pub fn show(&mut self, grid: Grid, center_x: i32, top_y: i32) {
        let s = unsafe { &mut *self.state };
        s.label = wide(&format!(
            "Looks like a {}\u{00d7}{} flipbook",
            grid.cols, grid.rows
        ));

        let sc = |v: i32| v * self.dpi as i32 / 96;
        let pad = sc(10);
        let gap = sc(10);
        let (lw, th) = unsafe { measure(self.hwnd, s.font, &decode(&s.label)) };
        let (bw, _) = unsafe { measure(self.hwnd, s.font, "View as flipbook") };
        let btn_w = bw + 2 * sc(8);
        let close_w = th + sc(6);
        let h = th + 2 * pad;

        let mut x = pad;
        // label
        let label_right = x + lw;
        x = label_right + gap;
        s.btn_rect = RECT {
            left: x,
            top: (h - (th + sc(6))) / 2,
            right: x + btn_w,
            bottom: (h - (th + sc(6))) / 2 + th + sc(6),
        };
        x += btn_w + gap;
        s.close_rect = RECT {
            left: x,
            top: (h - close_w) / 2,
            right: x + close_w,
            bottom: (h - close_w) / 2 + close_w,
        };
        let w = x + close_w + pad;

        let win_x = center_x - w / 2;
        unsafe {
            SetWindowPos(self.hwnd, HWND_TOPMOST, win_x, top_y, w, h, SWP_NOACTIVATE);
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            InvalidateRect(self.hwnd, std::ptr::null(), 1);
        }
        self.visible = true;
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            self.visible = false;
        }
    }
}

impl Drop for HintChip {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.hwnd);
            let s = Box::from_raw(self.state);
            DeleteObject(s.font);
        }
    }
}

unsafe fn measure(hwnd: HWND, font: HFONT, text: &str) -> (i32, i32) {
    let hdc = GetDC(hwnd);
    if hdc.is_null() {
        return (0, 0);
    }
    let prev = SelectObject(hdc, font);
    let w: Vec<u16> = text.encode_utf16().collect();
    let mut sz = SIZE { cx: 0, cy: 0 };
    GetTextExtentPoint32W(hdc, w.as_ptr(), w.len() as i32, &mut sz);
    SelectObject(hdc, prev);
    ReleaseDC(hwnd, hdc);
    (sz.cx, sz.cy)
}

unsafe extern "system" fn chip_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ChipState;
    if state.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let s = &*state;
    match msg {
        // Keep the click from activating the chip. `WS_EX_NOACTIVATE` alone is not enough: it stops
        // the chip being activated when *shown*, but a click on its client area still runs through
        // DefWindowProc's WM_MOUSEACTIVATE, which answers MA_ACTIVATE — so pressing "View as
        // flipbook" made this popup the active+focused window, and hiding it a moment later left
        // the focus on a hidden window, with no keyboard shortcuts reaching the frame until the
        // user clicked it again. Answering MA_NOACTIVATE ourselves leaves activation where it is
        // and still delivers WM_LBUTTONDOWN.
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            fill(ps.hdc, &rc, s.border);
            let inner = RECT {
                left: rc.left + 1,
                top: rc.top + 1,
                right: rc.right - 1,
                bottom: rc.bottom - 1,
            };
            fill(ps.hdc, &inner, s.bg);

            let prev = SelectObject(ps.hdc, s.font);
            SetBkMode(ps.hdc, TRANSPARENT as i32);

            // Label (left, vertically centered over the whole chip).
            SetTextColor(ps.hdc, s.fg);
            let mut lr = RECT {
                left: rc.left + 10,
                top: rc.top,
                right: s.btn_rect.left,
                bottom: rc.bottom,
            };
            DrawTextW(
                ps.hdc,
                s.label.as_ptr(),
                -1,
                &mut lr,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );

            // Accent button.
            fill(ps.hdc, &s.btn_rect, s.accent);
            SetTextColor(ps.hdc, s.accent_text);
            let mut br = s.btn_rect;
            DrawTextW(
                ps.hdc,
                s.button.as_ptr(),
                -1,
                &mut br,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );

            // Close ✕.
            SetTextColor(ps.hdc, s.fg);
            let mut cr = s.close_rect;
            let x = wide("\u{2715}");
            DrawTextW(
                ps.hdc,
                x.as_ptr(),
                -1,
                &mut cr,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );

            SelectObject(ps.hdc, prev);
            EndPaint(hwnd, &ps);
            0
        }
        WM_LBUTTONDOWN => {
            let px = (lparam & 0xffff) as u16 as i16 as i32;
            let py = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
            let inside = |r: &RECT| px >= r.left && px < r.right && py >= r.top && py < r.bottom;
            if inside(&s.close_rect) {
                PostMessageW(s.owner, WM_APP_FLIPBOOK_CHIP, CHIP_DISMISS, 0);
            } else if inside(&s.btn_rect) {
                PostMessageW(s.owner, WM_APP_FLIPBOOK_CHIP, CHIP_ACCEPT, 0);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn fill(hdc: HDC, rect: &RECT, color: u32) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, rect, brush);
    DeleteObject(brush);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decode a null-terminated wide buffer back to a `String` for measurement.
fn decode(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}
