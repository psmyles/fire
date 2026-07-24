//! Native Win32 shell: **one** top-level window that owns the message loop, the D3D11
//! [`GpuSurface`], and the Dear ImGui layer that draws the whole UI ([`crate::ui`]).
//!
//! It used to be two windows — a frame that GDI-painted the chrome and a child that hosted the
//! swapchain — because GDI and a flip-model swapchain cannot paint the same surface. With the chrome
//! now drawn *by the GPU, into the same backbuffer*, that split has nothing left to buy: the child
//! is gone, `WS_CLIPCHILDREN` is gone, and the image is simply drawn into a **sub-rect** of the one
//! swapchain (the chrome occupies the rest). See [`App::image_rect`] and
//! [`GpuSurface::set_image_rect`].
//!
//! Rendering stays strictly **event-driven** — the invariant this migration was most at risk of
//! breaking. ImGui's natural mode is to redraw forever; instead a frame is drawn only when something
//! actually happened, and [`App::request_frames`] asks for the *one or two* extra frames ImGui needs
//! to settle a hover or a click. No input, no timer, no message → no frame → an idle window costs
//! ~0. Measured, not assumed (architecture.md §5.2).
//!
//! Cross-thread wakeups (`WM_APP_OPEN` from the pipe server, `WM_APP_DECODE_DONE` from the worker
//! pool, `WM_APP_FOLDER_SCANNED` from the folder-scan thread) are posted to this window, which
//! reaches the shared [`App`] through its `GWLP_USERDATA` and owns the box.
//!
//! **The wndproc is three layers, and the order is the point.** [`frame_wndproc`] is the
//! `catch_unwind` firewall — a panic must never unwind into the Win32 dispatcher. Inside it,
//! [`route_event`] decides *who owns* each message (ImGui, an armed keybind row, the modal
//! settings window, an open popup, or the viewer) before any handler runs; its sequence of gates
//! is load-bearing, each one there because the gate before it would otherwise swallow the event.
//! Only what survives routing reaches [`frame_wndproc_impl`]'s dispatch, which is a shallow match
//! onto one handler per message family (`on_paint`, `on_layout`, `on_mouse`, `on_key`, `on_timer`,
//! `on_app_message`, `on_system`). Messages nobody claims fall to `DefWindowProc` — including the
//! key-ups and `WM_CHAR`, which routing gives settle frames but nothing dispatches.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};

use fire_decode::{DecodeOptions, DecodedImage};
use fire_ipc::OpenRequest;

use crate::chrome::{Action, ViewSnapshot};
use crate::flipbook::{self, FlipbookState, Grid, PerPath};
use crate::render::gpu::{FlipbookParams, GpuSurface};
use crate::render::imgui::Imgui;
use crate::transport::{TransportEdit, TransportSnapshot};
use crate::ui::theme::Metrics;

use windows_sys::Win32::Foundation::{
    GlobalFree, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromWindow, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Threading::{GetStartupInfoW, STARTF_USESHOWWINDOW, STARTUPINFOW};
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_PATHMUSTEXIST,
    OPENFILENAMEW,
};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus,
};
use windows_sys::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, DROPFILES, HDROP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, GetWindowPlacement, KillTimer, LoadCursorW, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetTimer, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
    SetWindowTextW, ShowWindow, TranslateMessage, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, GWLP_USERDATA, GWL_STYLE, HWND_TOP, IDC_ARROW, MINMAXINFO, MSG, SIZE_MAXIMIZED,
    SIZE_RESTORED, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
    SWP_NOZORDER, SW_FORCEMINIMIZE, SW_MAXIMIZE, SW_MINIMIZE, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED,
    SW_SHOWMINNOACTIVE, SW_SHOWNORMAL, WINDOWPLACEMENT, WM_APP, WM_CHAR, WM_CLOSE, WM_DESTROY,
    WM_DPICHANGED, WM_DROPFILES, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_KEYDOWN,
    WM_KEYUP, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_SIZE,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WPF_RESTORETOMAXIMIZED, WS_OVERLAPPEDWINDOW,
};

/// Clipboard format ids (stable Win32 values) not surfaced by windows-sys under the enabled
/// features, so we define them directly — see `copy_text_to_clipboard` / `copy_file_to_clipboard`.
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;

use crate::chrome;
use crate::config::Config;
use crate::decode_pool::{DecodeJob, DecodeOutcome, DecodePool, FlipbookGuess};
use crate::folder::{self, Folder};
use crate::foreground;
use crate::ipc_server;
use crate::keybinds::{KeyAction, KeyChord, Keybinds};
use crate::render::view::Channel;
use crate::watcher::FileWatcher;
use crate::window_state::WindowState;

/// An open request forwarded by the pipe server; LPARAM is `Box<OpenRequest>`.
pub const WM_APP_OPEN: u32 = WM_APP + 1;
/// A finished decode from a worker; LPARAM is `Box<DecodeOutcome>`.
pub const WM_APP_DECODE_DONE: u32 = WM_APP + 2;
/// A finished folder scan from the scan thread; LPARAM is `Box<FolderScan>`.
pub const WM_APP_FOLDER_SCANNED: u32 = WM_APP + 3;
/// The displayed image's file changed on disk (hot-reload). WPARAM is the watcher's generation
/// at arm time (stale-drop); no LPARAM payload — the UI re-decodes its own current path.
pub const WM_APP_FILE_CHANGED: u32 = WM_APP + 4;
/// A finished flipbook auto-detection from a worker, posted *after* its `WM_APP_DECODE_DONE` so the
/// per-pixel scan never delays the image. LPARAM is `Box<FlipbookGuess>`.
pub const WM_APP_FLIPBOOK_GUESS: u32 = WM_APP + 6;
/// The settings window's "Browse…" button. Posted, not called: `GetOpenFileNameW` pumps its own
/// modal loop, and the click that asked for it was discovered *during* `WM_PAINT`. It is now the
/// **only** thing in the app that needs this treatment — the popup menus and the settings window
/// itself are ImGui, and pump nothing.
pub const WM_APP_SETTINGS_BROWSE: u32 = WM_APP + 10;
/// `ui/theme.toml` changed on disk and the new stylesheet is already live (see [`crate::hotstyle`],
/// debug builds only). No payload — the UI thread re-derives everything the stylesheet feeds:
/// metrics, both ImGui styles, the icon atlas, the clear color. Handled unconditionally so the
/// reload path is the same code a release build would run; only the *watcher* that posts it is
/// debug-gated.
pub const WM_APP_THEME_RELOADED: u32 = WM_APP + 11;

/// A completed sibling-image scan, delivered to the UI thread (boxed, via the message LPARAM).
/// The win shell turns it into a [`Folder`] cursor once it confirms it's still current.
struct FolderScan {
    /// The image whose folder was scanned. Both the cursor's starting index *and* the staleness
    /// check: see [`App::folder_scanned`] for why this, rather than a decode generation.
    path: PathBuf,
    /// Sorted sibling image paths in the folder.
    entries: Vec<PathBuf>,
}

/// Timer id for animated-image (GIF) playback on the frame window; distinct from the tooltip
/// timer. Rescheduled on each tick with the next frame's delay, since GIF frame delays vary.
const ANIM_TIMER_ID: usize = 2;

/// Timer id for flipbook playback on the frame window (distinct from the tooltip / GIF timers).
/// Paused/disabled kills it, so an idle flipbook costs nothing.
const FLIPBOOK_TIMER_ID: usize = 3;

/// Timer id for the text caret's blink, armed only while a text field is being edited (in practice,
/// only in the settings window) — see [`App::sync_caret_timer`].
const CARET_TIMER_ID: usize = 4;
/// Caret blink tick. ImGui blinks on a wall-clock schedule; this only has to be frequent enough that
/// the phase changes look continuous.
const CARET_BLINK_MS: u32 = 33;

/// The flipbook playback timer's interval: a fixed ~60 Hz, whatever the sheet's frame rate and
/// whether or not it is crossfading.
///
/// **This timer neither paces the animation nor, normally, the frames.** [`App::advance_flipbook`]
/// derives the position from elapsed time, so the sheet plays at its own `fps` no matter when we
/// sample it; and while the window is visible, each frame is asked for by the *previous* frame's
/// present, which blocks until vblank and so paces playback at exactly the display's refresh rate
/// (see [`App::render`]). What this timer does is **start** playback's pump and **carry** it when
/// the present can't: an occluded window, where DXGI stops blocking.
///
/// It cannot be the pacer, and that is the point. `SetTimer` bottoms out near the system tick —
/// ~15.6 ms — which is slower than a single refresh on a 120 Hz panel. Anything slower than the
/// refresh leaves the pump unfed: the two frames [`App::redraw`] asks for land back to back, one
/// refresh apart, and then nothing until the next tick, so the position gets sampled at uneven
/// intervals. Uneven sampling of smooth motion is exactly what the eye reads as jitter in the
/// transport bar — and it is why moving the mouse made playback look *better* rather than worse:
/// mouse input feeds the pump at 125–1000 Hz, comfortably above any refresh rate.
///
/// Pinning the interval also removes a foot-gun. It used to be `1000/fps`, which at low frame rates
/// grew *longer* than [`MAX_FLIPBOOK_STEP`] — so a normal tick was clamped like a stall and playback
/// crawled. With the tick fixed that can't arise, and the cap means only what it says.
const FLIPBOOK_TICK_MS: u32 = 16;

/// Cap on the playback dt applied per tick, so a stall (modal loop, sleep) doesn't jump the
/// animation far ahead when ticks resume — it just loses time, like the GIF path.
const MAX_FLIPBOOK_STEP: f32 = 0.25;

/// Project a per-path [`FlipbookState`] to the surface's render parameters.
fn surface_flipbook(s: FlipbookState) -> FlipbookParams {
    FlipbookParams {
        grid: s.grid,
        frame_count: s.frame_count,
        frame_pos: s.frame_pos,
        blend: s.blend,
    }
}

/// Max decoded dimension on either axis — a CPU/RAM guard (no GPU texture limit now). An
/// RGBA8 bitmap at 16384² is ~1 GiB; float HDR is 4×. Larger images are CPU-downscaled.
const MAX_CPU_DIM: u32 = 16384;

/// Modifier virtual-keys, read live per keypress to build the pressed [`KeyChord`].
const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12; // Alt

/// Whether a modifier key is currently held (`GetKeyState`'s high bit).
fn key_down(vk: i32) -> bool {
    (unsafe { GetKeyState(vk) } as u16 & 0x8000) != 0
}

/// When an animation frame displayed *now*, with the given delay (ms), falls due for replacement.
/// The same clamp the timer gets, so the deadline and the timer agree.
fn anim_deadline(delay_ms: u32) -> Instant {
    Instant::now() + Duration::from_millis(delay_ms.max(1) as u64)
}

/// UI-thread state, stashed in both windows' `GWLP_USERDATA` (the frame owns the box).
struct App {
    /// The one window: message loop, swapchain, chrome, title, size, lifecycle.
    frame: isize,
    surface: GpuSurface,
    /// The Dear ImGui context + backends. Draws the chrome into the surface's backbuffer.
    imgui: Imgui,
    /// DPI-scaled chrome metrics (how tall the toolbar/status/transport are), which is what decides
    /// the image's sub-rect. Rebuilt on DPI change.
    metrics: Metrics,
    /// Current theme (which of the stylesheet's two palettes). Re-read on `WM_SETTINGCHANGE`.
    dark: bool,
    dpi: u32,
    /// Event-driven render pump: frames still owed. ImGui needs a frame or two after an input to
    /// settle hover/active state, so input asks for a couple; at zero we stop drawing and the
    /// window costs nothing. See [`App::request_frames`].
    frames_wanted: u8,
    pool: DecodePool,
    /// Status-bar file name (without the metadata tail).
    file_label: String,
    /// Status-bar metadata tail (format · dims · depth/channels · ICC).
    meta: String,
    /// True between an open request and its decode landing (status shows "loading…").
    loading: bool,
    /// Sibling-image cursor for ←/→ navigation + the status-bar count. `None` until the
    /// background folder scan for the current image lands (lazy: image first, count after).
    folder: Option<Folder>,
    /// Full path of the image currently loaded (or loading) — the hot-reload target. `None`
    /// before the first open.
    current_path: Option<PathBuf>,
    /// Full path of the image whose pixels are actually **on the surface**. Trails
    /// [`Self::current_path`], which flips at *request* time: between the two, a decode is in
    /// flight and the previous image is still the one being displayed.
    ///
    /// Everything the flipbook does keys off this rather than `current_path`, because the transport
    /// band, its edits, and playback are all about the sheet the user is *looking at*. Keyed off
    /// `current_path`, a slow decode (a big PSD/EXR is seconds) would show the incoming image's
    /// transport over the outgoing image, apply the user's clicks to the incoming image's state,
    /// and push the incoming image's playback position into the outgoing image's surface params —
    /// visibly scrubbing a sheet the edits were never meant for. `None` when nothing is displayed.
    shown_path: Option<PathBuf>,
    /// File-change watcher for hot-reload; `None` when disabled in config. Dropped with the
    /// `App`, which stops the watch thread.
    watcher: Option<FileWatcher>,
    /// The live user settings (`config.toml`). The authority for everything the settings dialog can
    /// change: the dialog edits a clone and hands it back via [`WM_APP_SETTINGS_APPLY`], which
    /// [`App::apply_settings`] adopts here and pushes into the renderer/watcher/pool.
    cfg: Config,
    /// The keyboard table, resolved from `cfg.keybinds` over the defaults. Drives both key dispatch
    /// ([`App::handle_key`]) and the toolbar tooltips' shortcut suffixes.
    keybinds: Keybinds,
    /// True while the frame is in borderless full-screen (chrome hidden, view covers the monitor).
    fullscreen: bool,
    /// The windowed placement captured on entering full-screen, restored on exit (and used when
    /// saving window state if we quit while still full-screen).
    windowed_placement: WINDOWPLACEMENT,
    /// Per-path flipbook state (fire's only per-path map; session-only). Keyed by image path so it
    /// survives folder navigation and hot-reload. `state` holds the user's settings, `hint`/`hint_
    /// dismissed` drive the chip. See [`crate::flipbook`].
    flipbook: HashMap<PathBuf, PerPath>,
    /// Wall-clock of the previous playback tick, for dt-based advance (robust to timer jitter).
    flipbook_last_tick: Option<Instant>,
    /// When the displayed GIF frame falls due and must be replaced by the next one. `None` for a
    /// still image. The deadline — not the timer firing — is what advances the animation; see
    /// [`App::advance_playback`].
    anim_due: Option<Instant>,
    /// The popup menu that is up, if any (actions or overflow). Like the settings window, an ImGui
    /// popup drawn inside our own frame: no `TrackPopupMenu`, no nested pump, no command-id table.
    menu: Option<crate::ui::MenuState>,
    /// The settings window, while it is open. It is an ImGui modal drawn inside our own frame, so —
    /// unlike the Win32 dialog it replaced — it runs no nested message pump and holds no borrow: its
    /// state simply lives here and is edited during the paint. `None` = closed.
    settings: Option<crate::ui::settings::State>,
    /// Whether the caret-blink timer is currently armed (see [`App::sync_caret_timer`]).
    caret_timer: bool,
    /// True between `WM_ENTERSIZEMOVE` and `WM_EXITSIZEMOVE`, i.e. while the user is dragging the
    /// frame's border or title bar. Used to *coalesce* the window-state save: we skip the flood of
    /// intermediate `WM_SIZE`s during a drag and persist once at `WM_EXITSIZEMOVE` instead. See
    /// [`save_window_state`] — persisting live (not just on close) is what lets the next
    /// `NewWindow` launch reopen at the size/maximized state the user last left a window in.
    in_size_move: bool,
}

impl App {
    /// Handle an open request (launch / drop / forward): load the image, and kick off a fresh
    /// folder scan so ←/→ navigation and the status-bar count repopulate for the new directory.
    fn open(&mut self, req: OpenRequest) {
        // Drop the old cursor immediately; the scan below rebuilds it (and the count fills in
        // after the image shows — lazy). Without this a stale "3 / 27" would linger until then.
        self.folder = None;
        self.load(&req.path, req.flags.activate);
        self.scan_folder(req.path);
    }

    /// Show the frame for `path`, raise it if `activate`, and enqueue the decode off-thread. The
    /// currently displayed image is *kept on screen* until the new one lands (swapped in by
    /// `WM_APP_DECODE_DONE`), so navigating between folder siblings doesn't flash the empty
    /// backdrop between frames — the same no-blank-flash discipline hot-reload uses. A failed
    /// decode clears it (see `decode_done`). Returns the generation assigned to this load (used to
    /// tag the folder scan for stale-drop). Shared by `open` (which also rescans the folder) and
    /// `navigate` (which reuses the existing cursor).
    fn load(&mut self, path: &Path, activate: bool) -> u64 {
        let name = file_name(path);
        self.file_label = name.clone();
        self.meta.clear();
        self.loading = true;
        set_title(
            self.frame,
            &format!("{}: {name} (loading…)", crate::product::NAME),
        );
        if activate {
            // Spend the one-shot foreground grant promptly (§4.1).
            foreground::raise(self.frame);
        }
        self.redraw();
        self.surface.invalidate();
        // No image yet → the HDR group (if it was showing) must drop out of the layout now.
        self.redraw();
        self.redraw();

        self.begin_decode(path, false)
    }

    /// Bump the generation, remember `path` as the current image, (re)arm the hot-reload watch on
    /// it, and enqueue the decode. The single chokepoint for both a fresh open and a hot-reload;
    /// `reload` rides along to `decode_done` so it knows whether to preserve the view. Returns the
    /// assigned generation.
    fn begin_decode(&mut self, path: &Path, reload: bool) -> u64 {
        let generation = self.surface.next_generation();
        self.current_path = Some(path.to_path_buf());
        if let Some(w) = &self.watcher {
            w.watch(generation, path);
        }
        let opts = DecodeOptions {
            max_dim: MAX_CPU_DIM,
            honor_icc: true,
        };
        self.pool.submit(DecodeJob {
            generation,
            path: path.to_path_buf(),
            opts,
            reload,
            detect_flipbook: self.cfg.flipbook.auto_detect,
        });
        generation
    }

    /// Hot-reload the current image after its file changed on disk. Re-decodes off-thread *without*
    /// clearing the current pixels (no blank flash) and tags the job as a reload so the new image
    /// swaps in preserving the view when its dimensions match. A stale wakeup (the user navigated
    /// away since the watch was armed) is dropped by generation, like every other cross-thread post.
    fn reload(&mut self, generation: u64) {
        if generation != self.surface.generation() {
            return; // superseded by a newer open/navigate/reload
        }
        let Some(path) = self.current_path.clone() else {
            return;
        };
        self.begin_decode(&path, true);
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
    fn scan_folder(&self, path: PathBuf) {
        let frame = self.frame;
        let spawned = std::thread::Builder::new()
            .name("fire-folder-scan".into())
            .spawn(move || {
                let entries = folder::scan(&path);
                let payload = Box::new(FolderScan { path, entries });
                let lparam = Box::into_raw(payload) as isize;
                // SAFETY: the box outlives the post; the UI thread reclaims it in the wndproc.
                // If the window is gone the post fails — reclaim here so we don't leak.
                let posted =
                    unsafe { PostMessageW(frame as HWND, WM_APP_FOLDER_SCANNED, 0, lparam) };
                if posted == 0 {
                    drop(unsafe { Box::from_raw(lparam as *mut FolderScan) });
                }
            });
        // Navigation is optional — losing it must not take the viewer down — but a thread the
        // OS refused to start is worth saying out loud, or the "n / m" count just never appears.
        if let Err(e) = spawned {
            eprintln!("fire: could not start the folder scan thread: {e}");
        }
    }

    /// Adopt a finished folder scan as the navigation cursor, if it's still the image we're on.
    /// Refreshes the status bar so the count appears.
    ///
    /// Stale-dropped by **path**, not by the decode generation the rest of the cross-thread posts
    /// use. A folder cursor describes a *directory*, not a decode: a hot-reload re-decodes the same
    /// file and bumps the generation, so a generation check would discard a scan that was still in
    /// flight for the very image we are still showing — and discard it permanently, because `open`
    /// already cleared the cursor and nothing but `open` starts a scan. ←/→ and the "n / m" count
    /// would then stay dead for the rest of the session. The path is what actually identifies the
    /// scan, and it still rejects the case the guard is for: a scan left over from a previous open
    /// of a *different* image.
    fn folder_scanned(&mut self, scan: FolderScan) {
        if self.current_path.as_deref() != Some(scan.path.as_path()) {
            return; // superseded by an open of a different image
        }
        self.folder = Folder::new(scan.entries, &scan.path);
        self.redraw();
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
                let file_size = std::fs::metadata(&outcome.path).map(|m| m.len()).ok();
                self.meta = format_meta(&img, file_size);
                // Keep the window at its current (remembered) size; never resize it to the
                // image. A fresh open fits the image to the current viewport, so every open
                // lands in fit-to-window mode regardless of the image's pixel dimensions. A
                // hot-reload at the *same* dimensions instead keeps the current view (zoom/pan/
                // channel/exposure) so a re-export of the same canvas doesn't yank the user out
                // of their zoomed-in detail; a reload that changed dimensions re-fits.
                let same_dims =
                    self.surface.current_image().map(|i| (i.width, i.height)) == Some((w, h));
                let upload = if outcome.reload && same_dims {
                    self.surface.replace_image_keep_view(img)
                } else {
                    self.surface.set_image(img)
                };
                // The image decoded fine but the GPU may still reject the upload (e.g.
                // `E_OUTOFMEMORY` on a very large texture). Treat that like a decode failure
                // rather than letting the panic abort the process.
                if let Err(e) = upload {
                    eprintln!("fire: GPU upload failed for {name}: {e}");
                    self.fail_load(&name, format!("failed: GPU upload ({e})"));
                    return;
                }
                // The surface now holds this image: from here the flipbook transport, its edits and
                // its playback are about *this* path (see `shown_path`). Set before `apply_flipbook`
                // below, which reads it.
                self.shown_path = Some(outcome.path.clone());
                set_title(self.frame, &format!("{}: {name}", crate::product::NAME));
                self.surface.invalidate();
                // A float source brings in the HDR group; an LDR one drops it — relayout either way.
                self.redraw();
                self.redraw();
                // Start playback if this is an animated GIF; stop any prior animation otherwise.
                self.sync_animation();
                // Re-apply any per-path flipbook state for the adopted image (restores it on
                // navigate-back). The auto-detection hint for a fresh open arrives *later*, via
                // `WM_APP_FLIPBOOK_GUESS` (kept off the time-to-first-pixel path), and re-applies
                // then — so a new sheet shows instantly and the chip pops a beat afterward.
                self.apply_flipbook();
                eprintln!("fire: opened {name} ({w}x{h}, {fmt})");
            }
            Err(e) => {
                eprintln!("fire: failed to open {name}: {e}");
                self.fail_load(&name, format!("failed: {e}"));
            }
        }
    }

    /// Apply a flipbook auto-detection result that arrived after its image (see
    /// [`WM_APP_FLIPBOOK_GUESS`]). Stale-dropped by generation like a decode, so a guess for an
    /// image the user has already navigated away from is ignored. On a match, `current_path` is the
    /// guess's path, so recording the hint and re-applying pops the chip for the visible image.
    fn flipbook_guess_done(&mut self, guess: FlipbookGuess) {
        if guess.generation != self.surface.generation() {
            return; // superseded by a newer open
        }
        self.flipbook.entry(guess.path).or_default().hint = guess.guess;
        self.apply_flipbook();
    }

    /// Shared failure path for a load (failed decode *or* failed GPU upload): show `meta` in the
    /// status bar, mark the title failed, and drop any stale image. We don't clear in `load` (to
    /// avoid the navigation flash), so a broken file shouldn't keep showing the previously
    /// displayed one — that's why this repaints the backdrop here.
    /// Drop whatever is on screen and go back to the empty state: no texture, no playback, no
    /// transport, and a repaint that puts up the drop / double-click hint.
    ///
    /// Shared by the two ways an image stops being displayed — a failed load and an explicit close.
    /// They differ only in what they do to the *title and labels* around this; the teardown itself
    /// is identical. `shown_path` is cleared first because `apply_flipbook` reads it to decide there
    /// is no transport to keep.
    fn reset_to_empty(&mut self) {
        self.shown_path = None;
        self.surface.clear_image();
        self.surface.invalidate();
        // No image (or a still one) → stop any GIF playback that was running.
        self.sync_animation();
        // No image → clear any flipbook surface state, stop its timer, and hide the chip.
        self.apply_flipbook();
        self.redraw();
    }

    fn fail_load(&mut self, name: &str, meta: String) {
        self.file_label = name.to_string();
        self.meta = meta;
        set_title(
            self.frame,
            &format!("{}: {name} (failed)", crate::product::NAME),
        );
        self.reset_to_empty();
    }

    /// (Re)start or stop GIF playback for the freshly adopted image. Arms the frame's animation
    /// timer to the current frame's delay when the image is animated; kills it otherwise. Called
    /// after every adopt (fresh open, navigate, hot-reload, failed load) so switching to a still
    /// image — or a decode failure — stops the previous animation. `SetTimer` with the same id
    /// replaces any pending timer, so this is safe to call repeatedly.
    fn sync_animation(&mut self) {
        match self.surface.frame_delay_ms() {
            Some(delay) => unsafe {
                self.anim_due = Some(anim_deadline(delay));
                SetTimer(self.frame as HWND, ANIM_TIMER_ID, delay.max(1), None);
            },
            None => unsafe {
                self.anim_due = None;
                KillTimer(self.frame as HWND, ANIM_TIMER_ID);
            },
        }
    }

    /// Advance the animated image one frame: upload the next frame, repaint the view, and reschedule
    /// the timer (and the deadline) for that frame's delay. If the image is no longer animated (e.g.
    /// it was cleared) the timer is stopped.
    fn tick_animation(&mut self) {
        match self.surface.advance_frame() {
            Some(delay) => {
                self.anim_due = Some(anim_deadline(delay));
                unsafe { SetTimer(self.frame as HWND, ANIM_TIMER_ID, delay.max(1), None) };
                self.surface.invalidate();
            }
            None => unsafe {
                self.anim_due = None;
                KillTimer(self.frame as HWND, ANIM_TIMER_ID);
            },
        }
    }

    /// Bring both time-driven playbacks — the GIF's frame and the flipbook's cell — up to *now*,
    /// asking for no frame of its own. Called at the top of `WM_PAINT`, so whatever caused the frame
    /// we are about to draw, it shows the image that belongs to this instant.
    ///
    /// **Why a paint may not assume the playback timers have fired.** `WM_TIMER` is the
    /// lowest-priority message there is: `GetMessage` synthesizes one only once the queue holds no
    /// posted message, no input, *and* no pending `WM_PAINT`. Moving the mouse supplies the last two
    /// at once — every `WM_MOUSEMOVE` asks ImGui for its settle frames, so the update region is never
    /// empty for as long as the cursor keeps moving — and both playback timers are starved for the
    /// whole gesture. Advancing only on the timer therefore froze playback the moment the mouse
    /// moved, and unfroze it when the mouse stopped. Deriving the position from elapsed time instead
    /// makes every frame correct no matter what asked for it, and leaves the timers doing what they
    /// always did: asking for a frame when nothing else would.
    ///
    /// This is *not* the per-frame CPU work the GPU invariant forbids: both advances are a few
    /// arithmetic ops against a deadline, and the GIF's texture upload happens exactly when its frame
    /// falls due — never once per rendered frame.
    fn advance_playback(&mut self) {
        if self.anim_due.is_some_and(|due| Instant::now() >= due) {
            self.tick_animation();
        }
        self.advance_flipbook();
    }

    // --- flipbook (sprite-sheet) mode ------------------------------------------

    /// The displayed image's per-path flipbook entry (created on demand). Keyed on
    /// [`Self::shown_path`] — see that field for why not `current_path`.
    fn flipbook_entry(&mut self) -> Option<&mut PerPath> {
        let path = self.shown_path.clone()?;
        Some(self.flipbook.entry(path).or_default())
    }

    /// A clone of the active flipbook state when the mode is enabled for the displayed image.
    fn flipbook_state(&self) -> Option<FlipbookState> {
        let e = self.flipbook.get(self.shown_path.as_ref()?)?;
        e.enabled.then(|| e.state.clone()).flatten()
    }

    /// Whether flipbook playback is actually running — i.e. whether a frame drawn now will differ
    /// from the last one, and so whether [`App::render`] should ask for another after it.
    fn flipbook_playing(&self) -> bool {
        self.flipbook_state()
            .is_some_and(|s| s.playing && s.frame_count > 1)
    }

    /// Whether the transport band is shown (flipbook active, windowed).
    fn transport_visible(&self) -> bool {
        !self.fullscreen && self.flipbook_state().is_some()
    }

    /// Mirror the active per-path state onto the surface (or clear it) and re-arm the timer.
    ///
    /// The band appearing/disappearing changes the image's sub-rect, but nothing needs to track that
    /// here any more: the rect is recomputed from scratch every frame in [`App::render`], which is
    /// the whole point of an immediate-mode UI — there is no retained layout to keep in sync, and so
    /// no "did the band's visibility change since last time?" bookkeeping to get wrong.
    fn apply_flipbook(&mut self) {
        let params = self.flipbook_state().map(surface_flipbook);
        self.surface.set_flipbook(params);
        self.sync_flipbook_timer();
        self.redraw();
    }

    /// Toggle flipbook mode for the current image (K / toolbar). Enabling seeds state from the
    /// detected grid (or an 8×8 default) and dismisses the hint chip; disabling stops playback but
    /// retains the settings for re-entry.
    fn toggle_flipbook(&mut self) {
        // Needs a still image (a GIF is already an animation, not a sprite sheet).
        if self.surface.current_image().is_none() || self.surface.frame_delay_ms().is_some() {
            return;
        }
        // Copy the defaults out before borrowing the per-path entry (both live on `self`).
        let defaults = self.cfg.flipbook;
        let Some(entry) = self.flipbook_entry() else {
            return;
        };
        if entry.enabled {
            entry.enabled = false;
            if let Some(s) = &mut entry.state {
                s.playing = false;
            }
        } else {
            entry.enabled = true;
            entry.hint_dismissed = true;
            if entry.state.is_none() {
                let grid = entry.hint.unwrap_or(Grid::new(8, 8));
                entry.state = Some(FlipbookState::new(grid, &defaults));
            }
        }
        self.apply_flipbook();
        self.redraw();
    }

    /// Arm/kill the flipbook playback timer to match the active state. Paused/off = no timer.
    fn sync_flipbook_timer(&mut self) {
        let tick = self
            .flipbook_state()
            .and_then(|s| (s.playing && s.frame_count > 1).then_some(FLIPBOOK_TICK_MS));
        match tick {
            Some(t) => {
                self.flipbook_last_tick = Some(Instant::now());
                unsafe { SetTimer(self.frame as HWND, FLIPBOOK_TIMER_ID, t.max(1), None) };
            }
            None => {
                self.flipbook_last_tick = None;
                unsafe { KillTimer(self.frame as HWND, FLIPBOOK_TIMER_ID) };
            }
        }
    }

    /// The flipbook timer fired: advance, and ask for the frame that shows it. On an idle window
    /// this is what paces playback; the advance itself is [`App::advance_flipbook`], which every
    /// paint also runs (see [`App::advance_playback`] for why it has to).
    fn tick_flipbook(&mut self) {
        if self.advance_flipbook() {
            self.redraw();
        }
    }

    /// Advance flipbook playback to *now* — dt-based, so timer jitter and starved ticks don't
    /// accumulate — and push the new position at the surface. Returns whether it advanced (i.e. the
    /// mode is on and playing); requests no frame of its own, so a paint can call it.
    fn advance_flipbook(&mut self) -> bool {
        let Some(path) = self.shown_path.clone() else {
            return false;
        };
        let Some(entry) = self.flipbook.get_mut(&path) else {
            return false;
        };
        if !entry.enabled {
            return false;
        }
        let Some(s) = &mut entry.state else {
            return false;
        };
        if !s.playing || s.frame_count <= 1 {
            return false;
        }
        let now = Instant::now();
        let dt = self
            .flipbook_last_tick
            .map(|t| (now - t).as_secs_f32().min(MAX_FLIPBOOK_STEP))
            .unwrap_or(0.0);
        s.frame_pos = (s.frame_pos + dt * s.fps).rem_euclid(s.frame_count as f32);
        let pos = s.frame_pos;
        self.flipbook_last_tick = Some(now);
        self.surface.set_flipbook_pos(pos);
        true
    }

    /// The flipbook detection hint, when the chip should be offered: the current image has an
    /// undismissed hint and flipbook mode is off. Drawn by [`crate::ui`] as a panel over the image —
    /// it used to be its own layered popup window, with all the show/hide/reposition/minimize
    /// bookkeeping that implies. Now it is a function of state, evaluated per frame.
    fn chip_hint(&self) -> Option<Grid> {
        let e = self.flipbook.get(self.shown_path.as_ref()?)?;
        if e.enabled || e.hint_dismissed {
            return None;
        }
        e.hint
    }

    /// Build the read model the transport band renders from.
    fn transport_snapshot(&self) -> Option<TransportSnapshot> {
        let s = self.flipbook_state()?;
        Some(TransportSnapshot {
            cols: s.grid.cols,
            rows: s.grid.rows,
            frame_count: s.frame_count,
            fps: s.fps,
            blend: s.blend,
            playing: s.playing,
            frame_pos: s.frame_pos,
            grid_max: flipbook::GRID_MAX,
        })
    }

    /// Apply a transport edit to the active flipbook state, then sync the surface/timer/repaint.
    fn apply_transport_edit(&mut self, edit: TransportEdit) {
        let Some(path) = self.shown_path.clone() else {
            return;
        };
        let mut grid_changed = false;
        {
            let Some(entry) = self.flipbook.get_mut(&path) else {
                return;
            };
            let Some(s) = &mut entry.state else {
                return;
            };
            match edit {
                TransportEdit::SetCols(c) => {
                    let follow = s.frame_count == s.grid.cols * s.grid.rows;
                    s.grid.cols = c;
                    if follow {
                        s.frame_count = s.grid.cols * s.grid.rows;
                    }
                    grid_changed = true;
                }
                TransportEdit::SetRows(r) => {
                    let follow = s.frame_count == s.grid.cols * s.grid.rows;
                    s.grid.rows = r;
                    if follow {
                        s.frame_count = s.grid.cols * s.grid.rows;
                    }
                    grid_changed = true;
                }
                TransportEdit::SetCount(n) => s.frame_count = n,
                TransportEdit::SetFps(f) => s.fps = f,
                TransportEdit::ToggleBlend => s.blend = !s.blend,
                TransportEdit::TogglePlay => s.playing = !s.playing,
                TransportEdit::Pause => s.playing = false,
                TransportEdit::Scrub(pos) => s.frame_pos = pos,
            }
            s.clamp();
        }
        if grid_changed {
            // A grid change refits to the new frame rect via set_flipbook.
            self.apply_flipbook();
        } else if let TransportEdit::Scrub(_) = edit {
            // Scrub is the only edit that moves nothing but the position — and it is the hot path
            // (a slider drag), so it takes the cheap route: no re-fit, no timer work, no full
            // param push, just the new position.
            if let Some(s) = self.flipbook_state() {
                self.surface.set_flipbook_pos(s.frame_pos);
            }
        } else {
            // Everything else — count, play, fps, blend — changes what playback *resolves
            // against*, not merely where it is. `frame_count` in particular is read by the shader
            // to pick the cell (and the blend seam), and by `sync_flipbook_timer` to decide
            // whether a timer should run at all: pushing only the position would leave the GPU
            // resolving against the old count (playback stalling at the old last frame, or
            // crossfading into a trimmed-off cell) and the timer armed for a state that no longer
            // exists. `set_flipbook` re-fits only when the grid changes, so the user's pan/zoom
            // survives this.
            let params = self.flipbook_state().map(surface_flipbook);
            self.surface.set_flipbook(params);
            self.sync_flipbook_timer();
        }
        self.redraw();
    }

    /// Perform a toolbar action, then repaint the image + chrome.
    fn do_action(&mut self, action: Action) {
        match action {
            // Navigation runs its own load + repaint (and relayout), so return without the shared
            // invalidate below — like the ←/→ keys.
            Action::Prev => return self.navigate(-1),
            Action::Next => return self.navigate(1),
            Action::ZoomOut => self.surface.zoom_centered(1.0 / self.cfg.zoom_step),
            Action::ZoomIn => self.surface.zoom_centered(self.cfg.zoom_step),
            Action::ZoomToggle => {
                if self.surface.is_fit() {
                    self.surface.one_to_one();
                } else {
                    self.surface.fit();
                }
            }
            Action::Channel(Channel::Rgb) => self.surface.set_channel(Channel::Rgb),
            Action::Channel(c) => self.surface.toggle_channel(c),
            Action::ToggleTonemap => self.surface.toggle_tonemap(),
            Action::ExpUp => self.surface.adjust_exposure(self.cfg.exposure_step),
            Action::ExpReset => self.surface.reset_exposure(),
            Action::ExpDown => self.surface.adjust_exposure(-self.cfg.exposure_step),
            Action::ToggleOutline => self.surface.toggle_outline(),
            Action::ToggleOctagon => self.surface.toggle_octagon(),
            Action::Background(bg) => self.surface.set_background(bg),
            // Toggling full-screen resizes the window, which fires WM_SIZE; fall through to the
            // shared redraw below.
            Action::ToggleFullscreen => self.toggle_fullscreen(),
            // Flipbook mode runs its own surface/timer sync + redraw.
            Action::ToggleFlipbook => return self.toggle_flipbook(),
            // These are reported as menu anchors, not actions; they never reach here.
            Action::OpenWithMenu | Action::Overflow => return,
        }
        self.redraw();
    }

    /// Show a popup menu, anchored at `pos` in client coords. The UI draws it on the next frame; all
    /// this does is say which menu, and where.
    ///
    /// The actions menu opens even with no image: its file entries hide themselves, but it still
    /// carries Settings, and since the toolbar's gear is gone this menu is the only way there.
    fn open_menu(&mut self, kind: crate::ui::MenuKind, pos: (f32, f32)) {
        self.menu = Some(crate::ui::MenuState::new(kind, pos));
        self.redraw();
    }

    /// Perform a command chosen from the actions menu.
    ///
    /// Every one of these is safe to run inline from the paint that discovered the click: they spawn
    /// detached processes or touch the clipboard, and none of them pumps a message loop — which is
    /// exactly what `TrackPopupMenu` did, and the reason this whole path used to be deferred.
    fn do_command(&mut self, cmd: crate::ui::Command) {
        use crate::ui::Command;
        let Some(image) = self.current_path.clone() else {
            // Settings isn't about the image, so it still works with nothing open.
            if cmd == Command::OpenSettings {
                self.open_settings();
            }
            return;
        };
        let hwnd = self.frame as HWND;
        match cmd {
            Command::ShowInExplorer => show_in_explorer(&image),
            Command::CopyFile => copy_file_to_clipboard(hwnd, &image),
            Command::CopyPath => copy_text_to_clipboard(hwnd, &image.to_string_lossy()),
            Command::CopyFileName => copy_text_to_clipboard(hwnd, &file_name(&image)),
            Command::OpenSettings => self.open_settings(),
            Command::OpenWith(path) => {
                if let Some(app) = crate::config::entry_at(&self.cfg.open_with, &path) {
                    launch_external(app, &image);
                }
            }
        }
    }

    /// Route a virtual-key press through the keybind table. Returns whether the press was consumed
    /// — the `WM_SYSKEYDOWN` path uses that to let unbound Alt chords (Alt+F4, the system menu)
    /// fall through to `DefWindowProc`.
    fn handle_key(&mut self, vk: u32) -> bool {
        // There is no "is a transport field being typed into?" preamble any more. ImGui answers that
        // with `want_capture_keyboard`, checked in the wndproc before we are ever called — so a key
        // that reaches here is, by construction, not text input. That *deletes* a class of bug (a
        // half-typed field stranded by a rebound Esc) rather than guarding against it.
        let chord = KeyChord {
            vk,
            ctrl: key_down(VK_CONTROL),
            alt: key_down(VK_MENU),
            shift: key_down(VK_SHIFT),
        };
        // Flipbook-context bindings (play/pause, step) win while the mode is active, and are inert
        // outside it — the precedence the table encodes.
        let in_flipbook = self.flipbook_state().is_some();
        let Some(action) = self.keybinds.lookup(chord, in_flipbook) else {
            return false;
        };
        self.perform_key_action(action);
        true
    }

    /// Perform a bound keyboard command.
    fn perform_key_action(&mut self, action: KeyAction) {
        match action {
            // Both file commands run their own repaint (and the picker pumps a modal loop), so they
            // return without the shared invalidate below.
            KeyAction::OpenFile => return self.open_via_dialog(),
            KeyAction::CloseImage => return self.close_image(),
            KeyAction::Fit => self.surface.fit(),
            KeyAction::ActualSize => self.surface.one_to_one(),
            KeyAction::ZoomIn => self.surface.zoom_centered(self.cfg.zoom_step),
            KeyAction::ZoomOut => self.surface.zoom_centered(1.0 / self.cfg.zoom_step),
            KeyAction::ChannelRgb => self.surface.set_channel(Channel::Rgb),
            KeyAction::ChannelR => self.surface.toggle_channel(Channel::R),
            KeyAction::ChannelG => self.surface.toggle_channel(Channel::G),
            KeyAction::ChannelB => self.surface.toggle_channel(Channel::B),
            KeyAction::ChannelA => self.surface.toggle_channel(Channel::A),
            KeyAction::ToggleTonemap => self.surface.toggle_tonemap(),
            KeyAction::ExposureUp => self.surface.adjust_exposure(self.cfg.exposure_step),
            KeyAction::ExposureDown => self.surface.adjust_exposure(-self.cfg.exposure_step),
            KeyAction::ExposureReset => self.surface.reset_exposure(),
            KeyAction::ToggleOutline => self.surface.toggle_outline(),
            // Same call the toolbar's backdrop buttons make, so the pick sticks for the session
            // exactly as clicking one does.
            KeyAction::CycleBackdrop => {
                let next = self.surface.background().next();
                self.surface.set_background(next);
            }
            // Navigation runs its own load + repaint (and relayout), so return without the shared
            // invalidate below.
            KeyAction::PrevImage => return self.navigate(-1),
            KeyAction::NextImage => return self.navigate(1),
            KeyAction::ToggleFullscreen => self.toggle_fullscreen(),
            // Esc leaves full-screen if in it; otherwise it closes the window — unless
            // `esc-closes-window` is off, which keeps the leave-full-screen half and drops the
            // destructive one (see the config field).
            KeyAction::CloseOrExitFullscreen => {
                if self.fullscreen {
                    self.set_fullscreen(false);
                } else if self.cfg.esc_closes_window {
                    unsafe { DestroyWindow(self.frame as HWND) };
                }
            }
            // Flipbook mode runs its own relayout/reposition/invalidate.
            KeyAction::ToggleFlipbook => return self.toggle_flipbook(),
            KeyAction::FlipbookPlayPause => return self.flipbook_key(TransportEdit::TogglePlay),
            KeyAction::FlipbookPrevFrame => return self.flipbook_step(-1),
            KeyAction::FlipbookNextFrame => return self.flipbook_step(1),
        }
        self.redraw();
    }

    /// Apply a playback edit from a keybind (Space) and repaint the band.
    fn flipbook_key(&mut self, edit: TransportEdit) {
        self.apply_transport_edit(edit);
    }

    /// Step the flipbook one frame (`, / .`), pausing playback and moving to the exact frame.
    fn flipbook_step(&mut self, delta: i32) {
        let Some(s) = self.flipbook_state() else {
            return;
        };
        let count = s.frame_count.max(1) as f32;
        let pos = (s.frame_pos.floor() + delta as f32).rem_euclid(count);
        // Pause, then move; two edits (TogglePlay only if currently playing).
        self.pause_flipbook();
        self.apply_transport_edit(TransportEdit::Scrub(pos));
    }

    /// Stop playback if it's running. Taking hold of the playhead — the slider (click, drag, or
    /// wheel) or the `,` / `.` step keys — is a deliberate hand-off from playback to the user, so
    /// the flipbook stays parked on the frame they landed on rather than running away from it.
    /// No-op when already paused or not in flipbook mode.
    fn pause_flipbook(&mut self) {
        if self.flipbook_state().is_some_and(|s| s.playing) {
            self.apply_transport_edit(TransportEdit::TogglePlay);
        }
    }

    /// Open the settings window: seed it from the live config and let the next paint draw it.
    ///
    /// This used to be a nested `GetMessageW` pump over a second HWND, which is why it had to be
    /// reached by `PostMessage` and could never be handed an `&mut App`. As an ImGui modal it is just
    /// state, so it can be switched on from anywhere — including from inside a click handler.
    /// Re-opening while already open keeps the window that's up (and its unsaved edits).
    fn open_settings(&mut self) {
        if self.settings.is_none() {
            self.settings = Some(crate::ui::settings::State::new(&self.cfg));
        }
        // Not `redraw()`: ImGui *fades* a modal's scrim in (`DimBgRatio += dt × 6` — 0.17 s of
        // **drawn** time), and it advances only on the frames we actually draw. Two frames leave it
        // at a tenth of its opacity and frozen there, until some unrelated input happens to pump
        // another frame — which doesn't read as "the fade is stuck", it reads as "the dim is too
        // weak". So ask for the fade's worth of frames. 0.17 s is ~11 frames at 60 Hz but ~24 here
        // (an empty frame costs well under a vsync), hence the headroom; it still *terminates*, so
        // the window is back to costing nothing the moment the scrim is up.
        self.request_frames(32);
    }

    /// Whether a keybind row is armed and waiting for a key press.
    fn settings_capturing(&self) -> bool {
        self.settings.as_ref().is_some_and(|s| s.capturing())
    }

    /// Feed a raw virtual key to the armed keybind row. The modifier state is read live here, since
    /// the settings module is pure UI and never touches Win32.
    fn settings_capture(&mut self, vk: u32) {
        if let Some(s) = &mut self.settings {
            s.capture_key(
                vk,
                key_down(VK_CONTROL),
                key_down(VK_MENU),
                key_down(VK_SHIFT),
            );
        }
    }

    /// The settings window's two shell-level keys: **Esc** cancels (discarding the draft), **Enter**
    /// commits and closes, exactly as the Win32 dialog did. Everything else the window needs, ImGui
    /// already routed.
    fn settings_key(&mut self, vk: u32) {
        const VK_RETURN: u32 = 0x0D;
        const VK_ESCAPE: u32 = 0x1B;
        match vk {
            VK_ESCAPE => {
                self.settings = None;
                self.redraw();
            }
            VK_RETURN => {
                if let Some(cfg) = self.settings.as_mut().map(|s| s.commit()) {
                    self.apply_settings(cfg);
                }
                self.settings = None;
                self.redraw();
            }
            _ => {}
        }
    }

    /// The settings window's "Browse…" — the common file dialog, filtered to executables. Deferred
    /// out of `WM_PAINT` (it pumps its own modal loop) via [`WM_APP_SETTINGS_BROWSE`], exactly like
    /// the popup menus.
    fn settings_browse(&mut self) {
        let Some(path) = browse_for_program(self.frame as HWND) else {
            return;
        };
        if let Some(s) = &mut self.settings {
            s.set_program(&path.to_string_lossy());
        }
        self.redraw();
    }

    /// Arm or disarm the caret-blink timer.
    ///
    /// The one thing in fire that needs a repaint with no input behind it: a text caret has to blink
    /// on its own. It is armed *only* while a field is being edited (i.e. essentially only in the
    /// settings window) and killed the moment focus leaves — otherwise it would be exactly the
    /// free-running timer the event-driven-render invariant forbids.
    fn sync_caret_timer(&mut self) {
        let want = self.imgui.wants_text_input();
        if want == self.caret_timer {
            return;
        }
        self.caret_timer = want;
        unsafe {
            if want {
                SetTimer(self.frame as HWND, CARET_TIMER_ID, CARET_BLINK_MS, None);
            } else {
                KillTimer(self.frame as HWND, CARET_TIMER_ID);
            }
        }
    }

    /// Adopt the settings the dialog committed (OK / Apply): push each field wherever it lives, then
    /// persist. Applied *before* saving, so an unwritable `config.toml` still costs the user
    /// persistence rather than the edit.
    ///
    /// Three tiers, by how far a change can reach without being obnoxious:
    ///   * **Live** — watcher, zoom/exposure steps, backdrop, keybinds, menu contents.
    ///   * **Next image** — the fit/tonemap an image *opens* with, and the flipbook playback
    ///     defaults: re-fitting or re-tonemapping the picture under the user's cursor would undo
    ///     whatever they'd just set up by hand.
    ///   * **Next launch** — `instance-mode` (the pipe server and mutex are decided at startup).
    fn apply_settings(&mut self, new: Config) {
        // Hot-reload: start or stop the watch thread, re-arming it on the open image.
        if new.hot_reload != self.cfg.hot_reload {
            if new.hot_reload {
                let w = FileWatcher::spawn(self.frame);
                if let Some(p) = &self.current_path {
                    w.watch(self.surface.generation(), p);
                }
                self.watcher = Some(w);
            } else {
                self.watcher = None; // dropping it stops the thread
            }
        }
        self.keybinds = Keybinds::from_config(&new.keybinds);
        apply_view_config(&mut self.surface, &new);
        self.cfg = new;
        // Opting into octagon persistence captures the overlay options as they are *right now* —
        // the settings draft only carries the checkbox; the live overlay is the authority.
        if self.cfg.octagon.remember {
            let s = self.surface.octagon();
            self.cfg.octagon.color = s.color;
            self.cfg.octagon.line_opacity = s.line_opacity;
            self.cfg.octagon.crop = s.crop;
            self.cfg.octagon.hide = s.hide;
        }
        self.cfg.save();

        // The toolbar's tooltips carry the (possibly rebound) shortcuts, and the backdrop buttons
        // reflect the new default; the open-with menu is rebuilt per-show, so it needs nothing.
        self.redraw();
    }

    /// Persist the octagon overlay's options into `config.toml` on exit, when the user opted in
    /// (Settings ▸ Overlay). The on/off toggle is never persisted — a launch always starts with
    /// the overlay off.
    fn persist_octagon(&mut self) {
        if !self.cfg.octagon.remember {
            return;
        }
        let s = self.surface.octagon();
        let oc = &mut self.cfg.octagon;
        if (oc.color, oc.line_opacity, oc.crop, oc.hide)
            != (s.color, s.line_opacity, s.crop, s.hide)
        {
            oc.color = s.color;
            oc.line_opacity = s.line_opacity;
            oc.crop = s.crop;
            oc.hide = s.hide;
            self.cfg.save();
        }
    }

    /// Build the snapshot the chrome renders from.
    fn snapshot(&self) -> ViewSnapshot {
        let s = &self.surface;
        let has_image = s.current_image().is_some();
        let zoom_pct = s.zoom_percent();
        let is_hdr = s.is_hdr();

        let status_left = if self.loading {
            format!("{} — loading…", self.file_label)
        } else if self.meta.is_empty() {
            // No image and nothing to say about one: the genuine empty state (fresh launch).
            if has_image {
                self.file_label.clone()
            } else {
                "No image".to_string()
            }
        } else {
            // `meta` is the only thing that distinguishes a *failed* load from an empty window:
            // `fail_load` clears the surface and stores the decoder's reason here, so keying the
            // empty state on `has_image` alone would swallow it and report "No image" for a file
            // the user just watched fail. Show the reason whenever there is one.
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

        // The octagon overlay's read model: only while it is on and something is displayed. The
        // frame rect comes out in image-region coords; the UI draws in client coords, so the
        // image origin is added here, once.
        let octagon = (s.octagon().enabled && has_image)
            .then(|| {
                let (fx, fy, fw, fh) = s.frame_screen_rect()?;
                let (ox, oy) = s.image_origin();
                Some(chrome::OctagonSnapshot {
                    state: s.octagon(),
                    frame: (fx + ox, fy + oy, fw, fh),
                })
            })
            .flatten();

        ViewSnapshot {
            channel: s.channel(),
            fit: s.is_fit(),
            tonemap: s.tonemap(),
            is_hdr,
            has_image,
            loading: self.loading,
            has_alpha: s.has_alpha(),
            background: s.background(),
            outline: s.outline(),
            octagon,
            can_navigate: self.folder.as_ref().is_some_and(|f| f.len() > 1),
            fullscreen: self.fullscreen,
            flipbook: self.flipbook_state().is_some(),
            has_animation: self.surface.frame_delay_ms().is_some(),
            shortcuts: self.keybinds.labels(),
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

    /// Cursor position from a mouse message's `LPARAM`, translated into **image-region** coords.
    ///
    /// All the pan/zoom math in [`crate::render::view`] is relative to the image's sub-rect, not the
    /// window — which is what the old child-view window used to give us for free. With one window we
    /// subtract the origin ourselves; forgetting it would offset every drag by the toolbar's height,
    /// so it lives in one place rather than at each call site.
    fn image_cursor(&self, lparam: LPARAM) -> (f32, f32) {
        let x = (lparam & 0xffff) as u16 as i16 as f32;
        let y = ((lparam >> 16) & 0xffff) as u16 as i16 as f32;
        let (ox, oy) = self.surface.image_origin();
        (x - ox, y - oy)
    }

    /// The image's sub-rect of the client, in physical px. In full-screen the chrome is hidden and
    /// the image owns the whole client; otherwise it sits between the toolbar and the status bar,
    /// minus the transport band when flipbook mode is on.
    ///
    /// This is the *only* definition of the image region. It is recomputed each frame and pushed
    /// into the surface — nothing caches it, so there is no layout to invalidate.
    fn image_rect(&self) -> (f32, f32, f32, f32) {
        let (w, h) = self.client();
        let (w, h) = (w as f32, h as f32);
        if self.fullscreen {
            return (0.0, 0.0, w.max(0.0), h.max(0.0));
        }
        let top = self.metrics.toolbar_h;
        let band = if self.transport_visible() {
            self.metrics.transport_h
        } else {
            0.0
        };
        let ih = (h - top - self.metrics.status_h - band).max(0.0);
        (0.0, top, w.max(0.0), ih)
    }

    /// Ask for `n` more frames and dirty the window. ImGui is immediate-mode: hover, click and
    /// active states settle over a frame or two, so a single repaint after input can leave a button
    /// visibly stuck mid-hover. Two is enough, and — crucially — it *terminates*: once the count
    /// hits zero no more `WM_PAINT` is requested and the window goes back to costing nothing.
    fn request_frames(&mut self, n: u8) {
        self.frames_wanted = self.frames_wanted.max(n);
        unsafe { InvalidateRect(self.frame as HWND, ptr::null(), 0) };
    }

    /// The everyday repaint: something changed, draw it.
    fn redraw(&mut self) {
        self.request_frames(2);
    }

    /// Flip in/out of borderless full-screen (toolbar button, F11, Esc, or middle-click).
    fn toggle_fullscreen(&mut self) {
        self.set_fullscreen(!self.fullscreen);
    }

    /// Enter (`on`) or leave borderless full-screen. Entering strips the window's border/caption and
    /// grows it to cover the monitor it's on (Raymond Chen's documented technique), saving the
    /// windowed placement so exit restores the exact prior position/size + maximized state. The
    /// chrome simply isn't drawn while full-screen (see [`crate::ui::build`]), and
    /// [`Self::image_rect`] hands the whole client to the image. No-op if already in the requested
    /// state.
    fn set_fullscreen(&mut self, on: bool) {
        if on == self.fullscreen {
            return;
        }
        let hwnd = self.frame as HWND;
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            if on {
                let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                if GetWindowPlacement(hwnd, &mut wp) == 0 {
                    return;
                }
                let mut mi: MONITORINFO = std::mem::zeroed();
                mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                if GetMonitorInfoW(mon, &mut mi) == 0 {
                    return;
                }
                self.windowed_placement = wp;
                // Set the mode before resizing so the synchronous WM_SIZE sees full-screen.
                self.fullscreen = true;
                SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_OVERLAPPEDWINDOW as isize));
                let r = mi.rcMonitor;
                SetWindowPos(
                    hwnd,
                    HWND_TOP,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                );
            } else {
                self.fullscreen = false;
                SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_OVERLAPPEDWINDOW as isize);
                SetWindowPlacement(hwnd, &self.windowed_placement);
                SetWindowPos(
                    hwnd,
                    ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                );
            }
        }
    }

    /// The empty state: no image loaded and none loading. The UI draws a drop / double-click hint
    /// over the image region, and a double-click there opens the file picker. During a load we stay
    /// out of this state (the previous image, or the backdrop, keeps showing) so the hint never
    /// flashes over a file the user just opened.
    fn empty_view_active(&self) -> bool {
        self.surface.current_image().is_none() && !self.loading
    }

    /// Open the system file picker (empty-viewport double-click / on-screen hint) and load the
    /// chosen image. The common Open dialog pumps its own modal loop on the UI thread, like the
    /// actions popup menu; a cancel is a no-op.
    fn open_via_dialog(&mut self) {
        if let Some(path) = open_file_dialog(self.frame as HWND) {
            self.open(OpenRequest::new(path));
        }
    }

    /// Close the displayed image (Ctrl+W) and go back to the empty state — the drop /
    /// double-click hint — *without* closing the window. That is the split with
    /// [`KeyAction::CloseOrExitFullscreen`]: Esc closes the window, this closes the picture.
    ///
    /// Bumping the generation is what makes it stick. A decode, a folder scan or a hot-reload
    /// wakeup can still be in flight for the image being dropped, and each of those stale-drops on
    /// something we clear here (`generation` for the decode and the reload, `current_path` for the
    /// scan) — so none of them can swap the closed image back in a moment later.
    fn close_image(&mut self) {
        if self.current_path.is_none() && self.empty_view_active() {
            return; // nothing open (and nothing on its way in)
        }
        self.surface.next_generation();
        self.current_path = None;
        // The per-path entries in `self.flipbook` stay — reopening the file restores its grid,
        // exactly as navigating back to it does.
        self.folder = None;
        self.file_label.clear();
        self.meta.clear();
        self.loading = false;
        set_title(self.frame, crate::product::NAME);
        self.reset_to_empty();
    }

    /// Re-derive everything that comes out of the stylesheet, and repaint.
    ///
    /// The single path for "the app's *look* has to change": a DPI change (metrics and the icon
    /// raster move), a light/dark or accent switch (colors move), and — in a debug build — an edit to
    /// `ui/theme.toml` (any of it can move). Cheap: the icon atlas is only re-rastered if its
    /// physical size actually changed, and everything else is a few dozen struct writes.
    fn restyle(&mut self) {
        self.metrics = Metrics::new(self.dpi);
        crate::ui::theme::apply(self.imgui.style_mut(), self.dark, self.metrics.scale);
        self.imgui.refresh_icons(self.surface.device());
        self.surface
            .set_clear(crate::ui::theme::view_clear_packed(self.dark));
        self.redraw();
    }

    /// Draw one frame: clear, the image into its sub-rect, then the whole UI over it, then present.
    ///
    /// This is the entire paint path — the `WM_PAINT` handler is a call to this. Note the ordering
    /// constraint baked into [`GpuSurface::begin_frame`]: it leaves the **UNORM** render target
    /// bound, which is what ImGui must draw through (its colors are already sRGB). Getting that
    /// wrong doesn't crash; it just washes the UI out.
    fn render(&mut self) {
        // The chrome fill, so the parts of the window the image doesn't cover start from a known
        // color rather than last frame's garbage.
        let bg = crate::ui::theme::chrome_bg(self.dark);
        self.surface.set_chrome_clear(bg);

        // Recomputed every frame — the transport band appearing, a resize, and a DPI change all just
        // fall out of this. Nothing to keep in sync.
        let (ix, iy, iw, ih) = self.image_rect();
        self.surface.set_image_rect(ix, iy, iw, ih);

        if !self.surface.begin_frame() {
            return; // no render target this frame (device lost / mid-resize); skip cleanly
        }

        let snap = self.snapshot();
        let transport = self.transport_snapshot();
        let chip = self.chip_hint();
        let (cw, ch) = self.client();
        let metrics = self.metrics;
        let dark = self.dark;
        let fullscreen = self.fullscreen;
        let icon_px = self.imgui.icon_px();
        let form = self.imgui.form_style(dark);

        // The settings and menu state are *edited* by the UI, so they go in by `&mut`. Move them out
        // for the duration rather than borrow fields of `self` across `self.imgui.frame(…)`.
        let mut settings = self.settings.take();
        let mut menu = self.menu.take();
        // Borrowed, not cloned: `Config` owns the open-with tree and the keybind map, and a frame is
        // drawn on every mouse move. Rust's disjoint-field capture makes this fine — the closure
        // takes `&self.cfg` while the call takes `&mut self.imgui`.
        let cfg = &self.cfg;
        let frame = self.imgui.frame(|ui, tex| {
            crate::ui::build(
                ui,
                tex,
                crate::ui::Inputs {
                    snap: &snap,
                    transport: transport.as_ref(),
                    chip,
                    settings: settings.as_mut(),
                    menu: menu.as_mut(),
                    cfg,
                    form,
                    m: &metrics,
                    icon_px,
                    dark,
                    client: (cw as f32, ch as f32),
                    image: (ix, iy, iw, ih),
                    fullscreen,
                },
            )
        });
        self.settings = settings;
        self.menu = menu;

        // **Playback is paced by the present, not by a timer.** `present` returned only once the
        // display had taken the frame (sync interval 1 blocks until vblank), so asking for another
        // one here paces the next exactly one refresh later — 120 Hz on a 120 Hz panel, whatever the
        // sheet's fps — and `advance_flipbook` samples the position once per refresh, evenly. That
        // even sampling is the whole point: the motion the eye follows is the transport bar's, and a
        // Win32 timer cannot clock it (`SetTimer` bottoms out around 15.6 ms — slower than a single
        // refresh at 120 Hz), which is why the timer alone still jittered. See [`FLIPBOOK_TICK_MS`].
        //
        // It terminates: at most one frame is owed at a time, and it is only asked for while
        // something is actually playing — a paused flipbook or a still image is back to costing
        // nothing. If the present *didn't* wait (occluded window: DXGI returns immediately), pacing
        // on it would spin, so we don't, and the timer carries playback until the window is visible.
        let presented = self.surface.present();
        if presented && self.flipbook_playing() {
            self.request_frames(1);
        }

        self.sync_caret_timer();
        self.apply_ui(frame);
    }

    /// Apply what the UI asked for this frame.
    fn apply_ui(&mut self, frame: crate::ui::Frame) {
        for edit in frame.edits {
            self.apply_transport_edit(edit);
        }
        for action in frame.actions {
            self.do_action(action);
        }
        if frame.chip_accept {
            self.toggle_flipbook();
        }
        if frame.chip_dismiss {
            if let Some(e) = self.flipbook_entry() {
                e.hint_dismissed = true;
            }
            self.redraw();
        }
        // The popup menus. Nothing is deferred any more: an ImGui popup pumps no messages, so a
        // toolbar button can simply ask for one and a chosen command can simply run.
        if let Some(cmd) = frame.command {
            self.do_command(cmd);
        }
        if frame.menu_close {
            self.menu = None;
            self.redraw();
        }
        if let Some(anchor) = frame.menu {
            self.open_menu(anchor.kind, anchor.pos);
        }

        // The octagon overlay's options window edited something.
        if let Some(s) = frame.octagon {
            self.surface.set_octagon(s);
            self.redraw();
        }

        // The settings window. Apply *before* close, so OK (which does both) commits.
        if let Some(cfg) = frame.settings_apply {
            self.apply_settings(cfg);
        }
        if frame.settings_close {
            self.settings = None;
            self.redraw();
        }
        // "Browse…" opens the common file dialog, which — like the popup menus — pumps its own modal
        // loop and so must not be entered from inside this paint.
        if frame.settings_browse {
            unsafe { PostMessageW(self.frame as HWND, WM_APP_SETTINGS_BROWSE, 0, 0) };
        }
    }
}

/// Push the view-related settings into the renderer. The one place that maps `Config` onto
/// [`GpuSurface`], shared by startup and the settings dialog's Apply, so the two can't drift.
/// Backdrop and outline apply to the image already on screen — both are session-global toggles, so
/// there is no "next image" for them to seed; the fit/tonemap defaults seed the *next* adopt
/// (yanking the current image's zoom or tonemap out from under the user would be hostile).
fn apply_view_config(surface: &mut GpuSurface, cfg: &Config) {
    surface.set_fit_upscale(cfg.fit_upscale);
    surface.set_zoom_snapping(&cfg.zoom_snap_levels, cfg.zoom_snap);
    surface.set_open_actual_size(cfg.default_fit == crate::config::FitCfg::ActualSize);
    surface.set_default_tonemap(cfg.default_tonemap.to_render());
    surface.set_background_pref(cfg.background.override_for_render());
    surface.set_outline(cfg.default_outline);
}

/// Register the window class and create the frame, sized from the remembered placement.
///
/// Returns the window, the placement it was restored from (the caller applies the exact position
/// and show state once the `App` is attached — the OS picks the initial position here), and the
/// system's dark-mode preference, which is read once and then owned by the `App`.
///
/// `None` if the window could not be created, which is terminal.
unsafe fn create_frame(hinstance: HMODULE) -> Option<(HWND, Option<WindowState>, bool)> {
    // The app icon embedded by build.rs (winresource id "1"); used for the frame title bar
    // and taskbar so the window shows the flame instead of the generic Win32 default. The
    // integer resource id is passed as a pseudo-pointer, the MAKEINTRESOURCE convention — not
    // a real dangling pointer, so clippy's manual_dangling_ptr suggestion doesn't apply.
    #[allow(clippy::manual_dangling_ptr)]
    let app_icon = LoadIconW(hinstance, 1 as *const u16);

    // Frame window class (owns chrome + message loop). WS_CLIPCHILDREN is set per-window.
    // CS_DBLCLKS so the empty viewport (which the frame owns while the D3D view is hidden)
    // receives WM_LBUTTONDBLCLK for double-click-to-open.
    let frame_class = wide("FireFrameClass");
    RegisterClassW(&WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
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

    let dark = chrome::system_uses_dark_mode();

    // Restore the remembered size now, so the GPU viewport starts at the right size.
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
        WS_OVERLAPPEDWINDOW,
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
        eprintln!("fire: CreateWindowExW failed");
        return None;
    }
    chrome::apply_dark_titlebar(frame, dark);
    Some((frame, saved, dark))
}

/// Build the `App` for an already-created frame: the GPU surface, ImGui, the decode pool, the
/// watcher and the keybind table.
///
/// **Construction order matters here.** The surface comes first because ImGui is built from its
/// live D3D11 device and context; everything after is independent.
unsafe fn build_app(frame: HWND, hinstance: HMODULE, dark: bool, cfg: Config) -> Box<App> {
    let dpi = GetDpiForWindow(frame).max(96);
    let metrics = Metrics::new(dpi);

    // One window, so one drop target.
    DragAcceptFiles(frame, 1);

    // The swapchain covers the whole client; the image is drawn into a sub-rect of it, recomputed
    // every frame (see `App::image_rect`).
    let (fw, fh) = client_size(frame);
    let mut surface = GpuSurface::new(
        frame as isize,
        hinstance as isize,
        fw.max(1),
        fh.max(1),
        cfg.fit_upscale,
    );
    surface.set_clear(crate::ui::theme::view_clear_packed(dark));
    // The view-related config the surface owns (backdrop / open-fit / tonemap defaults). Same
    // path the settings dialog re-runs on Apply — see `App::apply_view_config`.
    apply_view_config(&mut surface, &cfg);
    // The octagon overlay's options — persisted ones if the user opted in, defaults otherwise;
    // always starts switched off.
    surface.set_octagon(cfg.octagon.initial_state());

    // ImGui needs the live D3D11 device/context, so it is built from the surface.
    let mut imgui = Imgui::new(
        frame as isize,
        surface.device(),
        surface.device_context(),
        dpi,
    );
    crate::ui::theme::apply(imgui.style_mut(), dark, metrics.scale);

    // Debug only: watch `ui/theme.toml` in the source tree, so editing the stylesheet restyles
    // this window without a rebuild. Posts WM_APP_THEME_RELOADED; compiled out of release.
    #[cfg(debug_assertions)]
    crate::hotstyle::spawn(frame as isize);

    // Workers and the pipe server post here (this window owns title/size/lifecycle).
    let pool = DecodePool::new(frame as isize);
    // Hot-reload watcher (config-gated); posts WM_APP_FILE_CHANGED, same as the pool. None when
    // disabled, so no watch thread is spawned.
    let watcher = cfg.hot_reload.then(|| FileWatcher::spawn(frame as isize));
    let keybinds = Keybinds::from_config(&cfg.keybinds);

    Box::new(App {
        frame: frame as isize,
        surface,
        imgui,
        metrics,
        dark,
        dpi,
        frames_wanted: 0,
        pool,
        file_label: String::new(),
        meta: String::new(),
        loading: false,
        folder: None,
        current_path: None,
        shown_path: None,
        watcher,
        cfg,
        keybinds,
        fullscreen: false,
        windowed_placement: std::mem::zeroed(),
        flipbook: HashMap::new(),
        flipbook_last_tick: None,
        anim_due: None,
        menu: None,
        settings: None,
        caret_timer: false,
        in_size_move: false,
    })
}

/// Create the frame + child view, wire up the decode pool, optionally serve the pipe
/// (single-instance mode), open `initial` if given, and run the message loop until the window
/// is closed (the process then exits — non-resident).
pub fn run(initial: Option<PathBuf>, serve_pipe: bool, cfg: Config) {
    unsafe {
        let hinstance = GetModuleHandleW(ptr::null());
        let Some((frame, saved, dark)) = create_frame(hinstance) else {
            return;
        };
        let mut app = build_app(frame, hinstance, dark, cfg);

        // Open the launch path immediately (decode is async; the image swaps in via
        // WM_APP_DECODE_DONE once the loop runs).
        if let Some(path) = initial {
            app.open(OpenRequest::new(path));
        }

        let app_raw = Box::into_raw(app);
        SetWindowLongPtrW(frame, GWLP_USERDATA, app_raw as isize);

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

/// Frame window proc. A panic must never unwind across this `extern "system"` boundary into the
/// Win32 dispatcher (that aborts the process), so the real handling lives in
/// [`frame_wndproc_impl`] behind a `catch_unwind` firewall — the same panic-boundary posture the
/// decode FFI uses (see [`crate::decode_pool`]). On a caught panic we log and defer to
/// `DefWindowProc`, leaving the window alive.
unsafe extern "system" fn frame_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        frame_wndproc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(lr) => lr,
        Err(_) => {
            eprintln!("fire: recovered from a panic in frame_wndproc (msg {msg:#06x})");
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

/// Who owns an incoming message, as decided by [`route_event`].
enum Routed {
    /// Handled by ImGui or a modal layer; the wndproc returns 0 without dispatching.
    Consumed,
    /// The viewer's — fall through to the per-family dispatch.
    ToApp,
}

/// Decide who owns a message before the viewer's own dispatch sees it.
///
/// ImGui sees every message first (except WM_PAINT, which is ours) so it can update its input
/// state. Then two booleans decide who owns the event — replacing the entire hand-rolled
/// hover/capture/hit-test/focus layer the GDI chrome needed:
///
///   * `want_capture_mouse`    — the pointer is over a widget (toolbar, transport, popup).
///   * `want_capture_keyboard` — a text field has focus, so keys are typing, not commands.
///
/// Note this is *not* the wnd-proc handler's return value: upstream returns true only for the
/// handful of messages it fully consumes (WM_SETCURSOR and friends), never for "that click was
/// mine". Gating on the return value instead would let a click on a toolbar button *also* pan the
/// image underneath it.
///
/// **The order below is load-bearing** — each gate exists because the one before it would
/// otherwise take the event. Every path that returns [`Routed::Consumed`] is one the old inline
/// preamble answered with a bare `0`, never `DefWindowProc`, which is why two variants suffice.
unsafe fn route_event(
    app: &mut App,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Routed {
    let key_msg = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);

    // A keybind row on the settings tab is armed: this press *is* the binding. Take it before
    // ImGui sees it — Esc has to reach the capture (where it cancels), and ImGui would read it
    // as "close the modal" instead.
    if key_msg && app.settings_capturing() {
        app.settings_capture(wparam as u32);
        app.request_frames(2);
        return Routed::Consumed;
    }

    if app.imgui.wnd_proc(hwnd as isize, msg, wparam, lparam) {
        return Routed::Consumed;
    }

    let mouse_msg = matches!(
        msg,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_LBUTTONDBLCLK
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_MBUTTONDOWN
            | WM_MOUSEWHEEL
    );
    // WM_CHAR and the key-ups matter for the settle frames even though nothing below dispatches
    // them: without WM_CHAR, typing into a text field wouldn't repaint it.
    let input_msg = key_msg || matches!(msg, WM_CHAR | WM_KEYUP | WM_SYSKEYUP);

    // Any input can change a hover or an active state, so give ImGui its settle frames. This runs
    // *before* the ownership gates below, so a message they swallow still gets its frames.
    if mouse_msg || input_msg {
        app.request_frames(2);
    }

    // A pan/zoom drag already in flight owns the mouse to the end of the gesture, even if the
    // cursor strays over the chrome — otherwise the drag would stick the moment it crossed the
    // toolbar.
    if mouse_msg && !app.surface.is_mouse_captured() && app.imgui.wants_mouse() {
        return Routed::Consumed;
    }
    // The settings window is modal: while it is up, keys belong to it, not to the viewer — a
    // stray `F` must not re-fit the image behind it.
    //
    // Esc and Enter we handle ourselves. ImGui's nav deliberately does *not* close a modal on
    // Escape, and a dialog you can't escape is a trap. But while a **text field** is being
    // edited those two keys are the field's (Esc reverts it, Enter commits it) — ImGui has
    // already seen them via `wnd_proc` above — so we stay out of the way, and a second press,
    // once the field has let go, reaches us.
    if key_msg && app.settings.is_some() {
        if !app.imgui.wants_text_input() {
            app.settings_key(wparam as u32);
        }
        return Routed::Consumed;
    }
    // A popup menu is up. It isn't modal, so — unlike the settings window — ImGui leaves
    // `want_capture_keyboard` false and every key would fall straight through to the viewer: Esc
    // would *close the window* out from under the open menu. So the menu takes the keys, and Esc
    // dismisses it (which ImGui also does itself; doing it here as well is harmless and is the
    // part that doesn't depend on a default we don't own).
    if key_msg && app.menu.is_some() {
        const VK_ESCAPE: u32 = 0x1B;
        if wparam as u32 == VK_ESCAPE {
            app.menu = None;
            app.redraw();
        }
        return Routed::Consumed;
    }
    if key_msg && app.imgui.wants_keyboard() {
        return Routed::Consumed;
    }
    Routed::ToApp
}

/// Frame window handling: chrome paint, toolbar input, lifecycle, and the cross-thread wakeups.
/// Wrapped by [`frame_wndproc`]'s panic firewall.
unsafe fn frame_wndproc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let app_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if app_ptr.is_null() {
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return 0;
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let app = &mut *app_ptr;

    // WM_PAINT is ours alone and skips routing entirely; everything else is offered to ImGui and
    // the modal layers first.
    if msg != WM_PAINT {
        if let Routed::Consumed = route_event(app, hwnd, msg, wparam, lparam) {
            return 0;
        }
    }

    match msg {
        WM_PAINT => on_paint(app, hwnd),

        WM_SIZE | WM_ENTERSIZEMOVE | WM_EXITSIZEMOVE | WM_GETMINMAXINFO => {
            on_layout(app, hwnd, msg, wparam, lparam)
        }

        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN
        | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MOUSEWHEEL => on_mouse(app, hwnd, msg, wparam, lparam),

        WM_KEYDOWN | WM_SYSKEYDOWN => on_key(app, hwnd, msg, wparam, lparam),

        WM_TIMER => on_timer(app, wparam),

        WM_DROPFILES => {
            handle_drop(app, wparam as HDROP);
            0
        }

        // Cross-thread wakeups. Listed one by one rather than as a `WM_APP..=` range: the id space
        // has retired gaps, and a range would silently adopt whatever is added next.
        WM_APP_OPEN
        | WM_APP_DECODE_DONE
        | WM_APP_FLIPBOOK_GUESS
        | WM_APP_FOLDER_SCANNED
        | WM_APP_FILE_CHANGED
        | WM_APP_SETTINGS_BROWSE
        | WM_APP_THEME_RELOADED => on_app_message(app, msg, wparam, lparam),

        WM_DPICHANGED | WM_SETTINGCHANGE => on_system(app, hwnd, msg, wparam, lparam),

        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            // Remember where/how the window was before it goes away, to restore next launch.
            save_window_state(hwnd, app);
            app.persist_octagon();
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Draw one frame and settle the repaint debt.
unsafe fn on_paint(app: &mut App, hwnd: HWND) -> LRESULT {
    // Bring playback up to *now* before drawing, so this frame shows the cell/GIF frame that
    // belongs to this instant whatever asked for it — a hover, a resize, a drag. Deliberately
    // *before* `BeginPaint`: the advance dirties the window, and this is the repaint that
    // clears it, so it costs no extra frame. See `App::advance_playback`.
    app.advance_playback();
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    BeginPaint(hwnd, &mut ps);
    // The event-driven pump: draw the frame we were asked for, and stop when the debt is
    // paid. If `frames_wanted` is still positive afterwards, dirty the window so exactly one
    // more WM_PAINT arrives — never a self-sustaining loop.
    app.frames_wanted = app.frames_wanted.saturating_sub(1);
    app.render();
    if app.frames_wanted > 0 {
        InvalidateRect(hwnd, ptr::null(), 0);
    }
    EndPaint(hwnd, &ps);
    0
}

/// Window geometry: resize, the size/move modal loop, and the minimum tracking size.
unsafe fn on_layout(
    app: &mut App,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SIZE => {
            let w = (lparam & 0xffff) as u32;
            let h = ((lparam >> 16) & 0xffff) as u32;
            // The swapchain covers the whole client; the image's sub-rect of it is recomputed in
            // `render`, so there is nothing else to lay out.
            app.surface.resize(w, h);
            app.redraw();
            // Persist the placement as it changes, not just on close, so the *next* NewWindow launch
            // reopens at the size/maximized state the user last left a window in (rather than a stale
            // one saved by some earlier window). We only act on the maximize-button/restore-button
            // transitions here; a border drag floods `SIZE_RESTORED`, so that case saves once at
            // `WM_EXITSIZEMOVE` instead (guarded by `in_size_move`). Full-screen resizes are skipped —
            // saving there would persist the borderless monitor rect (`save_window_state` uses the
            // stashed windowed placement only when quitting mid-full-screen).
            let wp = wparam as u32;
            if !app.fullscreen && !app.in_size_move && (wp == SIZE_MAXIMIZED || wp == SIZE_RESTORED)
            {
                save_window_state(hwnd, app);
            }
            0
        }
        WM_ENTERSIZEMOVE => {
            app.in_size_move = true;
            0
        }
        WM_EXITSIZEMOVE => {
            // A border drag or title-bar move just ended; persist the final rect once (the
            // per-`WM_SIZE` save was suppressed while `in_size_move`).
            app.in_size_move = false;
            if !app.fullscreen {
                save_window_state(hwnd, app);
            }
            0
        }
        WM_GETMINMAXINFO => {
            // Keep the window wide enough that the toolbar can still lay out (the right group plus
            // a collapsed "»"), and tall enough for the chrome plus a sliver of image.
            let mmi = &mut *(lparam as *mut MINMAXINFO);
            let m = Metrics::new(GetDpiForWindow(hwnd).max(96));
            let cw = (420.0 * m.scale) as i32;
            let ch = (m.toolbar_h + m.status_h + 80.0 * m.scale) as i32;
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            let dpi = GetDpiForWindow(hwnd).max(96);
            let mut r = RECT {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            };
            AdjustWindowRectExForDpi(&mut r, style, 0, 0, dpi);
            mmi.ptMinTrackSize.x = r.right - r.left;
            mmi.ptMinTrackSize.y = r.bottom - r.top;
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Image input. Only reached when [`route_event`] decided ImGui didn't want the event, so
/// everything here acts on the image itself.
unsafe fn on_mouse(app: &mut App, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let (x, y) = app.image_cursor(lparam);
            app.surface.on_cursor_moved((x, y));
            if app.surface.is_zoom_dragging() {
                app.redraw(); // the RMB drag changes the zoom %, which the status bar shows
            }
            0
        }
        WM_LBUTTONDOWN => {
            let (x, y) = app.image_cursor(lparam);
            SetCapture(hwnd);
            SetFocus(hwnd);
            // Sync the pan origin to the press point so the first move's delta is measured from
            // here, not a stale position. Matters after the context menu (or any gap where we saw
            // no WM_MOUSEMOVE): without this the first drag lurches the image toward the click.
            app.surface.on_cursor_moved((x, y));
            app.surface.begin_drag();
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            app.surface.end_drag();
            0
        }
        WM_LBUTTONDBLCLK => {
            // Double-clicking the empty viewport opens the file picker (matching the on-screen hint).
            if app.empty_view_active() {
                let x = (lparam & 0xffff) as u16 as i16 as f32;
                let y = ((lparam >> 16) & 0xffff) as u16 as i16 as f32;
                let (ix, iy, iw, ih) = app.image_rect();
                if x >= ix && x < ix + iw && y >= iy && y < iy + ih {
                    app.open_via_dialog();
                }
            }
            0
        }
        WM_RBUTTONDOWN => {
            let (x, y) = app.image_cursor(lparam);
            SetCapture(hwnd);
            SetFocus(hwnd);
            app.surface.on_cursor_moved((x, y)); // pin the pivot to the press point
            app.surface.begin_zoom_drag();
            0
        }
        WM_RBUTTONUP => {
            ReleaseCapture();
            // A right *click* (the gesture never moved past the zoom-drag slop) opens the actions
            // menu at the cursor; an actual zoom-drag just ends.
            if !app.surface.end_zoom_drag() {
                let x = (lparam & 0xffff) as u16 as i16 as f32;
                let y = ((lparam >> 16) & 0xffff) as u16 as i16 as f32;
                app.open_menu(crate::ui::MenuKind::Actions, (x, y));
            }
            0
        }
        WM_MBUTTONDOWN => {
            // A middle-click over the image toggles full-screen. Take focus so Esc/F11 land here.
            SetFocus(hwnd);
            app.toggle_fullscreen();
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam >> 16) & 0xffff) as u16 as i16 as f32 / 120.0;
            if delta != 0.0 {
                let step = app.cfg.zoom_step;
                app.surface.zoom_at_cursor(step.powf(delta));
                app.redraw();
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Keyboard commands. `WM_SYSKEYDOWN`'s fallthrough is load-bearing: only chords actually bound
/// are consumed, so Alt+F4 and the Alt-menu still reach `DefWindowProc`.
unsafe fn on_key(app: &mut App, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_KEYDOWN => {
            app.handle_key(wparam as u32);
            0
        }
        // Alt chords arrive here, not as WM_KEYDOWN.
        WM_SYSKEYDOWN => {
            if app.handle_key(wparam as u32) {
                0
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// The three Win32 timers: GIF playback, flipbook playback, and the text caret blink.
unsafe fn on_timer(app: &mut App, wparam: WPARAM) -> LRESULT {
    match wparam {
        ANIM_TIMER_ID => app.tick_animation(),
        FLIPBOOK_TIMER_ID => app.tick_flipbook(),
        // The caret is drawn by ImGui, so blinking it is just another frame.
        CARET_TIMER_ID => app.request_frames(1),
        _ => {}
    }
    0
}

/// Cross-thread wakeups posted by the decode pool, the folder scan, the watcher, the pipe server
/// and the theme hot-reloader. Each carrying a payload reclaims its `Box` here — keeping every
/// `from_raw` in one function is what makes that discipline checkable at a glance.
unsafe fn on_app_message(app: &mut App, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
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
        WM_APP_FLIPBOOK_GUESS => {
            let guess = Box::from_raw(lparam as *mut FlipbookGuess);
            app.flipbook_guess_done(*guess);
            0
        }
        WM_APP_FOLDER_SCANNED => {
            let scan = Box::from_raw(lparam as *mut FolderScan);
            app.folder_scanned(*scan);
            0
        }
        WM_APP_FILE_CHANGED => {
            app.reload(wparam as u64);
            0
        }
        WM_APP_SETTINGS_BROWSE => {
            app.settings_browse();
            0
        }
        WM_APP_THEME_RELOADED => {
            app.restyle();
            0
        }
        // Unreachable: the dispatch above lists exactly the ids handled here. Not `DefWindowProc`
        // — a WM_APP_* message carries a raw pointer we own, and handing it to the default proc
        // would leak the payload rather than report the mistake.
        _ => unreachable!("on_app_message reached with msg {msg:#06x}"),
    }
}

/// DPI and system-theme changes: both re-style the whole UI.
unsafe fn on_system(
    app: &mut App,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DPICHANGED => {
            // Adopt the OS-suggested rect, then rescale the UI for the new DPI. ImGui 1.92 re-bakes
            // glyphs lazily, so this is a style rescale plus one icon-atlas re-raster — no font
            // atlas to rebuild.
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
            app.dpi = new_dpi.max(96);
            app.imgui.set_dpi(app.dpi);
            app.restyle();
            0
        }
        // A light/dark switch arrives as WM_SETTINGCHANGE (along with much else), and it is the only
        // theme input the app still takes from the system: every color, accent included, is the
        // stylesheet's. So this re-reads the preference and re-picks the palette it selects.
        WM_SETTINGCHANGE => {
            app.dark = chrome::system_uses_dark_mode();
            app.restyle();
            chrome::apply_dark_titlebar(hwnd, app.dark);
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

/// Build the double-NUL-terminated `lpstrFilter` for [`open_file_dialog`]: an "Image files" entry
/// listing every supported extension, then an "All files" catch-all. The filter is `label\0pattern\0`
/// pairs ended by one extra NUL (the `GetOpenFileNameW` contract).
///
/// The extensions come from [`fire_decode::SUPPORTED_EXTENSIONS`] — the same list folder navigation
/// uses. This function used to carry its own, 14 formats shorter, so the dialog hid `.qoi`, `.jxl`
/// and the Netpbm family that the viewer opens perfectly well. (A file the filter misses is still
/// openable via "All files": the decoder routes by magic bytes, not by name.)
fn image_filter_wide() -> Vec<u16> {
    let patterns: String = fire_decode::SUPPORTED_EXTENSIONS
        .iter()
        .map(|e| format!("*.{e}"))
        .collect::<Vec<_>>()
        .join(";");
    let mut buf: Vec<u16> = Vec::new();
    let mut push = |s: &str| {
        buf.extend(s.encode_utf16());
        buf.push(0);
    };
    push("Image files");
    push(&patterns);
    push("All files");
    push("*.*");
    buf.push(0); // extra NUL terminating the list
    buf
}

/// Run the common Open dialog owned by the frame, with `filter` as a double-NUL-terminated
/// label/pattern list, returning the chosen path or `None` on cancel.
///
/// `GetOpenFileNameW` pumps its own modal loop on the UI thread, like the actions popup menu; no
/// COM init is needed for the classic picker.
fn run_open_dialog(owner: HWND, filter: &[u16]) -> Option<PathBuf> {
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
        return None; // cancelled or dismissed
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(OsString::from_wide(&buf[..end])))
}

/// The Open dialog filtered to supported image formats.
fn open_file_dialog(owner: HWND) -> Option<PathBuf> {
    run_open_dialog(owner, &image_filter_wide())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The common Open dialog, filtered to executables — the settings window's "Browse…", behind an
/// open-with entry's program path.
fn browse_for_program(owner: HWND) -> Option<PathBuf> {
    let mut filter: Vec<u16> = Vec::new();
    for s in ["Programs", "*.exe;*.com;*.bat;*.cmd", "All files", "*.*"] {
        filter.extend(s.encode_utf16());
        filter.push(0);
    }
    filter.push(0);
    run_open_dialog(owner, &filter)
}

/// Launch a configured external app on `image` (the "Open in…" menu action). Best-effort: the child
/// runs detached (we never `wait`), and any failure is logged, never fatal — a bad `path` must not
/// take down the viewer. Each arg is one argv element (no shell), so there's no quoting/injection. A
/// no-op for a submenu entry (no `path`), which never reaches here.
fn launch_external(app: &crate::config::MenuEntry, image: &Path) {
    let Some(path) = app.path.as_deref() else {
        return;
    };
    match std::process::Command::new(path)
        .args(app.resolved_args(image))
        .spawn()
    {
        Ok(_child) => {}
        Err(e) => eprintln!("fire: failed to launch {} ({}): {e}", app.name, path),
    }
}

/// Open Explorer with `image` selected ("Show in Explorer"). Best-effort: a failure is logged, never
/// fatal. `raw_arg` writes the canonical `/select,"<path>"` form verbatim — Explorer's switch parser
/// wants exactly that (a normal quoted arg would wrap the whole `/select,…` token and break it).
fn show_in_explorer(image: &Path) {
    use std::os::windows::process::CommandExt;
    let arg = format!("/select,\"{}\"", image.display());
    if let Err(e) = std::process::Command::new("explorer.exe")
        .raw_arg(arg)
        .spawn()
    {
        eprintln!("fire: failed to show {} in Explorer: {e}", image.display());
    }
}

/// Publish one clipboard format, `fill`ing a freshly allocated `HGLOBAL` of `bytes`.
///
/// The ownership rule is the whole reason this exists once rather than per format: the `HGLOBAL`
/// belongs to *us* until `SetClipboardData` succeeds, and to the clipboard the instant it does — so
/// every failure path before that point must free it, and none after may. Best-effort throughout: a
/// failure leaves the clipboard no worse than the `EmptyClipboard` we already issued.
///
/// `fill` receives a pointer to `bytes` writable bytes and must initialize all of them.
///
/// # Safety
/// `fill` must not write past `bytes` from the pointer it is given.
unsafe fn set_clipboard(owner: HWND, format: u32, bytes: usize, fill: impl FnOnce(*mut u8)) {
    if OpenClipboard(owner) == 0 {
        return;
    }
    EmptyClipboard();
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
    if !h.is_null() {
        let base = GlobalLock(h) as *mut u8;
        if base.is_null() {
            GlobalFree(h);
        } else {
            fill(base);
            GlobalUnlock(h);
            if SetClipboardData(format, h).is_null() {
                GlobalFree(h); // ownership didn't transfer; release it
            }
        }
    }
    CloseClipboard();
}

/// Put UTF-16 `text` on the clipboard as `CF_UNICODETEXT` (the "Copy Path" / "Copy File Name"
/// actions).
fn copy_text_to_clipboard(owner: HWND, text: &str) {
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = std::mem::size_of_val(utf16.as_slice());
    // SAFETY: the buffer is `utf16.len()` u16s long, exactly what we ask for and exactly what we
    // write.
    unsafe {
        set_clipboard(owner, CF_UNICODETEXT, bytes, |base| {
            ptr::copy_nonoverlapping(utf16.as_ptr(), base as *mut u16, utf16.len());
        });
    }
}

/// Put `image` on the clipboard as `CF_HDROP` (the "Copy File" action), so it can be pasted into
/// Explorer or another app as the file itself. Layout per the `DROPFILES` contract: the header,
/// then the wide path (with its NUL), then one extra NUL ending the (single-entry) list.
fn copy_file_to_clipboard(owner: HWND, image: &Path) {
    let path: Vec<u16> = image
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let header = std::mem::size_of::<DROPFILES>();
    // header + the path (incl. its NUL) + one extra NUL ending the double-NUL-terminated list.
    let bytes = header + (path.len() + 1) * std::mem::size_of::<u16>();
    // SAFETY: `bytes` covers the header, the path and its two NULs; the writes below stay inside it.
    unsafe {
        set_clipboard(owner, CF_HDROP, bytes, |base| {
            ptr::write_bytes(base, 0, bytes); // zero the header fields + the trailing NUL
            let df = base as *mut DROPFILES;
            (*df).pFiles = header as u32; // byte offset from the header to the path list
            (*df).fWide = 1; // paths are UTF-16
            ptr::copy_nonoverlapping(path.as_ptr(), base.add(header) as *mut u16, path.len());
        });
    }
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string()
}

/// Status-bar metadata tail: "PNG   2048×1024   8-bit RGBA   1.4 MB   ICC".
fn format_meta(img: &DecodedImage, file_size: Option<u64>) -> String {
    let ch = match img.channels {
        1 => "Gray",
        2 => "Gray+A",
        3 => "RGB",
        4 => "RGBA",
        _ => "·",
    };
    let mut s = format!(
        "{}   {}×{}   {}-bit {}",
        img.source_format, img.width, img.height, img.bit_depth, ch
    );
    if let Some(bytes) = file_size {
        s.push_str(&format!("   {}", human_size(bytes)));
    }
    if img.icc.is_some() {
        s.push_str("   ICC");
    }
    if let Some((ow, oh)) = img.downscaled_from {
        s.push_str(&format!("   (from {ow}×{oh})"));
    }
    s
}

/// Format a byte count as a compact size string (B / KB / MB / GB, binary units).
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    match bytes {
        b if b >= GB => format!("{:.1} GB", b as f64 / GB as f64),
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.0} KB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

fn set_title(hwnd: isize, title: &str) {
    let w = wide(title);
    unsafe { SetWindowTextW(hwnd as HWND, w.as_ptr()) };
}

/// Current client-area size in physical px.
fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rc: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(hwnd, &mut rc) };
    (
        (rc.right - rc.left).max(1) as u32,
        (rc.bottom - rc.top).max(1) as u32,
    )
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
    cmd == SW_SHOWMINIMIZED
        || cmd == SW_SHOWMINNOACTIVE
        || cmd == SW_MINIMIZE
        || cmd == SW_FORCEMINIMIZE
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
/// Called from `WM_DESTROY`; the HWND is still valid there. If we quit while full-screen, the live
/// placement is the borderless monitor rect — persist the saved windowed placement instead so the
/// next launch reopens at the pre-full-screen size.
fn save_window_state(frame: HWND, app: &App) {
    let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    if app.fullscreen {
        wp = app.windowed_placement;
    } else if unsafe { GetWindowPlacement(frame, &mut wp) } == 0 {
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
