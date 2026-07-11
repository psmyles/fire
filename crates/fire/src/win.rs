//! Native Win32 shell. The top-level **frame** window owns
//! the message loop and paints the GDI chrome (toolbar + status bar, see [`crate::chrome`]);
//! a **child "view" window** in the middle hosts the D3D11 [`GpuSurface`] renderer.
//! Splitting the two means the frame can repaint its chrome without touching the image and
//! the image can repaint without redrawing the chrome (`WS_CLIPCHILDREN`), and the surface's
//! viewport is exactly the image region (no chrome insets to carry).
//!
//! With no image loaded the view is *hidden*, handing its region back to the frame, which paints
//! an empty-state hint there (drop a file / double-click to open); a double-click over that region
//! opens the file picker. Once an image loads (or one is decoding) the view is shown again and owns
//! the region. See [`App::sync_empty_view`].
//!
//! Cross-thread wakeups (`WM_APP_OPEN` from the pipe server, `WM_APP_DECODE_DONE` from the
//! worker pool, `WM_APP_FOLDER_SCANNED` from the folder-scan thread) are posted to the frame,
//! which owns the title/size/lifecycle. Both windows reach the shared [`App`] through their
//! `GWLP_USERDATA`; only the frame owns the box.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Instant;

use fire_decode::{DecodeOptions, DecodedImage};
use fire_ipc::OpenRequest;

use crate::flipbook::{self, FlipbookState, Grid, PerPath};
use crate::hint_chip::{HintChip, CHIP_ACCEPT, CHIP_DISMISS};
use crate::render::gpu::FlipbookParams;
use crate::transport::{Transport, TransportEdit, TransportSnapshot};

use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromWindow,
    ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
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
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, DROPFILES, HDROP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowPlacement, IsIconic,
    KillTimer, LoadCursorW, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
    SetWindowTextW, ShowWindow, TrackPopupMenu, TranslateMessage, CS_DBLCLKS, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, GWL_STYLE, HMENU, HWND_TOP, IDC_ARROW, MF_CHECKED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MINMAXINFO, MSG, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SW_FORCEMINIMIZE,
    SW_HIDE, SW_MAXIMIZE, SW_MINIMIZE, SW_SHOW, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED,
    SW_SHOWMINNOACTIVE, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_LEFTBUTTON, TPM_RETURNCMD, TPM_TOPALIGN,
    WA_INACTIVE, WINDOWPLACEMENT, WM_ACTIVATE, WM_APP, WM_CHAR, WM_CLOSE, WM_DESTROY,
    WM_DPICHANGED, WM_DROPFILES, WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOVE, WM_PAINT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_SIZE, WM_TIMER, WNDCLASSW, WPF_RESTORETOMAXIMIZED, WS_CHILD,
    WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// `WM_MOUSELEAVE` (0x02A3) isn't surfaced by windows-sys under the enabled features; it's a
/// stable message id, so we define it directly. Paired with `TrackMouseEvent`/`TME_LEAVE` to
/// clear the toolbar hover when the cursor exits the frame.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Clipboard format ids (stable Win32 values) not surfaced by windows-sys under the enabled
/// features, so we define them directly — see `copy_text_to_clipboard` / `copy_file_to_clipboard`.
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;

/// `TrackPopupMenu(TPM_RETURNCMD)` command ids for the toolbar actions popup (see
/// [`App::actions_menu`]). The fixed file actions take low ids; configured "Open in…" *leaf* apps
/// take `OPEN_WITH_ID_BASE + index`, where `index` is the leaf's position in a pre-order walk of the
/// (possibly nested) menu tree — submenus carry no id of their own. The two ranges never collide. A
/// 0 return means "dismissed", so ids start at 1.
const ID_SHOW_IN_EXPLORER: usize = 1;
const ID_COPY_FILE: usize = 2;
const ID_COPY_PATH: usize = 3;
const ID_COPY_NAME: usize = 4;
const OPEN_WITH_ID_BASE: usize = 100;
/// Command ids for the "»" overflow popup: `OVERFLOW_ID_BASE + index` into the items returned by
/// [`Chrome::overflow_menu`]. Kept clear of the actions-popup ranges above (this menu is separate,
/// but a distinct base keeps the two unambiguous).
const OVERFLOW_ID_BASE: usize = 1000;

use crate::chrome::{self, Action, Chrome, ViewSnapshot};
use crate::decode_pool::{DecodeJob, DecodeOutcome, DecodePool};
use crate::folder::{self, Folder};
use crate::foreground;
use crate::ipc_server;
use crate::render::gpu::GpuSurface;
use crate::render::view::Channel;
use crate::tooltip::Tooltip;
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
/// The flipbook hint chip was clicked. WPARAM is [`crate::hint_chip::CHIP_ACCEPT`] (enter the mode)
/// or [`crate::hint_chip::CHIP_DISMISS`] (dismiss for the session); no LPARAM.
pub const WM_APP_FLIPBOOK_CHIP: u32 = WM_APP + 5;

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

/// Timer id + delay (ms) for the hover-to-show tooltip on the toolbar. The timer is (re)armed when
/// the hovered button changes and fires once after the cursor rests; killed on any hover change,
/// leave, or click.
const TIP_TIMER_ID: usize = 1;
const TIP_DELAY_MS: u32 = 500;

/// Timer id for animated-image (GIF) playback on the frame window; distinct from the tooltip
/// timer. Rescheduled on each tick with the next frame's delay, since GIF frame delays vary.
const ANIM_TIMER_ID: usize = 2;

/// Timer id for flipbook playback on the frame window (distinct from the tooltip / GIF timers).
/// Paused/disabled kills it, so an idle flipbook costs nothing.
const FLIPBOOK_TIMER_ID: usize = 3;

/// Timer tick used while crossfading (blend on): a fixed ~60 Hz so the fractional position (and its
/// crossfade) advances smoothly. Blend off ticks at the frame rate instead (one texture step).
const FLIPBOOK_BLEND_TICK_MS: u32 = 16;

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
    /// Hover tooltip for the toolbar buttons (a separate owned popup window).
    tooltip: Tooltip,
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
    /// File-change watcher for hot-reload; `None` when disabled in config. Dropped with the
    /// `App`, which stops the watch thread.
    watcher: Option<FileWatcher>,
    /// User-configured entries for the toolbar's "Open in…" menu (from `config.toml`) — external
    /// apps and/or nested submenus. Empty ⇒ the menu button is disabled.
    open_with: Vec<crate::config::MenuEntry>,
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
    /// Set while scrubbing the slider paused a playing flipbook, so release can resume it.
    resume_after_scrub: bool,
    /// The band visibility last applied by [`App::apply_flipbook`], so it can detect an
    /// appear/disappear transition (callers mutate the map before calling it, so it can't recompute
    /// the "before" state).
    transport_shown: bool,
    /// The flipbook detection hint chip (a floating popup over the view top).
    chip: HintChip,
    /// The flipbook transport band (hand-painted; shown only in flipbook mode).
    transport: Transport,
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
        // SAFETY: frame is live for the App's lifetime.
        unsafe { ShowWindow(self.frame as HWND, SW_SHOW) };
        if activate {
            // Spend the one-shot foreground grant promptly (§4.1).
            foreground::raise(self.frame);
        }
        // Bring the D3D view up for the load (hiding the empty-state hint) before we ask it to paint.
        self.sync_empty_view();
        self.surface.invalidate();
        // No image yet → the HDR group (if it was showing) must drop out of the layout now.
        self.relayout();
        self.invalidate_chrome();

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
    fn scan_folder(&self, path: PathBuf, generation: u64) {
        let frame = self.frame;
        let _ = std::thread::Builder::new()
            .name("fire-folder-scan".into())
            .spawn(move || {
                let entries = folder::scan(&path);
                let payload = Box::new(FolderScan {
                    generation,
                    path,
                    entries,
                });
                let lparam = Box::into_raw(payload) as isize;
                // SAFETY: the box outlives the post; the UI thread reclaims it in the wndproc.
                // If the window is gone the post fails — reclaim here so we don't leak.
                let posted =
                    unsafe { PostMessageW(frame as HWND, WM_APP_FOLDER_SCANNED, 0, lparam) };
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
                set_title(self.frame, &format!("{}: {name}", crate::product::NAME));
                self.surface.invalidate();
                // A float source brings in the HDR group; an LDR one drops it — relayout either way.
                self.relayout();
                self.invalidate_chrome();
                // Start playback if this is an animated GIF; stop any prior animation otherwise.
                self.sync_animation();
                // Record this decode's flipbook hint (a reload re-detects) and re-apply any per-path
                // flipbook state for the adopted image (restores it on navigate-back; the chip
                // appears if a fresh hint landed).
                self.flipbook.entry(outcome.path.clone()).or_default().hint =
                    outcome.flipbook_guess;
                self.apply_flipbook();
                eprintln!("fire: opened {name} ({w}x{h}, {fmt})");
            }
            Err(e) => {
                eprintln!("fire: failed to open {name}: {e}");
                self.fail_load(&name, format!("failed: {e}"));
            }
        }
    }

    /// Shared failure path for a load (failed decode *or* failed GPU upload): show `meta` in the
    /// status bar, mark the title failed, and drop any stale image. We don't clear in `load` (to
    /// avoid the navigation flash), so a broken file shouldn't keep showing the previously
    /// displayed one — that's why this repaints the backdrop here.
    fn fail_load(&mut self, name: &str, meta: String) {
        self.file_label = name.to_string();
        self.meta = meta;
        set_title(
            self.frame,
            &format!("{}: {name} (failed)", crate::product::NAME),
        );
        self.surface.clear_image();
        self.surface.invalidate();
        self.relayout();
        self.invalidate_chrome();
        // Back to the empty state: hide the view and paint the drop / double-click hint.
        self.sync_empty_view();
        // No image (or a still one) → stop any GIF playback that was running.
        self.sync_animation();
        // No image → clear any flipbook surface state, stop its timer, and hide the chip.
        self.apply_flipbook();
    }

    /// (Re)start or stop GIF playback for the freshly adopted image. Arms the frame's animation
    /// timer to the current frame's delay when the image is animated; kills it otherwise. Called
    /// after every adopt (fresh open, navigate, hot-reload, failed load) so switching to a still
    /// image — or a decode failure — stops the previous animation. `SetTimer` with the same id
    /// replaces any pending timer, so this is safe to call repeatedly.
    fn sync_animation(&mut self) {
        match self.surface.frame_delay_ms() {
            Some(delay) => unsafe {
                SetTimer(self.frame as HWND, ANIM_TIMER_ID, delay.max(1), None);
            },
            None => unsafe {
                KillTimer(self.frame as HWND, ANIM_TIMER_ID);
            },
        }
    }

    /// Advance the animated image one frame (on the playback timer): upload the next frame, repaint
    /// the view, and reschedule the timer for that frame's delay. If the image is no longer animated
    /// (e.g. it was cleared) the timer is stopped.
    fn tick_animation(&mut self) {
        match self.surface.advance_frame() {
            Some(delay) => {
                unsafe { SetTimer(self.frame as HWND, ANIM_TIMER_ID, delay.max(1), None) };
                self.surface.invalidate();
            }
            None => unsafe {
                KillTimer(self.frame as HWND, ANIM_TIMER_ID);
            },
        }
    }

    // --- flipbook (sprite-sheet) mode ------------------------------------------

    /// The current image's per-path flipbook entry (created on demand).
    fn flipbook_entry(&mut self) -> Option<&mut PerPath> {
        let path = self.current_path.clone()?;
        Some(self.flipbook.entry(path).or_default())
    }

    /// A clone of the active flipbook state when the mode is enabled for the current image.
    fn flipbook_state(&self) -> Option<FlipbookState> {
        let e = self.flipbook.get(self.current_path.as_ref()?)?;
        e.enabled.then(|| e.state.clone()).flatten()
    }

    /// Whether the transport band is shown (flipbook active, windowed).
    fn transport_visible(&self) -> bool {
        !self.fullscreen && self.flipbook_state().is_some()
    }

    /// Mirror the active per-path state onto the surface (or clear it), re-arm the timer, update
    /// the hint chip, and re-lay-out if the band's visibility changed since it was last applied.
    /// Callers mutate the flipbook map *before* calling this, so the "before" state is tracked in
    /// `transport_shown` rather than recomputed (which would compare `transport_visible()` to
    /// itself and never fire).
    fn apply_flipbook(&mut self) {
        let params = self.flipbook_state().map(surface_flipbook);
        self.surface.set_flipbook(params);
        self.sync_flipbook_timer();
        self.sync_chip();
        let visible = self.transport_visible();
        if visible != self.transport_shown {
            // The band appeared/disappeared → the view rect changed: re-lay-out the band widgets
            // and shrink/grow the D3D view to make room (its WM_SIZE re-fits to the new region).
            self.transport_shown = visible;
            self.relayout();
            self.reposition_view();
            self.invalidate_chrome();
        }
        self.invalidate_transport();
    }

    /// Toggle flipbook mode for the current image (K / toolbar). Enabling seeds state from the
    /// detected grid (or an 8×8 default) and dismisses the hint chip; disabling stops playback but
    /// retains the settings for re-entry.
    fn toggle_flipbook(&mut self) {
        // Needs a still image (a GIF is already an animation, not a sprite sheet).
        if self.surface.current_image().is_none() || self.surface.frame_delay_ms().is_some() {
            return;
        }
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
                entry.state = Some(FlipbookState::new(grid));
            }
        }
        self.apply_flipbook();
        self.invalidate_chrome();
    }

    /// Arm/kill the flipbook playback timer to match the active state. Paused/off = no timer.
    fn sync_flipbook_timer(&mut self) {
        let tick = self.flipbook_state().and_then(|s| {
            (s.playing && s.frame_count > 1).then(|| {
                if s.blend {
                    FLIPBOOK_BLEND_TICK_MS
                } else {
                    (1000.0 / s.fps).clamp(8.0, 60_000.0) as u32
                }
            })
        });
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

    /// Advance flipbook playback (on the timer): dt-based so jitter/stalls don't accumulate.
    fn tick_flipbook(&mut self) {
        let Some(path) = self.current_path.clone() else {
            return;
        };
        let now = Instant::now();
        let dt = self
            .flipbook_last_tick
            .map(|t| (now - t).as_secs_f32().min(MAX_FLIPBOOK_STEP))
            .unwrap_or(0.0);
        self.flipbook_last_tick = Some(now);
        let Some(entry) = self.flipbook.get_mut(&path) else {
            return;
        };
        if !entry.enabled {
            return;
        }
        let Some(s) = &mut entry.state else {
            return;
        };
        if !s.playing || s.frame_count <= 1 {
            return;
        }
        s.frame_pos = (s.frame_pos + dt * s.fps).rem_euclid(s.frame_count as f32);
        let pos = s.frame_pos;
        self.surface.set_flipbook_pos(pos);
        self.invalidate_transport();
    }

    /// Show or hide the detection hint chip: shown when the current image has an undismissed hint,
    /// the mode is off, and we're windowed and not minimized.
    fn sync_chip(&mut self) {
        let minimized = unsafe { IsIconic(self.frame as HWND) != 0 };
        let entry = self
            .current_path
            .as_ref()
            .and_then(|p| self.flipbook.get(p));
        let show = !self.fullscreen
            && !minimized
            && entry.is_some_and(|e| e.hint.is_some() && !e.enabled && !e.hint_dismissed);
        if !show {
            self.chip.hide();
            return;
        }
        let grid = entry.and_then(|e| e.hint).unwrap();
        // Anchor at the top-center of the view rect, a small gap down.
        let (vx, vy, vw, _) = self.view_rect();
        let gap = self.chrome.metrics.dpi as i32 * 8 / 96;
        let mut pt = POINT {
            x: vx + vw / 2,
            y: vy + gap,
        };
        unsafe { ClientToScreen(self.frame as HWND, &mut pt) };
        self.chip.show(grid, pt.x, pt.y);
    }

    /// Build the read model the transport band paints from.
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

    /// The transport band rect (frame-client coords) when visible.
    fn transport_band_rect(&self) -> Option<RECT> {
        if !self.transport_visible() {
            return None;
        }
        let (w, h) = self.client();
        let th = self.chrome.metrics.transport_h;
        let top = h - self.chrome.metrics.status_h - th;
        Some(RECT {
            left: 0,
            top,
            right: w,
            bottom: top + th,
        })
    }

    /// Invalidate just the transport band strip (playback tick / scrub / hover).
    fn invalidate_transport(&self) {
        if let Some(r) = self.transport_band_rect() {
            unsafe { InvalidateRect(self.frame as HWND, &r, 0) };
        }
    }

    /// Apply a transport edit to the active flipbook state, then sync the surface/timer/repaint.
    fn apply_transport_edit(&mut self, edit: TransportEdit) {
        let Some(path) = self.current_path.clone() else {
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
                TransportEdit::Scrub(pos) => s.frame_pos = pos,
            }
            s.clamp();
        }
        if grid_changed {
            // A grid change refits to the new frame rect via set_flipbook.
            self.apply_flipbook();
        } else if matches!(
            edit,
            TransportEdit::TogglePlay | TransportEdit::SetFps(_) | TransportEdit::ToggleBlend
        ) {
            // Play/fps/blend keep the grid but change playback (or the shader blend) and the timer.
            let params = self.flipbook_state().map(surface_flipbook);
            self.surface.set_flipbook(params);
            self.sync_flipbook_timer();
        } else if let Some(s) = self.flipbook_state() {
            // Count/scrub: just move the position on the surface.
            self.surface.set_flipbook_pos(s.frame_pos);
        }
        self.invalidate_transport();
    }

    /// Perform a toolbar action, then repaint the image + chrome.
    fn do_action(&mut self, action: Action) {
        match action {
            // Navigation runs its own load + repaint (and relayout), so return without the shared
            // invalidate below — like the ←/→ keys.
            Action::Prev => return self.navigate(-1),
            Action::Next => return self.navigate(1),
            Action::ZoomOut => self.surface.zoom_centered(1.0 / ZOOM_STEP),
            Action::ZoomIn => self.surface.zoom_centered(ZOOM_STEP),
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
            Action::ExpUp => self.surface.adjust_exposure(EXPOSURE_STEP),
            Action::ExpReset => self.surface.reset_exposure(),
            Action::ExpDown => self.surface.adjust_exposure(-EXPOSURE_STEP),
            Action::ToggleOutline => self.surface.toggle_outline(),
            Action::Background(bg) => self.surface.set_background(bg),
            // Toggling full-screen resizes the frame, which fires WM_SIZE (relayout + reposition);
            // fall through to the shared chrome invalidate below.
            Action::ToggleFullscreen => self.toggle_fullscreen(),
            // Flipbook mode runs its own relayout/reposition/invalidate; return without the shared
            // chrome invalidate below.
            Action::ToggleFlipbook => return self.toggle_flipbook(),
            // The actions and overflow menus are driven directly from the click handler (they need
            // the button rect); they never reach the shared dispatch. No-op, skip the repaint below.
            Action::OpenWithMenu | Action::Overflow => return,
        }
        self.invalidate_chrome();
    }

    /// Open the toolbar's actions popup under its button: the fixed file actions (show in folder,
    /// copy file / path / name) followed by any configured "Open in…" external apps, then perform
    /// the chosen entry on the current image. A no-op if there's no image (the button is disabled
    /// then anyway). All on the UI thread — `TrackPopupMenu` runs its own modal pump but touches no
    /// worker/renderer.
    fn actions_menu(&mut self) {
        let Some(rect) = self.chrome.button_rect_for(Action::OpenWithMenu) else {
            return;
        };
        // Anchor the menu at the button's bottom-left, in screen coords.
        let mut pt = POINT {
            x: rect.left,
            y: rect.bottom,
        };
        unsafe { ClientToScreen(self.frame as HWND, &mut pt) };
        self.show_actions_menu(pt);
    }

    /// Open the "»" overflow popup under its button: the left-group controls that didn't fit the
    /// current window width, each dispatching through the normal action path when chosen. The menu
    /// items mirror the toolbar buttons' enabled/checked state. A no-op if nothing overflowed. On
    /// the UI thread, like the actions popup.
    fn overflow_menu(&mut self) {
        let Some(rect) = self.chrome.button_rect_for(Action::Overflow) else {
            return;
        };
        let snap = self.snapshot();
        let items = self.chrome.overflow_menu(&snap);
        if items.is_empty() {
            return;
        }
        // Anchor the menu at the button's bottom-left, in screen coords.
        let mut pt = POINT {
            x: rect.left,
            y: rect.bottom,
        };
        unsafe { ClientToScreen(self.frame as HWND, &mut pt) };
        let chosen = unsafe {
            // Same foreground idiom as the actions popup so an outside click dismisses cleanly.
            SetForegroundWindow(self.frame as HWND);
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            for (i, item) in items.iter().enumerate() {
                let mut flags = MF_STRING;
                if !item.enabled {
                    flags |= MF_GRAYED;
                }
                if item.checked {
                    flags |= MF_CHECKED;
                }
                let label = wide(item.label);
                AppendMenuW(menu, flags, OVERFLOW_ID_BASE + i, label.as_ptr());
            }
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_LEFTBUTTON,
                pt.x,
                pt.y,
                0,
                self.frame as HWND,
                ptr::null(),
            );
            DestroyMenu(menu);
            // 0 = dismissed; otherwise map the id back to the item's action.
            (cmd as usize)
                .checked_sub(OVERFLOW_ID_BASE)
                .and_then(|idx| items.get(idx))
                .map(|item| item.action)
        };
        if let Some(action) = chosen {
            self.do_action(action);
        }
    }

    /// Open the same actions popup at a point in the *view* window's client area — the right-click
    /// menu over the image (`x`/`y` are the WM_RBUTTONUP coordinates).
    fn actions_menu_at_view(&mut self, x: i32, y: i32) {
        let mut pt = POINT { x, y };
        unsafe { ClientToScreen(self.view as HWND, &mut pt) };
        self.show_actions_menu(pt);
    }

    /// Build the actions popup at a screen point, track it, and perform the chosen entry on the
    /// current image. Shared by [`Self::actions_menu`] (toolbar button) and [`Self::actions_menu_at_view`]
    /// (right-click). A no-op if there's no image.
    fn show_actions_menu(&mut self, pt: POINT) {
        let Some(image) = self.current_path.clone() else {
            return;
        };
        unsafe {
            // The documented idiom so the menu dismisses correctly on an outside click.
            SetForegroundWindow(self.frame as HWND);
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            // Fixed file actions first (always available once an image is open).
            let show = wide("Show in Explorer");
            AppendMenuW(menu, MF_STRING, ID_SHOW_IN_EXPLORER, show.as_ptr());
            let copy_file = wide("Copy File");
            AppendMenuW(menu, MF_STRING, ID_COPY_FILE, copy_file.as_ptr());
            let copy_path = wide("Copy Path");
            AppendMenuW(menu, MF_STRING, ID_COPY_PATH, copy_path.as_ptr());
            let copy_name = wide("Copy File Name");
            AppendMenuW(menu, MF_STRING, ID_COPY_NAME, copy_name.as_ptr());
            // Then the configured "Open in…" tree, after a divider. Submenus become nested popups;
            // each leaf app gets an id of OPEN_WITH_ID_BASE + its pre-order index, collected into
            // `leaves` so the returned command id maps straight back to the app to launch. Ids start
            // at OPEN_WITH_ID_BASE so they never collide with the fixed actions above.
            let mut leaves: Vec<&crate::config::MenuEntry> = Vec::new();
            if !self.open_with.is_empty() {
                AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
                build_open_with_menu(menu, &self.open_with, &mut leaves);
            }
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_LEFTBUTTON,
                pt.x,
                pt.y,
                0,
                self.frame as HWND,
                ptr::null(),
            );
            DestroyMenu(menu); // recursive — also frees the nested submenu popups appended above
            match cmd as usize {
                ID_SHOW_IN_EXPLORER => show_in_explorer(&image),
                ID_COPY_FILE => copy_file_to_clipboard(self.frame as HWND, &image),
                ID_COPY_PATH => {
                    copy_text_to_clipboard(self.frame as HWND, &image.to_string_lossy())
                }
                ID_COPY_NAME => copy_text_to_clipboard(self.frame as HWND, &file_name(&image)),
                id if id >= OPEN_WITH_ID_BASE => {
                    if let Some(app) = leaves.get(id - OPEN_WITH_ID_BASE) {
                        launch_external(app, &image);
                    }
                }
                _ => {} // 0 = dismissed, or an unknown id
            }
        }
    }

    /// Map a virtual-key press to a view command (layout-independent VK codes).
    fn handle_key(&mut self, vk: u32) {
        // While a transport field is being typed into, route Enter/Tab/Esc to the editor first so
        // keystrokes don't leak to view commands.
        if self.transport.is_editing() {
            match vk {
                0x0D | 0x09 => {
                    // Enter / Tab commit.
                    if let Some(snap) = self.transport_snapshot() {
                        if let Some(edit) = self.transport.commit(&snap) {
                            self.apply_transport_edit(edit);
                        }
                    }
                    self.invalidate_transport();
                    return;
                }
                0x1B => {
                    // Esc cancels the edit (does not leave the mode / close the window).
                    self.transport.cancel_edit();
                    self.invalidate_transport();
                    return;
                }
                _ => return, // other keys handled via WM_CHAR
            }
        }
        // Space / , / . drive playback when flipbook mode is active.
        if self.flipbook_state().is_some() {
            match vk {
                0x20 => return self.flipbook_key(TransportEdit::TogglePlay), // Space
                0xBC => return self.flipbook_step(-1),                       // ,
                0xBE => return self.flipbook_step(1),                        // .
                _ => {}
            }
        }
        match vk {
            0x46 => self.surface.fit(),                                 // F
            0x31 => self.surface.one_to_one(),                          // 1
            0x52 => self.surface.toggle_channel(Channel::R),            // R
            0x47 => self.surface.toggle_channel(Channel::G),            // G
            0x42 => self.surface.toggle_channel(Channel::B),            // B
            0x41 => self.surface.toggle_channel(Channel::A),            // A
            0x43 => self.surface.set_channel(Channel::Rgb),             // C
            0x54 => self.surface.toggle_tonemap(),                      // T
            0x4B => return self.toggle_flipbook(),                      // K
            0xDD => self.surface.adjust_exposure(EXPOSURE_STEP),        // ]
            0xDB => self.surface.adjust_exposure(-EXPOSURE_STEP),       // [
            0xBB | 0x6B => self.surface.zoom_centered(ZOOM_STEP),       // = / numpad +
            0xBD | 0x6D => self.surface.zoom_centered(1.0 / ZOOM_STEP), // - / numpad -
            // ← / → walk the folder. navigate() runs its own load + repaint, so return
            // afterwards rather than falling through to the shared invalidate below.
            0x25 => return self.navigate(-1), // Left
            0x27 => return self.navigate(1),  // Right
            0x7A => self.toggle_fullscreen(), // F11
            // Esc leaves full-screen if in it; otherwise it closes the window.
            0x1B => {
                if self.fullscreen {
                    self.set_fullscreen(false);
                } else {
                    unsafe { DestroyWindow(self.frame as HWND) };
                }
            }
            _ => return,
        }
        self.invalidate_chrome();
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
        if s.playing {
            self.apply_transport_edit(TransportEdit::TogglePlay);
        }
        self.apply_transport_edit(TransportEdit::Scrub(pos));
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
            tonemap: s.tonemap(),
            is_hdr,
            has_image,
            has_alpha: s.has_alpha(),
            background: s.background(),
            outline: s.outline(),
            can_navigate: self.folder.as_ref().is_some_and(|f| f.len() > 1),
            fullscreen: self.fullscreen,
            flipbook: self.flipbook_state().is_some(),
            has_animation: self.surface.frame_delay_ms().is_some(),
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

    /// The rect reserved for the image view. In full-screen the chrome is hidden and the view
    /// covers the entire client; otherwise it sits between the toolbar and status bar.
    fn view_rect(&self) -> (i32, i32, i32, i32) {
        let (w, h) = self.client();
        if self.fullscreen {
            return (0, 0, w.max(0), h.max(0));
        }
        let top = self.chrome.metrics.toolbar_h;
        // The flipbook transport band, when shown, sits between the view and the status bar.
        let band = if self.transport_visible() {
            self.chrome.metrics.transport_h
        } else {
            0
        };
        let vh = (h - top - self.chrome.metrics.status_h - band).max(0);
        (0, top, w.max(0), vh)
    }

    /// Flip in/out of borderless full-screen (toolbar button, F11, Esc, or middle-click).
    fn toggle_fullscreen(&mut self) {
        self.set_fullscreen(!self.fullscreen);
    }

    /// Enter (`on`) or leave borderless full-screen. Entering strips the window's border/caption and
    /// grows it to cover the monitor it's on (Raymond Chen's documented technique), saving the
    /// windowed placement so exit restores the exact prior position/size + maximized state. The
    /// chrome isn't painted while full-screen because [`Self::view_rect`] lets the child view cover
    /// the whole client — the `SetWindowPos` here fires `WM_SIZE`, which relays out and repositions
    /// the view for the new mode. No-op if already in the requested state.
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

    /// Reposition the child view to the current view rect (its own WM_SIZE resizes/refits the
    /// surface).
    fn reposition_view(&self) {
        let (x, y, w, h) = self.view_rect();
        unsafe {
            SetWindowPos(
                self.view as HWND,
                ptr::null_mut(),
                x,
                y,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// The empty state: no image loaded and none loading. Here the frame paints the drop /
    /// double-click hint (and the D3D view is hidden), and a double-click over the viewport opens
    /// the file picker. During a load we keep the view up (still showing the previous image, or the
    /// backdrop for the very first open) so the hint never flashes over a file the user just opened.
    fn empty_view_active(&self) -> bool {
        self.surface.current_image().is_none() && !self.loading
    }

    /// Show the D3D view when there's an image (or one is loading); otherwise hide it so the frame
    /// can paint the empty-state hint in the same region. Hiding a `WS_CLIPCHILDREN` child hands its
    /// rect back to the parent, so the frame's `WM_PAINT` gets the viewport area to draw the hint.
    /// Idempotent — called after every image-state change (load / decode / fail) and once at startup.
    fn sync_empty_view(&self) {
        let empty = self.empty_view_active();
        unsafe { ShowWindow(self.view as HWND, if empty { SW_HIDE } else { SW_SHOW }) };
        // Repaint the hint when we just went empty; when going non-empty the freshly shown view
        // repaints itself (via the load/decode `surface.invalidate`), so no frame repaint is needed.
        if empty {
            let (x, y, w, h) = self.view_rect();
            let vr = RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            unsafe { InvalidateRect(self.frame as HWND, &vr, 0) };
        }
    }

    /// Open the system file picker (empty-viewport double-click / on-screen hint) and load the
    /// chosen image. The common Open dialog pumps its own modal loop on the UI thread, like the
    /// actions popup menu; a cancel is a no-op.
    fn open_via_dialog(&mut self) {
        if let Some(path) = open_file_dialog(self.frame as HWND) {
            self.open(OpenRequest::new(path));
        }
    }

    /// Recompute the toolbar layout for the current DPI/size and visible button set (the HDR group
    /// shows only for float sources, so this also re-runs when an image is adopted/cleared).
    fn relayout(&mut self) {
        let (w, _) = self.client();
        let snap = self.snapshot();
        self.chrome.relayout(w, &snap);
        // Lay out the transport band's widgets when it's shown (its rect depends on the client size).
        if let Some(band) = self.transport_band_rect() {
            self.transport.layout(band, &self.chrome);
        }
    }

    /// Invalidate the toolbar + status strips (the chrome) without disturbing the view child.
    fn invalidate_chrome(&self) {
        let (w, h) = self.client();
        let tb = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: self.chrome.metrics.toolbar_h,
        };
        let sb = RECT {
            left: 0,
            top: h - self.chrome.metrics.status_h,
            right: w,
            bottom: h,
        };
        unsafe {
            InvalidateRect(self.frame as HWND, &tb, 0);
            InvalidateRect(self.frame as HWND, &sb, 0);
        }
        // The transport band sits above the status bar; refresh it too when present.
        self.invalidate_transport();
    }

    /// Invalidate only the status strip (e.g. on a zoom change).
    fn invalidate_status(&self) {
        let (w, h) = self.client();
        let sb = RECT {
            left: 0,
            top: h - self.chrome.metrics.status_h,
            right: w,
            bottom: h,
        };
        unsafe { InvalidateRect(self.frame as HWND, &sb, 0) };
    }

    /// Show the tooltip for the currently-hovered toolbar button (called when the hover-delay
    /// timer fires). Anchors the bubble just below the toolbar, left-aligned to the button.
    fn show_tooltip(&mut self) {
        let Some(idx) = self.chrome.hover else { return };
        let snap = self.snapshot();
        let Some((rect, text)) = self.chrome.button_tooltip(idx, &snap) else {
            return;
        };
        // Drop the bubble below the whole toolbar (a small DPI-scaled gap), left-aligned to the
        // button; convert from frame-client coords to the screen coords the popup wants.
        let gap = self.chrome.metrics.dpi as i32 * 2 / 96;
        let mut pt = POINT {
            x: rect.left,
            y: self.chrome.metrics.toolbar_h + gap,
        };
        unsafe { ClientToScreen(self.frame as HWND, &mut pt) };
        self.tooltip.show(text, pt.x, pt.y);
    }

    /// Hide the tooltip and cancel any pending hover-delay timer.
    fn cancel_tooltip(&mut self) {
        unsafe { KillTimer(self.frame as HWND, TIP_TIMER_ID) };
        self.tooltip.hide();
    }
}

/// Create the frame + child view, wire up the decode pool, optionally serve the pipe
/// (single-instance mode), open `initial` if given, and run the message loop until the window
/// is closed (the process then exits — non-resident).
pub fn run(
    initial: Option<PathBuf>,
    serve_pipe: bool,
    hot_reload: bool,
    fit_upscale: bool,
    open_with: Vec<crate::config::MenuEntry>,
) {
    unsafe {
        let hinstance = GetModuleHandleW(ptr::null());

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
        chrome::apply_dark_menus(frame, dark);

        let dpi = GetDpiForWindow(frame).max(96);
        let ch = Chrome::new(dpi, dark);
        let tooltip = Tooltip::new(frame as isize, dpi, dark);
        let chip = HintChip::new(frame as isize, dpi, dark);

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

        let mut surface = GpuSurface::new(
            view as isize,
            hinstance as isize,
            vw as u32,
            vh as u32,
            fit_upscale,
        );
        surface.set_clear(ch.view_clear_packed());
        // Workers and the pipe server post to the frame (it owns title/size/lifecycle).
        let pool = DecodePool::new(frame as isize);
        // Hot-reload watcher (config-gated); posts WM_APP_FILE_CHANGED to the frame, same as the
        // pool. None when disabled, so no watch thread is spawned.
        let watcher = hot_reload.then(|| FileWatcher::spawn(frame as isize));

        let mut app = Box::new(App {
            frame: frame as isize,
            view: view as isize,
            surface,
            pool,
            chrome: ch,
            tooltip,
            file_label: String::new(),
            meta: String::new(),
            loading: false,
            folder: None,
            current_path: None,
            watcher,
            open_with,
            fullscreen: false,
            windowed_placement: std::mem::zeroed(),
            flipbook: HashMap::new(),
            flipbook_last_tick: None,
            resume_after_scrub: false,
            transport_shown: false,
            chip,
            transport: Transport::default(),
        });
        app.relayout();

        // Open the launch path immediately (decode is async; the image swaps in via
        // WM_APP_DECODE_DONE once the loop runs).
        if let Some(path) = initial {
            app.open(OpenRequest::new(path));
        }

        // Set the initial view visibility: hidden (frame paints the drop / double-click hint) when
        // launched empty, shown when a launch path is loading. `open` above already synced for the
        // with-file case; this covers the no-file case before the frame is first shown.
        app.sync_empty_view();

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

    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            let (w, h) = app.client();
            let snap = app.snapshot();
            app.chrome.paint_toolbar(ps.hdc, w, &snap);
            app.chrome.paint_status(ps.hdc, w, h, &snap);
            // Flipbook transport band (above the status bar) when in flipbook mode.
            if let (Some(band), Some(tsnap)) = (app.transport_band_rect(), app.transport_snapshot())
            {
                app.transport.paint(ps.hdc, band, &app.chrome, &tsnap);
            }
            // Empty state: the D3D view is hidden, so the frame owns the viewport region and paints
            // the drop / double-click hint there (matching the double-click-to-open wiring below).
            if app.empty_view_active() {
                let (vx, vy, vw, vh) = app.view_rect();
                let vr = RECT {
                    left: vx,
                    top: vy,
                    right: vx + vw,
                    bottom: vy + vh,
                };
                app.chrome.paint_empty_view(ps.hdc, &vr);
            }
            EndPaint(hwnd, &ps);
            0
        }
        WM_SIZE => {
            app.cancel_tooltip();
            app.relayout();
            app.reposition_view();
            app.invalidate_chrome();
            // Reposition/hide the hint chip for the new size (also handles minimize/restore).
            app.sync_chip();
            0
        }
        WM_MOVE => {
            // The hint chip is a separate top-level popup, so it must follow the frame.
            app.sync_chip();
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_GETMINMAXINFO => {
            // Clamp the minimum track size so the toolbar can't be squeezed until its buttons
            // overlap: the chrome reports the smallest *client* size that still lays out (right group
            // + collapsed "»"), which we widen to an outer window rect for the frame's current style
            // and DPI. Windows pre-fills the other MINMAXINFO fields, so we override only the min.
            let mmi = &mut *(lparam as *mut MINMAXINFO);
            let (cw, ch) = app.chrome.min_client_size();
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
        WM_LBUTTONDOWN => {
            let x = (lparam & 0xffff) as u16 as i16 as i32;
            let y = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
            app.cancel_tooltip();
            // A press anywhere commits any in-progress typed field edit first.
            if app.transport.is_editing() {
                if let Some(snap) = app.transport_snapshot() {
                    if let Some(edit) = app.transport.commit(&snap) {
                        app.apply_transport_edit(edit);
                    }
                }
            }
            // Transport band press (grid/count/play/slider/fps/blend).
            if let Some(band) = app.transport_band_rect() {
                if y >= band.top && y < band.bottom {
                    if let Some(snap) = app.transport_snapshot() {
                        let press = app.transport.press(x, y, &snap);
                        if press.capture {
                            SetCapture(hwnd);
                        }
                        if press.slider {
                            // Pause during a scrub; resume on release if it was playing.
                            app.resume_after_scrub = snap.playing;
                            if snap.playing {
                                app.apply_transport_edit(TransportEdit::TogglePlay);
                            }
                        }
                        if let Some(edit) = press.edit {
                            app.apply_transport_edit(edit);
                        }
                        app.invalidate_transport();
                    }
                    return 0;
                }
            }
            let snap = app.snapshot();
            if let Some(action) = app.chrome.hit_test(x, y, &snap) {
                // The actions button opens a popup menu, which needs the button's screen rect, so
                // it's handled here rather than in the rect-less `do_action`.
                match action {
                    Action::OpenWithMenu => app.actions_menu(),
                    // The "»" popup, like the actions menu, needs the button's screen rect.
                    Action::Overflow => app.overflow_menu(),
                    _ => app.do_action(action),
                }
            }
            0
        }
        WM_LBUTTONUP => {
            if app.transport.is_dragging() {
                ReleaseCapture();
                let was_slider = app.transport.release();
                if was_slider && app.resume_after_scrub {
                    app.resume_after_scrub = false;
                    app.apply_transport_edit(TransportEdit::TogglePlay);
                }
                // A field click (no drag) entered type-edit mode → take focus so the following
                // WM_CHAR / Enter / Esc reach the frame (the view child may have held focus).
                if app.transport.is_editing() {
                    SetFocus(hwnd);
                }
                app.invalidate_transport();
            }
            0
        }
        WM_LBUTTONDBLCLK => {
            // Double-clicking the empty viewport opens the file picker (matches the on-screen hint).
            // Only the empty state reaches here: once an image loads the D3D view is shown and
            // catches its own clicks. The region check keeps double-clicks on the chrome inert.
            if app.empty_view_active() {
                let x = (lparam & 0xffff) as u16 as i16 as i32;
                let y = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
                let (vx, vy, vw, vh) = app.view_rect();
                if x >= vx && x < vx + vw && y >= vy && y < vy + vh {
                    app.open_via_dialog();
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            let x = (lparam & 0xffff) as u16 as i16 as i32;
            let y = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
            // An active transport drag (mouse captured) takes moves anywhere, even outside the band.
            if app.transport.is_dragging() {
                if let Some(snap) = app.transport_snapshot() {
                    if let Some(edit) = app.transport.drag_to(x, &snap) {
                        app.apply_transport_edit(edit);
                    }
                }
                return 0;
            }
            // Transport band hover (below the toolbar): repaint on change.
            if let Some(band) = app.transport_band_rect() {
                if y >= band.top && y < band.bottom {
                    if app.transport.set_hover(x, y) {
                        app.invalidate_transport();
                    }
                    if app.chrome.hover.is_some() {
                        app.chrome.hover = None;
                        app.invalidate_chrome();
                    }
                    return 0;
                } else if app.transport.clear_hover() {
                    app.invalidate_transport();
                }
            }
            let hov = if y < app.chrome.metrics.toolbar_h {
                app.chrome.hover_index(x, y)
            } else {
                None
            };
            if hov != app.chrome.hover {
                app.chrome.hover = hov;
                app.invalidate_chrome();
                // The hovered button changed: drop any showing tip and re-arm the hover delay.
                app.cancel_tooltip();
                if hov.is_some() {
                    // Ask for WM_MOUSELEAVE so the hover clears when the cursor exits.
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut tme);
                    SetTimer(hwnd, TIP_TIMER_ID, TIP_DELAY_MS, None);
                }
            }
            0
        }
        WM_TIMER => {
            match wparam {
                TIP_TIMER_ID => {
                    KillTimer(hwnd, TIP_TIMER_ID);
                    app.show_tooltip();
                }
                ANIM_TIMER_ID => app.tick_animation(),
                FLIPBOOK_TIMER_ID => app.tick_flipbook(),
                _ => {}
            }
            0
        }
        WM_MOUSELEAVE => {
            app.cancel_tooltip();
            if app.chrome.hover.is_some() {
                app.chrome.hover = None;
                app.invalidate_chrome();
            }
            if app.transport.clear_hover() {
                app.invalidate_transport();
            }
            0
        }
        WM_ACTIVATE => {
            // Losing activation (alt-tab / click away): drop the topmost tip and the hint chip so
            // they can't linger over other windows; on regaining activation, re-show the chip if it
            // still applies. Still defer to DefWindowProc for the default focus handling.
            if (wparam & 0xffff) == WA_INACTIVE as usize {
                app.cancel_tooltip();
                app.chip.hide();
            } else {
                app.sync_chip();
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_MOUSEWHEEL => {
            // WM_MOUSEWHEEL carries *screen* coords; convert to client to test the transport band.
            let notches = ((wparam >> 16) & 0xffff) as u16 as i16 as i32 / 120;
            let mut pt = POINT {
                x: (lparam & 0xffff) as u16 as i16 as i32,
                y: ((lparam >> 16) & 0xffff) as u16 as i16 as i32,
            };
            ScreenToClient(hwnd, &mut pt);
            let over_band = app
                .transport_band_rect()
                .is_some_and(|b| pt.y >= b.top && pt.y < b.bottom);
            if over_band {
                let ctrl = (GetKeyState(0x11) as u16 & 0x8000) != 0; // VK_CONTROL
                if let Some(snap) = app.transport_snapshot() {
                    if let Some(edit) = app.transport.wheel(pt.x, pt.y, notches, ctrl, &snap) {
                        app.apply_transport_edit(edit);
                    }
                }
            } else if notches != 0 {
                // Over the image: zoom about the surface's tracked cursor (position ignored here).
                app.surface.zoom_at_cursor(ZOOM_STEP.powf(notches as f32));
                app.invalidate_status();
            }
            0
        }
        WM_CHAR => {
            // Typed input for a transport numeric field (the codebase's only typed field). Digits
            // (and `.` for fps) and Backspace go to the active edit; nothing else consumes them.
            if app.transport.is_editing() {
                if let Some(ch) = char::from_u32(wparam as u32) {
                    if app.transport.type_char(ch) {
                        app.invalidate_transport();
                    }
                }
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
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
        WM_APP_FILE_CHANGED => {
            app.reload(wparam as u64);
            0
        }
        WM_APP_FLIPBOOK_CHIP => {
            // The hint chip was clicked: accept enters flipbook mode, dismiss hides it for good.
            match wparam {
                CHIP_ACCEPT => app.toggle_flipbook(),
                CHIP_DISMISS => {
                    if let Some(e) = app.flipbook_entry() {
                        e.hint_dismissed = true;
                    }
                    app.sync_chip();
                }
                _ => {}
            }
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
            app.cancel_tooltip();
            // Cancel any in-progress transport drag/edit (their pixel anchors are DPI-stale).
            app.transport.cancel_drag();
            app.transport.cancel_edit();
            app.chrome.set_dpi(new_dpi);
            app.tooltip.set_dpi(new_dpi);
            app.chip.set_dpi(new_dpi);
            app.relayout();
            app.reposition_view();
            app.sync_chip();
            InvalidateRect(hwnd, ptr::null(), 0);
            0
        }
        WM_SETTINGCHANGE => {
            // A theme switch (and much else) arrives here; re-detect and re-skin if changed.
            let dark = chrome::system_uses_dark_mode();
            if dark != app.chrome.dark {
                app.chrome.set_dark(dark);
                app.tooltip.set_dark(dark);
                app.chip.set_dark(dark);
                app.surface.set_clear(app.chrome.view_clear_packed());
                chrome::apply_dark_titlebar(hwnd, dark);
                chrome::apply_dark_menus(hwnd, dark);
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
            save_window_state(hwnd, app);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Child view proc. Same panic firewall as [`frame_wndproc`]: the real handling is in
/// [`view_wndproc_impl`], behind `catch_unwind`, so a panic in (e.g.) a paint can't unwind into
/// the Win32 dispatcher and abort.
unsafe extern "system" fn view_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        view_wndproc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(lr) => lr,
        Err(_) => {
            eprintln!("fire: recovered from a panic in view_wndproc (msg {msg:#06x})");
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

/// Child view handling: D3D11 present + image navigation (LMB-drag pan, wheel + RMB-drag zoom,
/// keys). Wrapped by [`view_wndproc`]'s panic firewall.
unsafe fn view_wndproc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
            let x = (lparam & 0xffff) as u16 as i16 as f32;
            let y = ((lparam >> 16) & 0xffff) as u16 as i16 as f32;
            SetCapture(hwnd);
            SetFocus(hwnd); // take keyboard focus so nav keys reach this window
                            // Sync the pan origin to the press point so the first move's delta is measured from
                            // here, not a stale position. Matters after the context menu (or any gap where the
                            // view saw no WM_MOUSEMOVE): without this, the cursor is still at the right-click
                            // point and the first drag lurches the image toward the click. (RMB does the same.)
            app.surface.on_cursor_moved((x, y));
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
            // A right *click* (the gesture never moved past the zoom-drag slop) opens the actions
            // menu at the cursor; an actual zoom-drag just ends.
            if !app.surface.end_zoom_drag() {
                let x = (lparam & 0xffff) as u16 as i16 as i32;
                let y = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
                app.actions_menu_at_view(x, y);
            }
            0
        }
        WM_MBUTTONDOWN => {
            // A middle-click anywhere over the image toggles full-screen. Take focus so Esc/F11
            // reach this window afterward.
            SetFocus(hwnd);
            app.toggle_fullscreen();
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

/// File extensions Fire can decode, driving the Open dialog's "Image files" filter. Mirrors the
/// installer's Explorer associations (`installer/fire.iss`) and `fire-decode`'s routing; the raw
/// list is `raw.rs`'s `EXTENSIONS`. A file the filter misses is still openable via "All files"
/// (the decoder routes by magic bytes regardless of extension).
const SUPPORTED_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "tif", "tiff", "webp", "ico", "tga",
    "hdr", "exr", "psd", "psb", "heic", "heif", "avif", "cr2", "cr3", "nef", "nrw", "arw", "sr2",
    "raf", "orf", "rw2", "pef", "srw", "dng", "3fr", "iiq", "erf", "mrw", "dcr", "kdc", "mef",
    "mos", "raw",
];

/// Build the double-NUL-terminated `lpstrFilter` for [`open_file_dialog`]: an "Image files" entry
/// listing every supported extension, then an "All files" catch-all. The filter is `label\0pattern\0`
/// pairs ended by one extra NUL (the `GetOpenFileNameW` contract).
fn image_filter_wide() -> Vec<u16> {
    let patterns: String = SUPPORTED_EXTS
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

/// Run the common Open dialog (owned by the frame) filtered to supported image formats, returning
/// the chosen path or `None` on cancel. `GetOpenFileNameW` pumps its own modal loop on the UI
/// thread, like the actions popup menu; no COM init is needed for the classic picker.
fn open_file_dialog(owner: HWND) -> Option<PathBuf> {
    let filter = image_filter_wide();
    let mut file_buf = vec![0u16; 4096];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = file_buf.as_mut_ptr();
    ofn.nMaxFile = file_buf.len() as u32;
    ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY;
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return None; // cancelled or dismissed
    }
    let end = file_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(file_buf.len());
    Some(PathBuf::from(OsString::from_wide(&file_buf[..end])))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Recursively append the "Open in…" `entries` to `menu`: a submenu (an entry with children) becomes
/// a nested `MF_POPUP`, and a leaf app (an entry with a `path`) becomes a command item whose id is
/// `OPEN_WITH_ID_BASE + leaves.len()`. `leaves` accumulates the launch targets in command-id order,
/// so [`App::show_actions_menu`] can map the returned id straight back to the app. Malformed entries
/// (neither `path` nor `items`) are skipped. The created submenu popups are owned by `menu` once
/// appended, so the caller's single `DestroyMenu(menu)` frees them all.
///
/// # Safety
/// `menu` must be a valid menu handle; called only from inside the `unsafe` block in
/// [`App::show_actions_menu`].
unsafe fn build_open_with_menu<'a>(
    menu: HMENU,
    entries: &'a [crate::config::MenuEntry],
    leaves: &mut Vec<&'a crate::config::MenuEntry>,
) {
    for entry in entries {
        let label = wide(&entry.name);
        if entry.is_submenu() {
            let sub = unsafe { CreatePopupMenu() };
            if sub.is_null() {
                continue; // out of menu handles; skip this submenu rather than abort the whole menu
            }
            unsafe {
                build_open_with_menu(sub, &entry.items, leaves);
                // MF_POPUP reinterprets the id argument as the submenu handle; ownership transfers
                // to `menu`, so `sub` needs no separate DestroyMenu.
                AppendMenuW(menu, MF_POPUP, sub as usize, label.as_ptr());
            }
        } else if entry.path.is_some() {
            let id = OPEN_WITH_ID_BASE + leaves.len();
            leaves.push(entry);
            unsafe { AppendMenuW(menu, MF_STRING, id, label.as_ptr()) };
        }
        // else: malformed entry (no `path`, no `items`) — silently skipped.
    }
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

/// Put UTF-16 `text` on the clipboard as `CF_UNICODETEXT` (the "Copy Path" / "Copy File Name"
/// actions). Best-effort: on any failure we free our own allocation — clipboard ownership of the
/// `HGLOBAL` only transfers once `SetClipboardData` succeeds — and bail without disturbing the
/// existing clipboard contents beyond the `EmptyClipboard` we already issued.
fn copy_text_to_clipboard(owner: HWND, text: &str) {
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = utf16.len() * std::mem::size_of::<u16>();
    unsafe {
        if OpenClipboard(owner) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !h.is_null() {
            let dst = GlobalLock(h) as *mut u16;
            if !dst.is_null() {
                ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
                GlobalUnlock(h);
                if SetClipboardData(CF_UNICODETEXT, h).is_null() {
                    GlobalFree(h); // ownership didn't transfer; release it
                }
            } else {
                GlobalFree(h);
            }
        }
        CloseClipboard();
    }
}

/// Put `image` on the clipboard as `CF_HDROP` (the "Copy File" action), so it can be pasted into
/// Explorer or another app as the file itself. Layout per the `DROPFILES` contract: the header,
/// then the wide path (with its NUL), then one extra NUL ending the (single-entry) list. Same
/// best-effort ownership rule as [`copy_text_to_clipboard`].
fn copy_file_to_clipboard(owner: HWND, image: &Path) {
    let path: Vec<u16> = image
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let header = std::mem::size_of::<DROPFILES>();
    // header + the path (incl. its NUL) + one extra NUL ending the double-NUL-terminated list.
    let bytes = header + (path.len() + 1) * std::mem::size_of::<u16>();
    unsafe {
        if OpenClipboard(owner) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !h.is_null() {
            let base = GlobalLock(h) as *mut u8;
            if !base.is_null() {
                ptr::write_bytes(base, 0, bytes); // zero the header fields + the trailing NUL
                let df = base as *mut DROPFILES;
                (*df).pFiles = header as u32; // byte offset from the header to the path list
                (*df).fWide = 1; // paths are UTF-16
                let dst = base.add(header) as *mut u16;
                ptr::copy_nonoverlapping(path.as_ptr(), dst, path.len());
                GlobalUnlock(h);
                if SetClipboardData(CF_HDROP, h).is_null() {
                    GlobalFree(h);
                }
            } else {
                GlobalFree(h);
            }
        }
        CloseClipboard();
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
