//! Native Win32 shell — replaces the winit event loop. The top-level **frame** window owns
//! the message loop and paints the GDI chrome (toolbar + status bar, see [`crate::chrome`]);
//! a **child "view" window** in the middle hosts the softbuffer [`SurfaceState`] renderer.
//! Splitting the two means the frame can repaint its chrome without touching the image and
//! the image can repaint without redrawing the chrome (`WS_CLIPCHILDREN`), and the surface's
//! viewport is exactly the image region (no chrome insets to carry).
//!
//! Cross-thread wakeups (`WM_APP_OPEN` from the pipe server, `WM_APP_DECODE_DONE` from the
//! worker pool) are posted to the frame, which owns the title/size/lifecycle. Both windows
//! reach the shared [`App`] through their `GWLP_USERDATA`; only the frame owns the box.

use std::path::PathBuf;
use std::ptr;

use fire_decode::{DecodeOptions, DecodedImage};
use fire_ipc::OpenRequest;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetDC, InvalidateRect, ReleaseDC, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, GetWindowLongPtrW, LoadCursorW, PostQuitMessage, RegisterClassW,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, MSG, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOZORDER, SW_SHOW, WM_APP, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETTINGCHANGE, WM_SIZE, WNDCLASSW,
    WS_CHILD, WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// `WM_MOUSELEAVE` (0x02A3) isn't surfaced by windows-sys under the enabled features; it's a
/// stable message id, so we define it directly. Paired with `TrackMouseEvent`/`TME_LEAVE` to
/// clear the toolbar hover when the cursor exits the frame.
const WM_MOUSELEAVE: u32 = 0x02A3;

use crate::chrome::{self, Action, Chrome, ViewSnapshot};
use crate::decode_pool::{DecodeJob, DecodeOutcome, DecodePool};
use crate::foreground;
use crate::ipc_server;
use crate::render::surface::SurfaceState;
use crate::render::view::Channel;

/// An open request forwarded by the pipe server; LPARAM is `Box<OpenRequest>`.
pub const WM_APP_OPEN: u32 = WM_APP + 1;
/// A finished decode from a worker; LPARAM is `Box<DecodeOutcome>`.
pub const WM_APP_DECODE_DONE: u32 = WM_APP + 2;

/// Multiplicative zoom per wheel notch (and per keyboard zoom step).
const ZOOM_STEP: f32 = 1.15;
/// Exposure step per keypress / toolbar press, in stops.
const EXPOSURE_STEP: f32 = 0.25;
/// Max decoded dimension on either axis — a CPU/RAM guard (no GPU texture limit now). An
/// RGBA8 bitmap at 16384² is ~1 GiB; float HDR is 4×. Larger images are CPU-downscaled.
const MAX_CPU_DIM: u32 = 16384;

/// UI-thread state, stashed in both windows' `GWLP_USERDATA` (the frame owns the box).
struct App {
    /// Top-level frame window: owns the chrome, title, size, and lifecycle.
    frame: isize,
    /// Child view window: the softbuffer render target (middle region).
    view: isize,
    surface: SurfaceState,
    pool: DecodePool,
    chrome: Chrome,
    /// Status-bar file name (without the metadata tail).
    file_label: String,
    /// Status-bar metadata tail (format · dims · depth/channels · ICC).
    meta: String,
    /// True between an open request and its decode landing (status shows "loading…").
    loading: bool,
}

impl App {
    /// Handle an open request: show the frame with a placeholder, raise it, and enqueue the
    /// decode off-thread. The image swaps in when `WM_APP_DECODE_DONE` arrives.
    fn open(&mut self, req: OpenRequest) {
        let name = file_name(&req.path);
        self.surface.clear_image();
        self.file_label = name.clone();
        self.meta.clear();
        self.loading = true;
        set_title(self.frame, &format!("Fire — {name} (loading…)"));
        // SAFETY: frame is live for the App's lifetime.
        unsafe { ShowWindow(self.frame as HWND, SW_SHOW) };
        if req.flags.activate {
            // Spend the one-shot foreground grant promptly (§4.1).
            foreground::raise(self.frame);
        }
        self.surface.invalidate();
        self.invalidate_chrome();

        let generation = self.surface.next_generation();
        let opts = DecodeOptions { max_dim: MAX_CPU_DIM, honor_icc: true };
        self.pool.submit(DecodeJob { generation, path: req.path, opts });
    }

    /// Handle a finished decode. Adopt it only if it is still the latest request (stale-drop).
    fn decode_done(&mut self, outcome: DecodeOutcome) {
        if outcome.generation != self.surface.generation() {
            return; // superseded by a newer open
        }
        let name = file_name(&outcome.path);
        self.loading = false;
        match outcome.result {
            Ok(img) => {
                let (w, h, fmt) = (img.width, img.height, img.source_format);
                self.file_label = name.clone();
                self.meta = format_meta(&img);
                self.surface.set_image(img);
                // Resize the frame so the image region (client minus chrome) shows it 1:1
                // where possible; the resulting WM_SIZE repositions the view and re-fits.
                let (iw, ih) = clamp_window_size(w, h);
                let ch = self.chrome.metrics.toolbar_h + self.chrome.metrics.status_h;
                resize_client(self.frame, iw, ih as i32 + ch);
                set_title(self.frame, &format!("Fire — {name}"));
                self.surface.invalidate();
                self.invalidate_chrome();
                eprintln!("fire: opened {name} ({w}x{h}, {fmt})");
            }
            Err(e) => {
                self.file_label = name.clone();
                self.meta = format!("failed: {e}");
                set_title(self.frame, &format!("Fire — {name} (failed)"));
                self.invalidate_chrome();
                eprintln!("fire: failed to open {name}: {e}");
            }
        }
    }

    /// Perform a toolbar action, then repaint the image + chrome.
    fn do_action(&mut self, action: Action) {
        match action {
            Action::Channel(Channel::Rgb) => self.surface.set_channel(Channel::Rgb),
            Action::Channel(c) => self.surface.toggle_channel(c),
            Action::Fit => self.surface.fit(),
            Action::OneToOne => self.surface.one_to_one(),
            Action::ToggleTonemap => self.surface.toggle_tonemap(),
            Action::ExpUp => self.surface.adjust_exposure(EXPOSURE_STEP),
            Action::ExpDown => self.surface.adjust_exposure(-EXPOSURE_STEP),
        }
        self.invalidate_chrome();
    }

    /// Map a virtual-key press to a view command (layout-independent VK codes).
    fn handle_key(&mut self, vk: u32) {
        match vk {
            0x46 => self.surface.fit(),                          // F
            0x31 => self.surface.one_to_one(),                   // 1
            0x52 => self.surface.toggle_channel(Channel::R),     // R
            0x47 => self.surface.toggle_channel(Channel::G),     // G
            0x42 => self.surface.toggle_channel(Channel::B),     // B
            0x41 => self.surface.toggle_channel(Channel::A),     // A
            0x43 => self.surface.set_channel(Channel::Rgb),      // C
            0x54 => self.surface.toggle_tonemap(),               // T
            0xDD => self.surface.adjust_exposure(EXPOSURE_STEP), // ]
            0xDB => self.surface.adjust_exposure(-EXPOSURE_STEP), // [
            0xBB | 0x6B => self.surface.zoom_centered(ZOOM_STEP), // = / numpad +
            0xBD | 0x6D => self.surface.zoom_centered(1.0 / ZOOM_STEP), // - / numpad -
            0x1B => unsafe {
                DestroyWindow(self.frame as HWND); // Esc
            },
            _ => return,
        }
        self.invalidate_chrome();
    }

    /// Build the snapshot the chrome renders from.
    fn snapshot(&self) -> ViewSnapshot {
        let s = &self.surface;
        let has_image = s.current_image().is_some();
        let zoom_pct = s.zoom_percent();
        let is_hdr = s.is_hdr();

        let status_left = if !has_image && !self.loading {
            "No image".to_string()
        } else if self.loading {
            format!("{} — loading…", self.file_label)
        } else if self.meta.is_empty() {
            self.file_label.clone()
        } else {
            format!("{}   ·   {}", self.file_label, self.meta)
        };
        let status_right = if has_image {
            if is_hdr {
                format!("EV {:+.2}    {}%", s.exposure(), zoom_pct)
            } else {
                format!("{}%", zoom_pct)
            }
        } else {
            String::new()
        };

        ViewSnapshot {
            channel: s.channel(),
            fit: s.is_fit(),
            zoom_pct,
            tonemap: s.tonemap(),
            is_hdr,
            has_image,
            status_left,
            status_right,
        }
    }

    /// Frame client size in physical px.
    fn client(&self) -> (i32, i32) {
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.frame as HWND, &mut rc) };
        ((rc.right - rc.left).max(0), (rc.bottom - rc.top).max(0))
    }

    /// The rect reserved for the image view (between toolbar and status bar).
    fn view_rect(&self) -> (i32, i32, i32, i32) {
        let (w, h) = self.client();
        let top = self.chrome.metrics.toolbar_h;
        let vh = (h - top - self.chrome.metrics.status_h).max(0);
        (0, top, w.max(0), vh)
    }

    /// Reposition the child view to the current view rect (its own WM_SIZE resizes/refits the
    /// surface).
    fn reposition_view(&self) {
        let (x, y, w, h) = self.view_rect();
        unsafe {
            SetWindowPos(self.view as HWND, ptr::null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    /// Recompute the toolbar layout for the current DPI/size (needs a measuring HDC).
    fn relayout(&mut self) {
        unsafe {
            let hdc = GetDC(self.frame as HWND);
            self.chrome.relayout(hdc);
            ReleaseDC(self.frame as HWND, hdc);
        }
    }

    /// Invalidate the toolbar + status strips (the chrome) without disturbing the view child.
    fn invalidate_chrome(&self) {
        let (w, h) = self.client();
        let tb = RECT { left: 0, top: 0, right: w, bottom: self.chrome.metrics.toolbar_h };
        let sb = RECT { left: 0, top: h - self.chrome.metrics.status_h, right: w, bottom: h };
        unsafe {
            InvalidateRect(self.frame as HWND, &tb, 0);
            InvalidateRect(self.frame as HWND, &sb, 0);
        }
    }

    /// Invalidate only the status strip (e.g. on a zoom change).
    fn invalidate_status(&self) {
        let (w, h) = self.client();
        let sb = RECT { left: 0, top: h - self.chrome.metrics.status_h, right: w, bottom: h };
        unsafe { InvalidateRect(self.frame as HWND, &sb, 0) };
    }
}

/// Create the frame + child view, wire up the decode pool, optionally serve the pipe
/// (single-instance mode), open `initial` if given, and run the message loop until the window
/// is closed (the process then exits — non-resident).
pub fn run(initial: Option<PathBuf>, serve_pipe: bool) {
    unsafe {
        let hinstance = GetModuleHandleW(ptr::null());

        // Frame window class (owns chrome + message loop). WS_CLIPCHILDREN is set per-window.
        let frame_class = wide("FireFrameClass");
        RegisterClassW(&WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(frame_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: ptr::null_mut(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: frame_class.as_ptr(),
        });

        // Child view window class (softbuffer target). No background brush: it paints fully.
        let view_class = wide("FireViewClass");
        RegisterClassW(&WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(view_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: ptr::null_mut(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: view_class.as_ptr(),
        });

        let dark = chrome::system_uses_dark_mode();

        let title = wide("Fire");
        let frame = CreateWindowExW(
            0,
            frame_class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            800,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        );
        if frame.is_null() {
            eprintln!("fire: CreateWindowExW(frame) failed");
            return;
        }
        chrome::apply_dark_titlebar(frame, dark);

        let dpi = GetDpiForWindow(frame).max(96);
        let ch = Chrome::new(dpi, dark);

        // Initial view rect from the frame client size and chrome metrics.
        let (fw, fh) = client_size(frame);
        let top = ch.metrics.toolbar_h;
        let vw = (fw as i32).max(1);
        let vh = (fh as i32 - top - ch.metrics.status_h).max(1);

        let view = CreateWindowExW(
            0,
            view_class.as_ptr(),
            ptr::null(),
            WS_CHILD | WS_VISIBLE,
            0,
            top,
            vw,
            vh,
            frame,
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        );
        if view.is_null() {
            eprintln!("fire: CreateWindowExW(view) failed");
            return;
        }

        let mut surface = SurfaceState::new(view as isize, hinstance as isize, vw as u32, vh as u32);
        surface.set_clear(ch.view_clear_packed());
        // Workers and the pipe server post to the frame (it owns title/size/lifecycle).
        let pool = DecodePool::new(frame as isize);

        let mut app = Box::new(App {
            frame: frame as isize,
            view: view as isize,
            surface,
            pool,
            chrome: ch,
            file_label: String::new(),
            meta: String::new(),
            loading: false,
        });
        app.relayout();

        // Open the launch path immediately (decode is async; the image swaps in via
        // WM_APP_DECODE_DONE once the loop runs).
        if let Some(path) = initial {
            app.open(OpenRequest::new(path));
        }

        // Attach the shared App to both windows so either wndproc can reach it.
        let app_raw = Box::into_raw(app);
        SetWindowLongPtrW(frame, GWLP_USERDATA, app_raw as isize);
        SetWindowLongPtrW(view, GWLP_USERDATA, app_raw as isize);

        // Always show the frame (even if launched without a file).
        ShowWindow(frame, SW_SHOW);

        if serve_pipe {
            ipc_server::spawn(frame as isize);
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Reclaim the App box on exit (children are already destroyed; the view holds a
        // non-owning copy of this pointer that is never freed).
        if !app_raw.is_null() {
            drop(Box::from_raw(app_raw));
        }
    }
}

/// Frame window proc: chrome paint, toolbar input, lifecycle, and the cross-thread wakeups.
unsafe extern "system" fn frame_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let app_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if app_ptr.is_null() {
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return 0;
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let app = &mut *app_ptr;

    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            let (w, h) = app.client();
            let snap = app.snapshot();
            app.chrome.paint_toolbar(ps.hdc, w, &snap);
            app.chrome.paint_status(ps.hdc, w, h, &snap);
            EndPaint(hwnd, &ps);
            0
        }
        WM_SIZE => {
            app.relayout();
            app.reposition_view();
            app.invalidate_chrome();
            0
        }
        WM_LBUTTONDOWN => {
            let x = (lparam & 0xffff) as u16 as i16 as i32;
            let y = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
            let snap = app.snapshot();
            if let Some(action) = app.chrome.hit_test(x, y, &snap) {
                app.do_action(action);
            }
            0
        }
        WM_MOUSEMOVE => {
            let x = (lparam & 0xffff) as u16 as i16 as i32;
            let y = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
            let hov = if y < app.chrome.metrics.toolbar_h {
                app.chrome.hover_index(x, y)
            } else {
                None
            };
            if hov != app.chrome.hover {
                app.chrome.hover = hov;
                app.invalidate_chrome();
                if hov.is_some() {
                    // Ask for WM_MOUSELEAVE so the hover clears when the cursor exits.
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut tme);
                }
            }
            0
        }
        WM_MOUSELEAVE => {
            if app.chrome.hover.is_some() {
                app.chrome.hover = None;
                app.invalidate_chrome();
            }
            0
        }
        WM_MOUSEWHEEL => {
            // Delivered to the focused window; the surface zooms about its own tracked cursor,
            // so we ignore the (screen-space) position here.
            let delta = ((wparam >> 16) & 0xffff) as u16 as i16 as f32 / 120.0;
            if delta != 0.0 {
                app.surface.zoom_at_cursor(ZOOM_STEP.powf(delta));
                app.invalidate_status();
            }
            0
        }
        WM_KEYDOWN => {
            app.handle_key(wparam as u32);
            0
        }
        WM_APP_OPEN => {
            let req = Box::from_raw(lparam as *mut OpenRequest);
            app.open(*req);
            0
        }
        WM_APP_DECODE_DONE => {
            let outcome = Box::from_raw(lparam as *mut DecodeOutcome);
            app.decode_done(*outcome);
            0
        }
        WM_DPICHANGED => {
            // Adopt the OS-suggested rect, then rescale chrome + view for the new DPI.
            let new_dpi = (wparam & 0xffff) as u32;
            let prc = lparam as *const RECT;
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
            app.chrome.set_dpi(new_dpi);
            app.relayout();
            app.reposition_view();
            InvalidateRect(hwnd, ptr::null(), 0);
            0
        }
        WM_SETTINGCHANGE => {
            // A theme switch (and much else) arrives here; re-detect and re-skin if changed.
            let dark = chrome::system_uses_dark_mode();
            if dark != app.chrome.dark {
                app.chrome.set_dark(dark);
                app.surface.set_clear(app.chrome.view_clear_packed());
                chrome::apply_dark_titlebar(hwnd, dark);
                InvalidateRect(hwnd, ptr::null(), 0);
                app.surface.invalidate();
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Child view proc: softbuffer paint + image navigation (pan/zoom/keys).
unsafe extern "system" fn view_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let app_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if app_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let app = &mut *app_ptr;

    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            app.surface.render();
            EndPaint(hwnd, &ps);
            0
        }
        WM_SIZE => {
            let w = (lparam & 0xffff) as u32;
            let h = ((lparam >> 16) & 0xffff) as u32;
            app.surface.resize(w, h);
            0
        }
        WM_MOUSEMOVE => {
            let x = (lparam & 0xffff) as u16 as i16 as f32;
            let y = ((lparam >> 16) & 0xffff) as u16 as i16 as f32;
            app.surface.on_cursor_moved((x, y));
            0
        }
        WM_LBUTTONDOWN => {
            SetCapture(hwnd);
            SetFocus(hwnd); // take keyboard focus so nav keys reach this window
            app.surface.begin_drag();
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            app.surface.end_drag();
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam >> 16) & 0xffff) as u16 as i16 as f32 / 120.0;
            if delta != 0.0 {
                app.surface.zoom_at_cursor(ZOOM_STEP.powf(delta));
                app.invalidate_status();
            }
            0
        }
        WM_KEYDOWN => {
            app.handle_key(wparam as u32);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// --- Win32 helpers ----------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name().and_then(|s| s.to_str()).unwrap_or("image").to_string()
}

/// Status-bar metadata tail: "PNG   2048×1024   8-bit RGBA   ICC".
fn format_meta(img: &DecodedImage) -> String {
    let ch = match img.channels {
        1 => "Gray",
        2 => "Gray+A",
        3 => "RGB",
        4 => "RGBA",
        _ => "·",
    };
    let mut s = format!("{}   {}×{}   {}-bit {}", img.source_format, img.width, img.height, img.bit_depth, ch);
    if img.icc.is_some() {
        s.push_str("   ICC");
    }
    if let Some((ow, oh)) = img.downscaled_from {
        s.push_str(&format!("   (from {ow}×{oh})"));
    }
    s
}

fn set_title(hwnd: isize, title: &str) {
    let w = wide(title);
    unsafe { SetWindowTextW(hwnd as HWND, w.as_ptr()) };
}

/// Current client-area size in physical px.
fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rc: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(hwnd, &mut rc) };
    ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32)
}

/// Resize the window so its *client* area is `cw`×`ch`.
fn resize_client(hwnd: isize, cw: u32, ch: i32) {
    let mut rc = RECT { left: 0, top: 0, right: cw as i32, bottom: ch.max(1) };
    unsafe {
        AdjustWindowRect(&mut rc, WS_OVERLAPPEDWINDOW, 0);
        SetWindowPos(
            hwnd as HWND,
            ptr::null_mut(),
            0,
            0,
            rc.right - rc.left,
            rc.bottom - rc.top,
            SWP_NOMOVE | SWP_NOZORDER,
        );
    }
}

/// Keep the initial window within a reasonable on-screen size while preserving aspect.
fn clamp_window_size(w: u32, h: u32) -> (u32, u32) {
    const MAX_W: f32 = 1600.0;
    const MAX_H: f32 = 1000.0;
    let w = w.max(1) as f32;
    let h = h.max(1) as f32;
    let scale = (MAX_W / w).min(MAX_H / h).min(1.0);
    (((w * scale) as u32).max(1), ((h * scale) as u32).max(1))
}
