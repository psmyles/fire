//! Native Win32 shell. The top-level **frame** window owns
//! the message loop and paints the GDI chrome (toolbar + status bar, see [`crate::chrome`]);
//! a **child "view" window** in the middle hosts the D3D11 [`GpuSurface`] renderer.
//! Splitting the two means the frame can repaint its chrome without touching the image and
//! the image can repaint without redrawing the chrome (`WS_CLIPCHILDREN`), and the surface's
//! viewport is exactly the image region (no chrome insets to carry).
//!
//! Cross-thread wakeups (`WM_APP_OPEN` from the pipe server, `WM_APP_DECODE_DONE` from the
//! worker pool, `WM_APP_FOLDER_SCANNED` from the folder-scan thread) are posted to the frame,
//! which owns the title/size/lifecycle. Both windows reach the shared [`App`] through their
//! `GWLP_USERDATA`; only the frame owns the box.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr;

use fire_decode::{DecodeOptions, DecodedImage};
use fire_ipc::OpenRequest;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetDC, InvalidateRect, ReleaseDC, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{GetStartupInfoW, STARTF_USESHOWWINDOW, STARTUPINFOW};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageW, GetWindowLongPtrW, GetWindowPlacement, LoadCursorW, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
    SetWindowTextW, ShowWindow,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, MSG,
    SWP_NOACTIVATE, SWP_NOZORDER, SW_FORCEMINIMIZE, SW_MAXIMIZE, SW_MINIMIZE, SW_SHOW,
    SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, SW_SHOWMINNOACTIVE, SW_SHOWNORMAL, WINDOWPLACEMENT,
    WM_APP, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_DROPFILES, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETTINGCHANGE, WM_SIZE,
    WNDCLASSW, WPF_RESTORETOMAXIMIZED, WS_CHILD, WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// `WM_MOUSELEAVE` (0x02A3) isn't surfaced by windows-sys under the enabled features; it's a
/// stable message id, so we define it directly. Paired with `TrackMouseEvent`/`TME_LEAVE` to
/// clear the toolbar hover when the cursor exits the frame.
const WM_MOUSELEAVE: u32 = 0x02A3;

use crate::chrome::{self, Action, Chrome, ViewSnapshot};
use crate::decode_pool::{DecodeJob, DecodeOutcome, DecodePool};
use crate::folder::{self, Folder};
use crate::foreground;
use crate::ipc_server;
use crate::render::gpu::GpuSurface;
use crate::render::view::Channel;
use crate::window_state::WindowState;

/// An open request forwarded by the pipe server; LPARAM is `Box<OpenRequest>`.
pub const WM_APP_OPEN: u32 = WM_APP + 1;
/// A finished decode from a worker; LPARAM is `Box<DecodeOutcome>`.
pub const WM_APP_DECODE_DONE: u32 = WM_APP + 2;
/// A finished folder scan from the scan thread; LPARAM is `Box<FolderScan>`.
pub const WM_APP_FOLDER_SCANNED: u32 = WM_APP + 3;

/// A completed sibling-image scan, delivered to the UI thread (boxed, via the message LPARAM).
/// The win shell turns it into a [`Folder`] cursor once it confirms it's still current.
struct FolderScan {
    /// The issuing window's generation at scan time; used for stale-drop (same as decodes).
    generation: u64,
    /// The image whose folder was scanned; locates the cursor's starting index.
    path: PathBuf,
    /// Sorted sibling image paths in the folder.
    entries: Vec<PathBuf>,
}

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
    /// Child view window: the D3D11 render target (middle region).
    view: isize,
    surface: GpuSurface,
    pool: DecodePool,
    chrome: Chrome,
    /// Status-bar file name (without the metadata tail).
    file_label: String,
    /// Status-bar metadata tail (format · dims · depth/channels · ICC).
    meta: String,
    /// True between an open request and its decode landing (status shows "loading…").
    loading: bool,
    /// Sibling-image cursor for ←/→ navigation + the status-bar count. `None` until the
    /// background folder scan for the current image lands (lazy: image first, count after).
    folder: Option<Folder>,
}

impl App {
    /// Handle an open request (launch / drop / forward): load the image, and kick off a fresh
    /// folder scan so ←/→ navigation and the status-bar count repopulate for the new directory.
    fn open(&mut self, req: OpenRequest) {
        // Drop the old cursor immediately; the scan below rebuilds it (and the count fills in
        // after the image shows — lazy). Without this a stale "3 / 27" would linger until then.
        self.folder = None;
        let generation = self.load(&req.path, req.flags.activate);
        self.scan_folder(req.path, generation);
    }

    /// Show the frame with a placeholder for `path`, raise it if `activate`, and enqueue the
    /// decode off-thread. The image swaps in when `WM_APP_DECODE_DONE` arrives. Returns the
    /// generation assigned to this load (used to tag the folder scan for stale-drop). Shared by
    /// `open` (which also rescans the folder) and `navigate` (which reuses the existing cursor).
    fn load(&mut self, path: &Path, activate: bool) -> u64 {
        let name = file_name(path);
        self.surface.clear_image();
        self.file_label = name.clone();
        self.meta.clear();
        self.loading = true;
        set_title(self.frame, &format!("{}: {name} (loading…)", crate::product::NAME));
        // SAFETY: frame is live for the App's lifetime.
        unsafe { ShowWindow(self.frame as HWND, SW_SHOW) };
        if activate {
            // Spend the one-shot foreground grant promptly (§4.1).
            foreground::raise(self.frame);
        }
        self.surface.invalidate();
        self.invalidate_chrome();

        let generation = self.surface.next_generation();
        let opts = DecodeOptions { max_dim: MAX_CPU_DIM, honor_icc: true };
        self.pool.submit(DecodeJob { generation, path: path.to_path_buf(), opts });
        generation
    }

    /// Move to the previous (`delta = -1`) or next (`delta = +1`) sibling image and load it,
    /// reusing the current folder cursor (no rescan). A no-op until the scan has landed or when
    /// the folder holds only the open image.
    fn navigate(&mut self, delta: isize) {
        let path = match self.folder.as_mut() {
            Some(f) if f.len() > 1 => f.advance(delta),
            _ => return,
        };
        self.load(&path, false);
    }

    /// Scan `path`'s folder for sibling images off the UI thread, posting the result back via
    /// `WM_APP_FOLDER_SCANNED`. Mirrors the decode pool's discipline: the worker never touches
    /// the window or renderer, only `PostMessage`s a boxed payload the wndproc reclaims.
    fn scan_folder(&self, path: PathBuf, generation: u64) {
        let frame = self.frame;
        let _ = std::thread::Builder::new()
            .name("fire-folder-scan".into())
            .spawn(move || {
                let entries = folder::scan(&path);
                let payload = Box::new(FolderScan { generation, path, entries });
                let lparam = Box::into_raw(payload) as isize;
                // SAFETY: the box outlives the post; the UI thread reclaims it in the wndproc.
                // If the window is gone the post fails — reclaim here so we don't leak.
                let posted = unsafe { PostMessageW(frame as HWND, WM_APP_FOLDER_SCANNED, 0, lparam) };
                if posted == 0 {
                    drop(unsafe { Box::from_raw(lparam as *mut FolderScan) });
                }
            });
    }

    /// Adopt a finished folder scan as the navigation cursor, if it's still current (stale-drop
    /// by generation, exactly like decodes). Refreshes the status bar so the count appears.
    fn folder_scanned(&mut self, scan: FolderScan) {
        if scan.generation != self.surface.generation() {
            return; // superseded by a newer open
        }
        self.folder = Folder::new(scan.entries, &scan.path);
        self.invalidate_status();
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
                // Keep the window at its current (remembered) size; never resize it to the
                // image. `set_image` fits the image to the current viewport, so every open
                // lands in fit-to-window mode regardless of the image's pixel dimensions.
                self.surface.set_image(img);
                set_title(self.frame, &format!("{}: {name}", crate::product::NAME));
                self.surface.invalidate();
                self.invalidate_chrome();
                eprintln!("fire: opened {name} ({w}x{h}, {fmt})");
            }
            Err(e) => {
                self.file_label = name.clone();
                self.meta = format!("failed: {e}");
                set_title(self.frame, &format!("{}: {name} (failed)", crate::product::NAME));
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
            // ← / → walk the folder. navigate() runs its own load + repaint, so return
            // afterwards rather than falling through to the shared invalidate below.
            0x25 => return self.navigate(-1), // Left
            0x27 => return self.navigate(1),  // Right
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
        // Right side: the folder position/count (once the scan lands) followed by the zoom and,
        // for HDR, the exposure. The count shows whenever a cursor exists, even on a failed
        // decode (you can still page past a broken file).
        let mut status_right = String::new();
        if let Some(f) = &self.folder {
            status_right.push_str(&format!("{} / {}", f.position(), f.len()));
        }
        if has_image {
            if !status_right.is_empty() {
                status_right.push_str("    ");
            }
            if is_hdr {
                status_right.push_str(&format!("EV {:+.2}    {}%", s.exposure(), zoom_pct));
            } else {
                status_right.push_str(&format!("{}%", zoom_pct));
            }
        }

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

        // The app icon embedded by build.rs (winresource id "1"); used for the frame title bar
        // and taskbar so the window shows the flame instead of the generic Win32 default. The
        // integer resource id is passed as a pseudo-pointer, the MAKEINTRESOURCE convention.
        let app_icon = LoadIconW(hinstance, 1 as *const u16);

        // Frame window class (owns chrome + message loop). WS_CLIPCHILDREN is set per-window.
        let frame_class = wide("FireFrameClass");
        RegisterClassW(&WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(frame_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: app_icon,
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: frame_class.as_ptr(),
        });

        // Child view window class (D3D11 swapchain target). No background brush: it paints fully.
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

        // Restore the remembered size now (so the GPU viewport starts at the right size); the
        // exact position + maximized state is applied via SetWindowPlacement after the App is
        // attached (the OS picks the initial position here).
        let saved = WindowState::load();
        let (init_w, init_h) = match &saved {
            Some(s) => (s.width.max(200), s.height.max(150)),
            None => (1280, 800),
        };

        let title = wide(crate::product::NAME);
        let frame = CreateWindowExW(
            0,
            frame_class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            init_w,
            init_h,
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

        // Accept files dropped from Explorer onto either window. A drop can land on the chrome
        // (frame) or the image region (the view child); WS_CLIPCHILDREN means the view owns its
        // own client rect, so registering only the frame would miss drops over the image.
        DragAcceptFiles(frame, 1);
        DragAcceptFiles(view, 1);

        let mut surface = GpuSurface::new(view as isize, hinstance as isize, vw as u32, vh as u32);
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
            folder: None,
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

        // Always show the frame (even if launched without a file). The launcher's "Run"
        // setting (the shortcut's Normal/Minimized/Maximized) wins for the show state;
        // otherwise we restore the remembered maximized state. With a saved placement we
        // apply the exact position + show state atomically via SetWindowPlacement.
        let show = effective_show_cmd(launcher_show_cmd(), saved.as_ref());
        match &saved {
            Some(s) => apply_placement(frame, s, show),
            None => {
                ShowWindow(frame, show);
            }
        }

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
        WM_DROPFILES => {
            handle_drop(app, wparam as HDROP);
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
        WM_APP_FOLDER_SCANNED => {
            let scan = Box::from_raw(lparam as *mut FolderScan);
            app.folder_scanned(*scan);
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
            // Remember where/how the window was before it goes away, to restore next launch.
            save_window_state(hwnd);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Child view proc: D3D11 present + image navigation (LMB-drag pan, wheel + RMB-drag zoom, keys).
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
            if app.surface.is_zoom_dragging() {
                app.invalidate_status(); // RMB drag changes the zoom %
            }
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
        WM_RBUTTONDOWN => {
            let x = (lparam & 0xffff) as u16 as i16 as f32;
            let y = ((lparam >> 16) & 0xffff) as u16 as i16 as f32;
            SetCapture(hwnd);
            SetFocus(hwnd);
            app.surface.on_cursor_moved((x, y)); // pin the pivot to the press point
            app.surface.begin_zoom_drag();
            0
        }
        WM_RBUTTONUP => {
            ReleaseCapture();
            app.surface.end_zoom_drag();
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
        WM_DROPFILES => {
            handle_drop(app, wparam as HDROP);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// --- Win32 helpers ----------------------------------------------------------

/// Open the first file carried by a `WM_DROPFILES` `HDROP`, then release it. Shared by the frame
/// and view procs since a drop can land on the chrome or the image region. The viewer shows one
/// image at a time, so extra dropped files are ignored.
unsafe fn handle_drop(app: &mut App, hdrop: HDROP) {
    if let Some(path) = drop_first_path(hdrop) {
        app.open(OpenRequest::new(path));
    }
    DragFinish(hdrop);
}

/// Read file index 0 out of an `HDROP` as a `PathBuf`. Returns `None` if the drop carried no
/// files or the name couldn't be retrieved. `DragQueryFileW(.., null, 0)` returns the required
/// length in UTF-16 code units (excluding the NUL); the second call fills the buffer.
unsafe fn drop_first_path(hdrop: HDROP) -> Option<PathBuf> {
    let len = DragQueryFileW(hdrop, 0, ptr::null_mut(), 0);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize + 1];
    let copied = DragQueryFileW(hdrop, 0, buf.as_mut_ptr(), buf.len() as u32);
    if copied == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buf[..copied as usize])))
}

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

// --- window placement: launcher Run setting + remembered position/size --------

/// The show command the launcher requested via this process's `STARTUPINFO` — the shortcut's
/// "Run" field (Normal / Minimized / Maximized), or what `CreateProcess` passed as `nCmdShow`.
/// `None` if the launcher didn't specify one (then we use our remembered state).
fn launcher_show_cmd() -> Option<i32> {
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    unsafe { GetStartupInfoW(&mut si) };
    if si.dwFlags & STARTF_USESHOWWINDOW != 0 {
        Some(si.wShowWindow as i32)
    } else {
        None
    }
}

/// Resolve the show state to use on launch: an explicit Maximized/Minimized from the launcher
/// always wins; otherwise restore the remembered maximized state (or show normal).
fn effective_show_cmd(launcher: Option<i32>, saved: Option<&WindowState>) -> i32 {
    let saved_max = saved.is_some_and(|s| s.maximized);
    match launcher {
        Some(c) if is_maximize(c) => SW_SHOWMAXIMIZED,
        Some(c) if is_minimize(c) => SW_SHOWMINNOACTIVE,
        _ if saved_max => SW_SHOWMAXIMIZED,
        _ => SW_SHOWNORMAL,
    }
}

fn is_maximize(cmd: i32) -> bool {
    cmd == SW_SHOWMAXIMIZED || cmd == SW_MAXIMIZE
}

fn is_minimize(cmd: i32) -> bool {
    cmd == SW_SHOWMINIMIZED || cmd == SW_SHOWMINNOACTIVE || cmd == SW_MINIMIZE || cmd == SW_FORCEMINIMIZE
}

/// Restore the saved restored-rect and apply `show` atomically via `SetWindowPlacement`. Using
/// the placement API (vs positioning at create time) round-trips the workspace coordinates we
/// saved exactly, independent of taskbar/work-area offsets, and sets the correct *restore* rect
/// even when launching maximized.
unsafe fn apply_placement(frame: HWND, s: &WindowState, show: i32) {
    let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
    wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    wp.showCmd = show as u32;
    wp.ptMinPosition = POINT { x: -1, y: -1 };
    wp.ptMaxPosition = POINT { x: -1, y: -1 };
    wp.rcNormalPosition = RECT {
        left: s.x,
        top: s.y,
        right: s.x + s.width.max(1),
        bottom: s.y + s.height.max(1),
    };
    SetWindowPlacement(frame, &wp);
}

/// Capture the frame's *restored* position/size + maximized flag and persist it (best effort).
/// Called from `WM_DESTROY`; the HWND is still valid there.
fn save_window_state(frame: HWND) {
    let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    if unsafe { GetWindowPlacement(frame, &mut wp) } == 0 {
        return;
    }
    let r = wp.rcNormalPosition;
    // `showCmd` is the *current* state; `WPF_RESTORETOMAXIMIZED` covers "maximized then
    // minimized" so we still reopen maximized. We never persist a minimized state (reopening
    // minimized would be a poor surprise) — only the normal rect plus this maximized flag.
    let maximized =
        wp.showCmd == SW_SHOWMAXIMIZED as u32 || (wp.flags & WPF_RESTORETOMAXIMIZED) != 0;
    WindowState {
        x: r.left,
        y: r.top,
        width: (r.right - r.left).max(1),
        height: (r.bottom - r.top).max(1),
        maximized,
    }
    .save();
}
