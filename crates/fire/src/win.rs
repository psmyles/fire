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

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Instant;

use fire_decode::{DecodeOptions, DecodedImage};
use fire_ipc::OpenRequest;

use crate::chrome::{Action, ViewSnapshot};
use crate::flipbook::{self, FlipbookState, Grid, PerPath};
use crate::render::gpu::{FlipbookParams, GpuSurface};
use crate::render::imgui::Imgui;
use crate::transport::{TransportEdit, TransportSnapshot};
use crate::ui::theme::Metrics;

use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromWindow,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
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
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowPlacement,
    KillTimer, LoadCursorW, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
    SetWindowTextW, ShowWindow, TrackPopupMenu, TranslateMessage, CS_DBLCLKS, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, GWL_STYLE, HMENU, HWND_TOP, IDC_ARROW, MF_CHECKED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MINMAXINFO, MSG, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SW_FORCEMINIMIZE,
    SW_MAXIMIZE, SW_MINIMIZE, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED,
    SW_SHOWMINNOACTIVE, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_LEFTBUTTON, TPM_RETURNCMD, TPM_TOPALIGN,
    WINDOWPLACEMENT, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_DWMCOLORIZATIONCOLORCHANGED, WM_DPICHANGED, WM_DROPFILES, WM_GETMINMAXINFO, WM_KEYDOWN,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_SIZE, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSW,
    WPF_RESTORETOMAXIMIZED, WS_OVERLAPPEDWINDOW,
};

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
const ID_SETTINGS: usize = 5;
const OPEN_WITH_ID_BASE: usize = 100;
/// Command ids for the "»" overflow popup: `OVERFLOW_ID_BASE + index` into the items returned by
/// [`Chrome::overflow_menu`]. Kept clear of the actions-popup ranges above (this menu is separate,
/// but a distinct base keeps the two unambiguous).
const OVERFLOW_ID_BASE: usize = 1000;

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
/// OK / Apply in the settings dialog: LPARAM is `Box<Config>`, the edited settings. Posted (never
/// sent) so the frame adopts them under its own `&mut App` rather than the dialog holding one across
/// its modal loop — see [`crate::settings`].
pub const WM_APP_SETTINGS_APPLY: u32 = WM_APP + 7;
/// "Settings…" was chosen from the right-click menu. Posted rather than opened inline because that
/// menu is tracked from inside an `&mut App` borrow, which the dialog's nested loop must not overlap.
pub const WM_APP_OPEN_SETTINGS: u32 = WM_APP + 8;

/// Open the popup menu the UI asked for this frame. Posted (not called) so `TrackPopupMenu`'s
/// nested modal pump runs *after* `WM_PAINT` has completed — see [`App::apply_ui`].
pub const WM_APP_OPEN_MENU: u32 = WM_APP + 9;

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
    /// Current theme. Re-read on `WM_SETTINGCHANGE` / `WM_DWMCOLORIZATIONCOLORCHANGED`.
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
    /// A popup menu the UI asked for, deferred out of `WM_PAINT` (see [`App::apply_ui`]).
    pending_menu: Option<PendingMenu>,
}

/// A Win32 popup menu requested by the UI, waiting for the paint to finish.
struct PendingMenu {
    action: Action,
    /// Screen coords of the menu's top-left.
    pt: POINT,
    /// The overflow menu's contents (empty for the actions menu).
    items: Vec<Action>,
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
    fn fail_load(&mut self, name: &str, meta: String) {
        self.file_label = name.to_string();
        self.meta = meta;
        set_title(
            self.frame,
            &format!("{}: {name} (failed)", crate::product::NAME),
        );
        self.surface.clear_image();
        self.surface.invalidate();
        self.redraw();
        self.redraw();
        // Back to the empty state: hide the view and paint the drop / double-click hint.
        self.redraw();
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
        self.redraw();
    }

    /// The flipbook detection hint, when the chip should be offered: the current image has an
    /// undismissed hint and flipbook mode is off. Drawn by [`crate::ui`] as a panel over the image —
    /// it used to be its own layered popup window, with all the show/hide/reposition/minimize
    /// bookkeeping that implies. Now it is a function of state, evaluated per frame.
    fn chip_hint(&self) -> Option<Grid> {
        let e = self.flipbook.get(self.current_path.as_ref()?)?;
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
            Action::Background(bg) => self.surface.set_background(bg),
            // Toggling full-screen resizes the window, which fires WM_SIZE; fall through to the
            // shared redraw below.
            Action::ToggleFullscreen => self.toggle_fullscreen(),
            // Flipbook mode runs its own surface/timer sync + redraw.
            Action::ToggleFlipbook => return self.toggle_flipbook(),
            // The settings dialog runs a nested modal loop, which re-enters this wndproc and would
            // take a second `&mut App` while ours is live. Hand it to the message loop, which drops
            // our borrow first. (Same reason the popup menus are deferred — see `apply_ui`.)
            Action::OpenSettings => {
                unsafe { PostMessageW(self.frame as HWND, WM_APP_OPEN_SETTINGS, 0, 0) };
                return;
            }
            // These are reported as menu anchors, not actions; they never reach here.
            Action::OpenWithMenu | Action::Overflow => return,
        }
        self.redraw();
    }

    /// Open the deferred popup menu (posted by [`Self::apply_ui`]). `TrackPopupMenu` runs its own
    /// modal pump, so this must happen *after* the paint that requested it has finished — never
    /// inside `WM_PAINT`, where a nested pump would re-enter the still-unvalidated paint.
    fn open_pending_menu(&mut self) {
        let Some(pending) = self.pending_menu.take() else {
            return;
        };
        match pending.action {
            Action::Overflow => self.overflow_menu(pending.pt, &pending.items),
            _ => self.show_actions_menu(pending.pt),
        }
        self.redraw();
    }

    /// The "»" overflow popup: the left-group controls that didn't fit the window width, each
    /// dispatching through the normal action path when chosen. Items mirror the toolbar buttons'
    /// enabled/checked state.
    fn overflow_menu(&mut self, pt: POINT, items: &[Action]) {
        if items.is_empty() {
            return;
        }
        let snap = self.snapshot();
        let chosen = unsafe {
            // The documented foreground idiom, so an outside click dismisses cleanly.
            SetForegroundWindow(self.frame as HWND);
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            for (i, action) in items.iter().enumerate() {
                let mut flags = MF_STRING;
                if !snap.enabled(*action) {
                    flags |= MF_GRAYED;
                }
                if snap.active(*action) {
                    flags |= MF_CHECKED;
                }
                let label = wide(&snap.tooltip(*action));
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
                .copied()
        };
        if let Some(action) = chosen {
            self.do_action(action);
        }
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
            // Fixed file actions first, each shown unless hidden in `[context-menu]`. All four can
            // be off, leaving just the open-with tree.
            let cm = self.cfg.context_menu;
            let fixed = [
                (cm.show_in_explorer, ID_SHOW_IN_EXPLORER, "Show in Explorer"),
                (cm.copy_file, ID_COPY_FILE, "Copy File"),
                (cm.copy_path, ID_COPY_PATH, "Copy Path"),
                (cm.copy_file_name, ID_COPY_NAME, "Copy File Name"),
            ];
            let mut shown = 0usize;
            for (on, id, text) in fixed {
                if on {
                    let label = wide(text);
                    AppendMenuW(menu, MF_STRING, id, label.as_ptr());
                    shown += 1;
                }
            }
            // Then the configured "Open in…" tree, after a divider. Submenus become nested popups;
            // each leaf app gets an id of OPEN_WITH_ID_BASE + its pre-order index, collected into
            // `leaves` so the returned command id maps straight back to the app to launch. Ids start
            // at OPEN_WITH_ID_BASE so they never collide with the fixed actions above.
            let mut leaves: Vec<&crate::config::MenuEntry> = Vec::new();
            if !self.cfg.open_with.is_empty() {
                if shown > 0 {
                    AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
                }
                build_open_with_menu(menu, &self.cfg.open_with, &mut leaves);
                shown += 1;
            }
            // "Settings…" always comes last, after a divider — it's the one entry that isn't about
            // the image.
            if shown > 0 {
                AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
            }
            let settings = wide("Settings\u{2026}");
            AppendMenuW(menu, MF_STRING, ID_SETTINGS, settings.as_ptr());
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
                // Not opened inline: we're inside an `&mut self` borrow, and the dialog's nested
                // message loop re-enters the wndproc, which takes its own. Post and let the loop
                // deliver it once this borrow is gone.
                ID_SETTINGS => {
                    PostMessageW(self.frame as HWND, WM_APP_OPEN_SETTINGS, 0, 0);
                }
                id if id >= OPEN_WITH_ID_BASE => {
                    if let Some(app) = leaves.get(id - OPEN_WITH_ID_BASE) {
                        launch_external(app, &image);
                    }
                }
                _ => {} // 0 = dismissed, or an unknown id
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
            // Navigation runs its own load + repaint (and relayout), so return without the shared
            // invalidate below.
            KeyAction::PrevImage => return self.navigate(-1),
            KeyAction::NextImage => return self.navigate(1),
            KeyAction::ToggleFullscreen => self.toggle_fullscreen(),
            // Esc leaves full-screen if in it; otherwise it closes the window.
            KeyAction::CloseOrExitFullscreen => {
                if self.fullscreen {
                    self.set_fullscreen(false);
                } else {
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

    /// Open the settings dialog. Takes the config *by clone* and re-enters through
    /// [`WM_APP_SETTINGS_APPLY`], because the dialog runs a nested message pump that re-enters this
    /// wndproc — so it must not be called while an `&mut App` borrow is alive. Every caller
    /// therefore reaches it via a `PostMessage`, never inline from a click handler.
    fn open_settings(&mut self) {
        let cfg = self.cfg.clone();
        let (frame, dark) = (self.frame, self.dark);
        crate::settings::run_modal(frame, cfg, dark);
        self.redraw();
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
        self.cfg.save();

        // The toolbar's tooltips carry the (possibly rebound) shortcuts, and the backdrop buttons
        // reflect the new default; the open-with menu is rebuilt per-show, so it needs nothing.
        self.redraw();
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
            loading: self.loading,
            has_alpha: s.has_alpha(),
            background: s.background(),
            outline: s.outline(),
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

        let frame = self.imgui.frame(|ui, tex| {
            crate::ui::build(
                ui,
                tex,
                &snap,
                transport.as_ref(),
                chip,
                &metrics,
                icon_px,
                dark,
                (cw as f32, ch as f32),
                (ix, iy, iw, ih),
                fullscreen,
            )
        });

        self.surface.present();
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
        // The two Win32 popup menus. `TrackPopupMenu` pumps its own modal loop, which re-enters this
        // wndproc — and we are currently *inside* `WM_PAINT`, between BeginPaint and EndPaint, with
        // an `&mut App` live. Opening one here would recurse into an unvalidated paint. So record it
        // and post: the loop delivers WM_APP_OPEN_MENU once this paint has finished and our borrow
        // is gone.
        if let Some(anchor) = frame.menu {
            let mut pt = POINT {
                x: anchor.x,
                y: anchor.y,
            };
            unsafe { ClientToScreen(self.frame as HWND, &mut pt) };
            self.pending_menu = Some(PendingMenu {
                action: anchor.action,
                pt,
                items: frame.overflow,
            });
            unsafe { PostMessageW(self.frame as HWND, WM_APP_OPEN_MENU, 0, 0) };
        }
    }

}

/// Push the view-related settings into the renderer. The one place that maps `Config` onto
/// [`GpuSurface`], shared by startup and the settings dialog's Apply, so the two can't drift.
/// Backdrop applies to the image already on screen; the fit/tonemap defaults seed the *next* adopt
/// (yanking the current image's zoom or tonemap out from under the user would be hostile).
fn apply_view_config(surface: &mut GpuSurface, cfg: &Config) {
    surface.set_fit_upscale(cfg.fit_upscale);
    surface.set_open_actual_size(cfg.default_fit == crate::config::FitCfg::ActualSize);
    surface.set_default_tonemap(cfg.default_tonemap.to_render());
    surface.set_background_pref(cfg.background.override_for_render());
}

/// Create the frame + child view, wire up the decode pool, optionally serve the pipe
/// (single-instance mode), open `initial` if given, and run the message loop until the window
/// is closed (the process then exits — non-resident).
pub fn run(initial: Option<PathBuf>, serve_pipe: bool, cfg: Config) {
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
            return;
        }
        chrome::apply_dark_titlebar(frame, dark);
        chrome::apply_dark_menus(frame, dark);

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
        surface.set_clear(crate::chrome::Palette::for_mode(dark).view_clear_packed());
        // The view-related config the surface owns (backdrop / open-fit / tonemap defaults). Same
        // path the settings dialog re-runs on Apply — see `App::apply_view_config`.
        apply_view_config(&mut surface, &cfg);

        // ImGui needs the live D3D11 device/context, so it is built from the surface.
        let mut imgui = Imgui::new(
            frame as isize,
            surface.device(),
            surface.device_context(),
            dpi,
        );
        crate::ui::theme::apply(imgui.style_mut(), dark, metrics.scale);

        // Workers and the pipe server post here (this window owns title/size/lifecycle).
        let pool = DecodePool::new(frame as isize);
        // Hot-reload watcher (config-gated); posts WM_APP_FILE_CHANGED, same as the pool. None when
        // disabled, so no watch thread is spawned.
        let watcher = cfg.hot_reload.then(|| FileWatcher::spawn(frame as isize));
        let keybinds = Keybinds::from_config(&cfg.keybinds);

        let mut app = Box::new(App {
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
            watcher,
            cfg,
            keybinds,
            fullscreen: false,
            windowed_placement: std::mem::zeroed(),
            flipbook: HashMap::new(),
            flipbook_last_tick: None,
            pending_menu: None,
        });

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

    // ImGui sees every message first (except WM_PAINT, which is ours) so it can update its input
    // state. Then two booleans decide who owns the event — replacing the entire hand-rolled
    // hover/capture/hit-test/focus layer the GDI chrome needed:
    //
    //   * `want_capture_mouse`    — the pointer is over a widget (toolbar, transport, popup).
    //   * `want_capture_keyboard` — a text field has focus, so keys are typing, not commands.
    //
    // Note this is *not* the wnd-proc handler's return value: upstream returns true only for the
    // handful of messages it fully consumes (WM_SETCURSOR and friends), never for "that click was
    // mine". Gating on the return value instead would let a click on a toolbar button *also* pan the
    // image underneath it.
    if msg != WM_PAINT {
        if app.imgui.wnd_proc(hwnd as isize, msg, wparam, lparam) {
            return 0;
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
        let key_msg = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);

        // Any input can change a hover or an active state, so give ImGui its settle frames.
        if mouse_msg || key_msg {
            app.request_frames(2);
        }

        // A pan/zoom drag already in flight owns the mouse to the end of the gesture, even if the
        // cursor strays over the chrome — otherwise the drag would stick the moment it crossed the
        // toolbar.
        if mouse_msg && !app.surface.is_mouse_captured() && app.imgui.wants_mouse() {
            return 0;
        }
        if key_msg && app.imgui.wants_keyboard() {
            return 0;
        }
    }

    match msg {
        WM_PAINT => {
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
        WM_SIZE => {
            let w = (lparam & 0xffff) as u32;
            let h = ((lparam >> 16) & 0xffff) as u32;
            // The swapchain covers the whole client; the image's sub-rect of it is recomputed in
            // `render`, so there is nothing else to lay out.
            app.surface.resize(w, h);
            app.redraw();
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

        // --- image input (only reached when ImGui didn't want the event) ---------------------

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
                let x = (lparam & 0xffff) as u16 as i16 as i32;
                let y = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
                let mut pt = POINT { x, y };
                ClientToScreen(hwnd, &mut pt);
                app.show_actions_menu(pt);
                app.redraw();
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
        WM_KEYDOWN => {
            app.handle_key(wparam as u32);
            0
        }
        // Alt chords arrive here, not as WM_KEYDOWN. Consume only the ones actually bound, so
        // Alt+F4 and the Alt-menu still reach DefWindowProc.
        WM_SYSKEYDOWN => {
            if app.handle_key(wparam as u32) {
                0
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_TIMER => {
            match wparam {
                ANIM_TIMER_ID => app.tick_animation(),
                FLIPBOOK_TIMER_ID => app.tick_flipbook(),
                _ => {}
            }
            0
        }
        WM_DROPFILES => {
            handle_drop(app, wparam as HDROP);
            0
        }

        // --- cross-thread wakeups ------------------------------------------------------------

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
        WM_APP_OPEN_SETTINGS => {
            app.open_settings();
            0
        }
        WM_APP_OPEN_MENU => {
            app.open_pending_menu();
            0
        }
        WM_APP_SETTINGS_APPLY => {
            let cfg = Box::from_raw(lparam as *mut Config);
            app.apply_settings(*cfg);
            0
        }

        // --- window / system -----------------------------------------------------------------

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
            app.metrics = Metrics::new(app.dpi);
            let (dark, scale) = (app.dark, app.metrics.scale);
            app.imgui.set_dpi(app.surface.device(), app.dpi);
            crate::ui::theme::apply(app.imgui.style_mut(), dark, scale);
            app.redraw();
            0
        }
        // A light/dark switch arrives as WM_SETTINGCHANGE (along with much else); an accent-color
        // change arrives as WM_DWMCOLORIZATIONCOLORCHANGED. Both mean "re-read the theme".
        WM_SETTINGCHANGE | WM_DWMCOLORIZATIONCOLORCHANGED => {
            let dark = chrome::system_uses_dark_mode();
            // The accent can move without the light/dark mode changing, so restyle unconditionally
            // rather than gating on `dark != app.dark` (that was a real bug in the GDI chrome).
            app.dark = dark;
            let scale = app.metrics.scale;
            crate::ui::theme::apply(app.imgui.style_mut(), dark, scale);
            app.surface
                .set_clear(crate::chrome::Palette::for_mode(dark).view_clear_packed());
            chrome::apply_dark_titlebar(hwnd, dark);
            chrome::apply_dark_menus(hwnd, dark);
            app.redraw();
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
        } else if entry.path.as_deref().is_some_and(|p| !p.trim().is_empty()) {
            let id = OPEN_WITH_ID_BASE + leaves.len();
            leaves.push(entry);
            unsafe { AppendMenuW(menu, MF_STRING, id, label.as_ptr()) };
        }
        // else: malformed entry (no usable `path`, no `items`) — silently skipped. The settings
        // dialog creates entries before they have a program, so a half-filled one just doesn't show
        // up in the menu yet.
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
