//! One viewer window: its winit window, its GPU surface and ImGui layer, the image it shows and
//! everything the user can do to it.
//!
//! This is the former Win32 `App`, with the message plumbing replaced by winit events and the
//! cross-thread `PostMessage`s by [`AppEvent`]s. The logic — open/navigate/reload, the flipbook,
//! the transport, the settings window, the actions menu — is unchanged; what changed is only how an
//! event reaches it and how a repaint or a timer is asked for.
//!
//! **Input routing is three layers, and the order is the point** ([`Viewer::window_event`]). First
//! the lifecycle events (resize, DPI, theme, drop, close) that nothing else may intercept. Then the
//! *ownership* gates for input: an armed keybind row, ImGui, the modal settings window, an open
//! popup — each there because the gate before it would otherwise swallow the event. Only what
//! survives reaches the per-family handlers (`on_mouse`, `on_key`). The three keyboard special
//! cases from the architecture notes carry over verbatim: a keybind capture takes every key before
//! ImGui sees it (Esc included), the settings window is modal but ImGui's `want_capture_keyboard`
//! cannot say so, and an open popup is *not* modal so ImGui leaves the keys to us.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fire_decode::{DecodeOptions, DecodedImage};
use fire_ipc::OpenRequest;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Fullscreen, Theme, Window, WindowId};

use crate::app::timers::{TimerKind, Timers, KINDS};
use crate::app::{AppEvent, FolderScan, Initial};
use crate::chrome::{self, Action, ViewSnapshot};
use crate::config::{Config, WheelActionCfg};
use crate::decode_pool::{DecodeJob, DecodeOutcome, DecodePool, FlipbookGuess};
use crate::flipbook::{self, FlipbookState, Grid, PerPath};
use crate::folder::{self, Folder};
use crate::keybinds::{KeyAction, KeyChord, Keybinds, ShortcutLabels};
use crate::platform::{self, LaunchShow};
use crate::render::gpu::{FlipbookParams, Gpu, GpuSurface, Presented};
use crate::render::imgui::Imgui;
use crate::render::view::Channel;
use crate::transport::{TransportEdit, TransportSnapshot};
use crate::ui::theme::Metrics;
use crate::watcher::FileWatcher;
use crate::window_state::WindowState;

/// Max decoded dimension on either axis — a CPU/RAM guard (the GPU limit is requested to match).
/// An RGBA8 bitmap at 16384² is ~1 GiB; float HDR is 4×. Larger images are CPU-downscaled.
pub const MAX_CPU_DIM: u32 = 16384;

/// The flipbook playback timer's interval: a fixed ~60 Hz, whatever the sheet's frame rate and
/// whether or not it is crossfading.
///
/// **This timer neither paces the animation nor, normally, the frames.** [`Viewer::advance_flipbook`]
/// derives the position from elapsed time, so the sheet plays at its own `fps` no matter when we
/// sample it; and while the window is visible, each frame is asked for by the *previous* frame's
/// present, which blocks until vblank and so paces playback at exactly the display's refresh rate
/// (see [`Viewer::render`]). What this timer does is **start** playback's pump and **carry** it when
/// the present can't: an occluded window, where the swapchain stops blocking.
///
/// It cannot be the pacer, and that is the point: a timer tick is coarser than a single refresh on
/// a 120 Hz panel, and uneven sampling of smooth motion is exactly what the eye reads as jitter.
const FLIPBOOK_TICK_MS: u64 = 16;

/// Cap on the playback dt applied per tick, so a stall (modal loop, sleep) doesn't jump the
/// animation far ahead when ticks resume — it just loses time, like the GIF path.
const MAX_FLIPBOOK_STEP: f32 = 0.25;

/// Caret blink tick. ImGui blinks on a wall-clock schedule; this only has to be frequent enough that
/// the phase changes look continuous.
const CARET_BLINK_MS: u64 = 33;

/// Two presses closer than this in time and space are a double-click (winit reports presses only;
/// the OS's own double-click synthesis is a Win32 class flag winit does not set). 500 ms is the
/// Windows default; the distance is the default 4 px in each direction.
///
/// The slop is in **logical** px and is scaled by the display's DPI at the comparison, because the
/// cursor arrives in physical ones: unscaled, a Retina display would halve the tolerance and make
/// double-clicks harder to land the steadier the hand needs to be.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_SLOP: f32 = 4.0;

/// Project a per-path [`FlipbookState`] to the surface's render parameters.
fn surface_flipbook(s: FlipbookState) -> FlipbookParams {
    FlipbookParams {
        grid: s.grid,
        frame_count: s.frame_count,
        frame_pos: s.frame_pos,
        blend: s.blend,
    }
}

/// When an animation frame displayed *now*, with the given delay (ms), falls due for replacement.
/// The same clamp the timer gets, so the deadline and the timer agree.
fn anim_deadline(delay_ms: u32) -> Instant {
    Instant::now() + Duration::from_millis(delay_ms.max(1) as u64)
}

/// A native dialog a viewer wants run. Requested during an event, started from the loop's idle
/// step (`Fire::run_dialogs`), and answered later by [`AppEvent::DialogDone`].
///
/// **The dialog runs on a worker thread, and that is not an optimisation.** `rfd` puts up an
/// app-modal picker that pumps its own event loop for as long as it is up. Run from inside a
/// winit callback — which is what every one of our handlers is, the idle step included — that
/// nested loop feeds events straight back into winit's dispatcher, which refuses to re-enter and
/// panics ("tried to handle event while another event is currently being handled"). The panic
/// then unwinds into a CoreFoundation run-loop callback, where unwinding is not allowed, and the
/// process aborts. It was a reliable crash: open the picker, move the mouse over it, gone.
///
/// Off-thread, `rfd` hands the panel to the main thread itself (a `dispatch_sync` on macOS,
/// per-call COM init on Windows), so the modal loop runs from the *run loop* rather than from
/// inside our handler and re-entrancy never arises. It also means the window behind the picker
/// keeps redrawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialog {
    /// Ctrl+O / the empty-viewport double-click: pick an image to open.
    OpenImage,
    /// The settings window's "Browse…": pick a program for an open-with entry.
    BrowseProgram,
}

/// Put up the native file picker and wait for it. **Worker thread only** — see [`Dialog`].
///
/// `window` is the owner: it is what the picker is modal to, and on Windows what it is centred
/// over. `rfd` resolves it on the main thread, inside its own hand-off, so passing it across the
/// thread boundary here is sound.
fn pick(dialog: Dialog, window: &Window) -> Option<PathBuf> {
    match dialog {
        // The extensions come from `fire_decode::SUPPORTED_EXTENSIONS` — the same list folder
        // navigation uses. (A file the filter misses is still openable via "All files": the
        // decoder routes by magic bytes, not by name.)
        Dialog::OpenImage => rfd::FileDialog::new()
            .set_title("Open image")
            .add_filter("Image files", fire_decode::SUPPORTED_EXTENSIONS)
            .add_filter("All files", &["*"])
            .set_parent(window)
            .pick_file(),
        Dialog::BrowseProgram => rfd::FileDialog::new()
            .set_title("Choose a program")
            .add_filter("Programs", &["exe", "com", "bat", "cmd", "app"])
            .add_filter("All files", &["*"])
            .set_parent(window)
            .pick_file(),
    }
}

/// What a viewer asked the shell for during the last event (see `Fire::after_dispatch`).
#[derive(Default)]
pub struct Requests {
    /// Close this window.
    pub close: bool,
    /// The settings window committed this config; every other window should adopt it.
    pub settings_applied: Option<Config>,
}

/// One viewer window's state.
pub struct Viewer {
    window: Arc<Window>,
    surface: GpuSurface,
    /// The Dear ImGui context + backends for this window. Draws the chrome into the frame.
    imgui: Imgui,
    timers: Timers,
    proxy: EventLoopProxy<AppEvent>,
    /// The sequence number of the pending timer of each kind, or `None` if none is wanted; a
    /// firing whose number no longer matches is stale (see [`crate::app::timers`]).
    timer_seq: [Option<u64>; KINDS],
    /// DPI-scaled chrome metrics (how tall the toolbar/status/transport are), which is what decides
    /// the image's sub-rect. Rebuilt on DPI change.
    metrics: Metrics,
    /// Current theme (which of the stylesheet's two palettes). Re-read on `ThemeChanged`.
    dark: bool,
    dpi: u32,
    /// Event-driven render pump: frames still owed. ImGui needs a frame or two after an input to
    /// settle hover/active state, so input asks for a couple; at zero we stop drawing and the
    /// window costs nothing. See [`Viewer::request_frames`].
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
    /// viewer, which stops the watch thread.
    watcher: Option<FileWatcher>,
    /// The live user settings (`config.toml`). The authority for everything the settings dialog can
    /// change: the dialog edits a clone and hands it back, which [`Viewer::apply_settings`] adopts
    /// here and pushes into the renderer/watcher/pool.
    cfg: Config,
    /// [`Keybinds::labels`] cached — rebuilt only when the bindings change (`apply_settings`),
    /// then shared into each frame's snapshot by `Arc` clone.
    shortcut_labels: Arc<ShortcutLabels>,
    /// The keyboard table, resolved from `cfg.keybinds` over the defaults. Drives both key dispatch
    /// ([`Viewer::handle_key`]) and the toolbar tooltips' shortcut suffixes.
    keybinds: Keybinds,
    /// The full-screen state as of the last check — *only* to notice a change the desktop made on
    /// its own, which is what [`Viewer::sync_fullscreen`] repaints for. Never read as the state
    /// itself: [`Viewer::fullscreen`] is that, and it asks the window.
    fullscreen_seen: bool,
    /// Per-path flipbook state (fire's only per-path map; session-only). Keyed by image path so it
    /// survives folder navigation and hot-reload. `state` holds the user's settings, `hint`/`hint_
    /// dismissed` drive the chip. See [`crate::flipbook`].
    flipbook: HashMap<PathBuf, PerPath>,
    /// Wall-clock of the previous playback tick, for dt-based advance (robust to timer jitter).
    flipbook_last_tick: Option<Instant>,
    /// When the displayed GIF frame falls due and must be replaced by the next one. `None` for a
    /// still image. The deadline — not the timer firing — is what advances the animation; see
    /// [`Viewer::advance_playback`].
    anim_due: Option<Instant>,
    /// The popup menu that is up, if any (actions or overflow). Like the settings window, an ImGui
    /// popup drawn inside our own frame: no native menu, no nested pump, no command-id table.
    menu: Option<crate::ui::MenuState>,
    /// The settings window, while it is open. It is an ImGui modal drawn inside our own frame, so
    /// it runs no nested message pump and holds no borrow: its state simply lives here and is
    /// edited during the paint. `None` = closed.
    settings: Option<crate::ui::settings::State>,
    /// Whether the caret-blink timer is currently wanted (see [`Viewer::sync_caret_timer`]).
    caret_timer: bool,
    /// Leftover fraction of a wheel notch, when the wheel is set to walk the folder
    /// ([`WheelActionCfg::NavigateFolder`]). A precision touchpad or tilt wheel scrolls in
    /// fractions of a notch; zooming consumes those naturally (a fractional power of `zoom_step`),
    /// but "next image" is discrete, so the fractions are banked here until they make a whole
    /// notch. Without this, a fine-grained wheel would either do nothing at all or, if we rounded
    /// up, skip several images per flick. Reset on direction change so a reversal is immediate
    /// rather than having to pay off the other direction's debt first.
    wheel_notches: f32,
    /// The live modifier keys, from `ModifiersChanged`.
    mods: ModifiersState,
    /// The cursor's last known position, in client physical px.
    cursor: (f32, f32),
    /// The previous left press, for double-click synthesis.
    last_click: Option<(Instant, (f32, f32))>,
    /// The window's placement while it was last neither maximized, minimized nor full-screen —
    /// what `window.toml` remembers. winit has no "restored rect" query, so it is tracked here
    /// from the move/resize events instead.
    normal_pos: Option<PhysicalPosition<i32>>,
    normal_size: Option<PhysicalSize<u32>>,
    /// Whether the last size event found the window maximized; a change persists the placement.
    maximized: bool,
    requests: Requests,
    dialog: Option<Dialog>,
    /// A picker is up. One at a time: see `run_dialog`.
    dialog_running: bool,
}

impl Viewer {
    /// Create the window, bring the GPU up on it (the first time), build the ImGui layer, the
    /// decode plumbing and the keybind table. The window is created hidden; [`Viewer::show`]
    /// reveals it once everything behind it exists.
    ///
    /// `Err` is a startup failure the caller owns telling the user about.
    pub fn new(
        el: &ActiveEventLoop,
        gpu: impl FnOnce() -> Result<Rc<Gpu>, String>,
        cfg: Config,
        pool: DecodePool,
        timers: Timers,
        proxy: EventLoopProxy<AppEvent>,
    ) -> Result<Self, String> {
        let t_window = Instant::now();
        // Restore the remembered size now, so the surface starts at the right size. The
        // launcher's Run setting (a Windows shortcut's Normal/Minimized/Maximized) wins for the
        // show state; otherwise the remembered maximized state is restored.
        let saved = WindowState::load();
        let launcher = platform::launcher_show();
        let maximized = match launcher {
            Some(LaunchShow::Maximized) => true,
            Some(LaunchShow::Minimized) | Some(LaunchShow::Normal) => false,
            None => saved.is_some_and(|s| s.maximized),
        };
        let mut attrs = Window::default_attributes()
            .with_title(crate::product::NAME)
            .with_visible(false)
            .with_maximized(maximized)
            .with_window_icon(window_icon());
        match &saved {
            Some(s) => {
                attrs = attrs
                    .with_inner_size(PhysicalSize::new(
                        s.width.max(200) as u32,
                        s.height.max(150) as u32,
                    ))
                    .with_position(PhysicalPosition::new(s.x, s.y));
            }
            None => attrs = attrs.with_inner_size(PhysicalSize::new(1280u32, 800u32)),
        }
        let window = Arc::new(
            el.create_window(attrs)
                .map_err(|e| format!("could not create the window: {e}"))?,
        );
        let dpi = ((window.scale_factor() * 96.0).round() as u32).max(96);
        let dark = window
            .theme()
            .or_else(|| el.system_theme())
            .is_none_or(|t| t == Theme::Dark);
        let metrics = Metrics::new(dpi);
        crate::render::gpu::report_timing(&format!(
            "window — {:.2} ms",
            t_window.elapsed().as_secs_f64() * 1e3
        ));

        // The GPU, joined only now: the window above came up while the bring-up thread was
        // still creating the device, instead of after it.
        let gpu = gpu().map_err(|e| format!("the GPU could not be initialized: {e}"))?;

        // The surface covers the whole client; the image is drawn into a sub-rect of it,
        // recomputed every frame (see `Viewer::image_rect`).
        let t = Instant::now();
        let size = window.inner_size();
        let mut surface = GpuSurface::new(
            gpu,
            Arc::clone(&window),
            size.width.max(1),
            size.height.max(1),
            cfg.fit_upscale,
        )?;
        crate::render::gpu::report_timing(&format!(
            "swapchain — {:.2} ms",
            t.elapsed().as_secs_f64() * 1e3
        ));
        surface.set_clear(crate::ui::theme::view_clear_packed(dark));
        // The view-related config the surface owns (backdrop / open-fit / tonemap defaults). Same
        // path the settings dialog re-runs on Apply — see `apply_view_config`.
        apply_view_config(&mut surface, &cfg);
        // The octagon overlay's options — persisted ones if the user opted in, defaults otherwise;
        // always starts switched off.
        surface.set_octagon(cfg.octagon.initial_state());

        let t = Instant::now();
        let mut imgui = Imgui::new(Arc::clone(&window), dpi)?;
        let scale = metrics.scale;
        imgui.restyle(|style| crate::ui::theme::apply(style, dark, scale));
        crate::render::gpu::report_timing(&format!(
            "imgui — {:.2} ms",
            t.elapsed().as_secs_f64() * 1e3
        ));

        // Hot-reload watcher (config-gated); sends `FileChanged`. None when disabled, so no watch
        // thread is spawned.
        let watcher = cfg
            .hot_reload
            .then(|| FileWatcher::spawn(proxy.clone(), window.id()));
        let keybinds = Keybinds::from_config(&cfg.keybinds);
        let shortcut_labels = Arc::new(keybinds.labels());

        let me = Viewer {
            window,
            surface,
            imgui,
            timers,
            proxy,
            timer_seq: [None; KINDS],
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
            shortcut_labels,
            keybinds,
            fullscreen_seen: false,
            flipbook: HashMap::new(),
            flipbook_last_tick: None,
            anim_due: None,
            menu: None,
            settings: None,
            caret_timer: false,
            wheel_notches: 0.0,
            mods: ModifiersState::empty(),
            cursor: (0.0, 0.0),
            last_click: None,
            normal_pos: None,
            normal_size: None,
            maximized,
            requests: Requests::default(),
            dialog: None,
            dialog_running: false,
        };
        me.apply_min_size();
        if launcher == Some(LaunchShow::Minimized) {
            me.window.set_minimized(true);
        }
        Ok(me)
    }

    /// Reveal the window. Its first appearance is already in the remembered state, and it takes
    /// the foreground on its own — we are the process the launcher just started. (A *forwarded*
    /// open raises explicitly; see [`Viewer::open`].)
    pub fn show(&mut self) {
        self.window.set_visible(true);
        self.redraw();
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// Adopt the launch path, whose decode is already in flight (submitted in `main` before this
    /// window existed): the title, the hot-reload watch and the folder scan, exactly as
    /// [`Viewer::load`] would have done, minus the decode.
    pub fn adopt_initial(&mut self, init: Initial) {
        let name = file_name(&init.path);
        self.file_label.clone_from(&name);
        self.loading = true;
        self.set_title(&format!("{}: {name} (loading…)", crate::product::NAME));
        self.surface.set_generation(init.generation);
        self.current_path = Some(init.path.clone());
        if let Some(w) = &self.watcher {
            w.watch(init.generation, &init.path);
        }
        self.scan_folder(init.path);
    }

    /// Everything the viewer asked the shell for during the last event.
    pub fn take_requests(&mut self) -> Requests {
        std::mem::take(&mut self.requests)
    }

    pub fn take_dialog(&mut self) -> Option<Dialog> {
        self.dialog.take()
    }

    // --- open / navigate / reload ------------------------------------------------------------

    /// Handle an open request (launch / drop / forward): load the image, and kick off a fresh
    /// folder scan so ←/→ navigation and the status-bar count repopulate for the new directory.
    pub fn open(&mut self, req: OpenRequest) {
        // Drop the old cursor immediately; the scan below rebuilds it (and the count fills in
        // after the image shows — lazy). Without this a stale "3 / 27" would linger until then.
        self.folder = None;
        self.load(&req.path, req.flags.activate);
        self.scan_folder(req.path);
    }

    /// Show the window for `path`, raise it if `activate`, and enqueue the decode off-thread. The
    /// currently displayed image is *kept on screen* until the new one lands (swapped in by
    /// [`Viewer::decode_done`]), so navigating between folder siblings doesn't flash the empty
    /// backdrop between frames — the same no-blank-flash discipline hot-reload uses. A failed
    /// decode clears it. Returns the generation assigned to this load. Shared by `open` (which
    /// also rescans the folder) and `navigate` (which reuses the existing cursor).
    fn load(&mut self, path: &Path, activate: bool) -> u64 {
        let name = file_name(path);
        // clone_from reuses file_label's existing allocation; this runs on every navigation.
        self.file_label.clone_from(&name);
        self.meta.clear();
        self.loading = true;
        self.set_title(&format!("{}: {name} (loading…)", crate::product::NAME));
        if activate {
            // Spend the one-shot foreground grant promptly (architecture §4.1).
            self.raise();
        }
        self.surface.invalidate();
        // Repaint: the title/status changed, and with no image yet the HDR group (if it was
        // showing) must drop out of the layout now. `redraw` is idempotent — it raises
        // `frames_wanted` to 2 rather than accumulating — so once is all there is.
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
            window: Some(self.id()),
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
    /// away since the watch was armed) is dropped by generation, like every other cross-thread event.
    pub fn reload(&mut self, generation: u64) {
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

    /// Walk the folder by a wheel turn of `notches` (positive = wheel away from the user), when the
    /// wheel is configured for navigation. Wheel *up* goes to the **previous** image, matching
    /// Explorer's preview pane and Windows Photos.
    ///
    /// Fractional turns are banked in `wheel_notches` rather than rounded, so a precision touchpad
    /// steps one image per notch's worth of scrolling instead of either doing nothing or racing
    /// through the folder. A flick that spans several notches moves several images — one `load` each
    /// would be wasted work, so the cursor is advanced in one hop and only the image landed on is
    /// decoded.
    fn wheel_navigate(&mut self, notches: f32) {
        if notches == 0.0 {
            return;
        }
        // Nowhere to go (folder scan not landed yet, or a single-image folder): bank nothing.
        // Deducting first would silently consume scrolling that `navigate` then ignores.
        if self.folder.as_ref().is_none_or(|f| f.len() <= 1) {
            return;
        }
        // A reversal starts from zero: carrying the old direction's fraction would swallow the
        // first turn back.
        if self.wheel_notches != 0.0
            && self.wheel_notches.is_sign_negative() != notches.is_sign_negative()
        {
            self.wheel_notches = 0.0;
        }
        self.wheel_notches += notches;
        let whole = self.wheel_notches.trunc();
        if whole == 0.0 {
            return;
        }
        self.wheel_notches -= whole;
        self.navigate(-(whole as isize)); // wheel up (positive) = previous image
    }

    /// Scan `path`'s folder for sibling images off the UI thread, sending the result back as
    /// [`AppEvent::FolderScanned`]. Mirrors the decode pool's discipline: the worker never touches
    /// the window or renderer, only sends an event.
    fn scan_folder(&self, path: PathBuf) {
        let proxy = self.proxy.clone();
        let window = self.id();
        let spawned = std::thread::Builder::new()
            .name("fire-folder-scan".into())
            .spawn(move || {
                let entries = folder::scan(&path);
                let payload = Box::new(FolderScan { path, entries });
                // A closed event loop (the app is exiting) just drops it.
                let _ = proxy.send_event(AppEvent::FolderScanned(window, payload));
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
    /// Stale-dropped by **path**, not by the decode generation the rest of the cross-thread events
    /// use. A folder cursor describes a *directory*, not a decode: a hot-reload re-decodes the same
    /// file and bumps the generation, so a generation check would discard a scan that was still in
    /// flight for the very image we are still showing — and discard it permanently, because `open`
    /// already cleared the cursor and nothing but `open` starts a scan. ←/→ and the "n / m" count
    /// would then stay dead for the rest of the session. The path is what actually identifies the
    /// scan, and it still rejects the case the guard is for: a scan left over from a previous open
    /// of a *different* image.
    pub fn folder_scanned(&mut self, scan: FolderScan) {
        if self.current_path.as_deref() != Some(scan.path.as_path()) {
            return; // superseded by an open of a different image
        }
        self.folder = Folder::new(scan.entries, &scan.path);
        self.redraw();
    }

    /// Handle a finished decode. Adopt it only if it is still the latest request (stale-drop).
    pub fn decode_done(&mut self, outcome: DecodeOutcome) {
        if outcome.generation != self.surface.generation() {
            return; // superseded by a newer open
        }
        let name = file_name(&outcome.path);
        self.loading = false;
        match outcome.result {
            Ok(img) => {
                let (w, h, fmt) = (img.width, img.height, img.source_format);
                self.file_label.clone_from(&name);
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
                    self.surface.replace_image_keep_view(img, &outcome.mips)
                } else {
                    self.surface.set_image(img, &outcome.mips)
                };
                // The image decoded fine but the GPU may still reject the upload (e.g. out of
                // memory on a very large texture). Treat that like a decode failure rather than
                // letting it take the process down.
                if let Err(e) = upload {
                    eprintln!("fire: GPU upload failed for {name}: {e}");
                    self.fail_load(&name, format!("failed: GPU upload ({e})"));
                    return;
                }
                // The surface now holds this image: from here the flipbook transport, its edits and
                // its playback are about *this* path (see `shown_path`). Set before `apply_flipbook`
                // below, which reads it.
                self.shown_path = Some(outcome.path.clone());
                self.set_title(&format!("{}: {name}", crate::product::NAME));
                self.surface.invalidate();
                // A float source brings in the HDR group; an LDR one drops it — relayout either
                // way. (One call: `redraw` is idempotent, not accumulating.)
                self.redraw();
                // Start playback if this is an animated GIF; stop any prior animation otherwise.
                self.sync_animation();
                // Re-apply any per-path flipbook state for the adopted image (restores it on
                // navigate-back). The auto-detection hint for a fresh open arrives *later*, via
                // `FlipbookGuess` (kept off the time-to-first-pixel path), and re-applies then — so
                // a new sheet shows instantly and the chip pops a beat afterward.
                self.apply_flipbook();
                eprintln!("fire: opened {name} ({w}x{h}, {fmt})");
            }
            Err(e) => {
                eprintln!("fire: failed to open {name}: {e}");
                self.fail_load(&name, format!("failed: {e}"));
            }
        }
    }

    /// Apply a flipbook auto-detection result that arrived after its image. Stale-dropped by
    /// generation like a decode, so a guess for an image the user has already navigated away from
    /// is ignored. On a match, `current_path` is the guess's path, so recording the hint and
    /// re-applying pops the chip for the visible image.
    pub fn flipbook_guess_done(&mut self, guess: FlipbookGuess) {
        if guess.generation != self.surface.generation() {
            return; // superseded by a newer open
        }
        self.flipbook.entry(guess.path).or_default().hint = guess.guess;
        self.apply_flipbook();
    }

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

    /// Shared failure path for a load (failed decode *or* failed GPU upload): show `meta` in the
    /// status bar, mark the title failed, and drop any stale image. We don't clear in `load` (to
    /// avoid the navigation flash), so a broken file shouldn't keep showing the previously
    /// displayed one — that's why this repaints the backdrop here.
    fn fail_load(&mut self, name: &str, meta: String) {
        self.file_label = name.to_string();
        self.meta = meta;
        self.set_title(&format!("{}: {name} (failed)", crate::product::NAME));
        self.reset_to_empty();
    }

    // --- timers --------------------------------------------------------------------------------

    /// Arm (or re-arm) the timer of `kind` to fire in `ms`. Replaces any pending one of that kind.
    fn arm(&mut self, kind: TimerKind, ms: u64) {
        let at = Instant::now() + Duration::from_millis(ms.max(1));
        let seq = self.timers.borrow_mut().arm(self.id(), kind, at);
        self.timer_seq[kind as usize] = Some(seq);
    }

    /// Cancel the timer of `kind`. Its heap entry, if any, is dropped when it pops.
    fn kill(&mut self, kind: TimerKind) {
        self.timer_seq[kind as usize] = None;
    }

    /// A timer fell due. Ignored unless it is the one currently wanted for its kind.
    pub fn on_timer(&mut self, kind: TimerKind, seq: u64) {
        if self.timer_seq[kind as usize] != Some(seq) {
            return; // re-armed or killed since
        }
        self.timer_seq[kind as usize] = None;
        match kind {
            TimerKind::Anim => self.tick_animation(),
            TimerKind::Flipbook => self.tick_flipbook(),
            // The caret is drawn by ImGui, so blinking it is just another frame — and another tick,
            // for as long as a text field is being edited.
            TimerKind::Caret => {
                self.request_frames(1);
                if self.caret_timer {
                    self.arm(TimerKind::Caret, CARET_BLINK_MS);
                }
            }
        }
    }

    /// (Re)start or stop GIF playback for the freshly adopted image. Arms the animation timer to
    /// the current frame's delay when the image is animated; kills it otherwise. Called after every
    /// adopt (fresh open, navigate, hot-reload, failed load) so switching to a still image — or a
    /// decode failure — stops the previous animation.
    fn sync_animation(&mut self) {
        match self.surface.frame_delay_ms() {
            Some(delay) => {
                self.anim_due = Some(anim_deadline(delay));
                self.arm(TimerKind::Anim, delay.max(1) as u64);
            }
            None => {
                self.anim_due = None;
                self.kill(TimerKind::Anim);
            }
        }
    }

    /// Advance the animated image one frame: upload the next frame, repaint the view, and reschedule
    /// the timer (and the deadline) for that frame's delay. If the image is no longer animated (e.g.
    /// it was cleared) the timer is stopped.
    fn tick_animation(&mut self) {
        match self.surface.advance_frame() {
            Some(delay) => {
                self.anim_due = Some(anim_deadline(delay));
                self.arm(TimerKind::Anim, delay.max(1) as u64);
                self.surface.invalidate();
            }
            None => {
                self.anim_due = None;
                self.kill(TimerKind::Anim);
            }
        }
    }

    /// Bring both time-driven playbacks — the GIF's frame and the flipbook's cell — up to *now*,
    /// asking for no frame of its own. Called at the top of every redraw, so whatever caused the
    /// frame we are about to draw, it shows the image that belongs to this instant.
    ///
    /// **Why a paint may not assume the playback timers have fired.** Timers are the lowest
    /// priority wakeup there is: a deadline is only dispatched once the queue holds no event and
    /// no pending redraw. Moving the mouse supplies both at once — every `CursorMoved` asks ImGui
    /// for its settle frames — and both playback timers are starved for the whole gesture.
    /// Advancing only on the timer therefore froze playback the moment the mouse moved, and unfroze
    /// it when the mouse stopped. Deriving the position from elapsed time instead makes every frame
    /// correct no matter what asked for it, and leaves the timers doing what they always did:
    /// asking for a frame when nothing else would.
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
    /// from the last one, and so whether [`Viewer::render`] should ask for another after it.
    fn flipbook_playing(&self) -> bool {
        self.flipbook_state()
            .is_some_and(|s| s.playing && s.frame_count > 1)
    }

    /// Whether the transport band is shown (flipbook active, windowed).
    fn transport_visible(&self) -> bool {
        !self.fullscreen() && self.flipbook_state().is_some()
    }

    /// Mirror the active per-path state onto the surface (or clear it) and re-arm the timer.
    ///
    /// The band appearing/disappearing changes the image's sub-rect, but nothing needs to track that
    /// here any more: the rect is recomputed from scratch every frame in [`Viewer::render`], which is
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
        self.apply_flipbook(); // ends in a redraw of its own
    }

    /// Arm/kill the flipbook playback timer to match the active state. Paused/off = no timer.
    fn sync_flipbook_timer(&mut self) {
        let playing = self
            .flipbook_state()
            .is_some_and(|s| s.playing && s.frame_count > 1);
        if playing {
            if self.flipbook_last_tick.is_none() {
                self.flipbook_last_tick = Some(Instant::now());
            }
            self.arm(TimerKind::Flipbook, FLIPBOOK_TICK_MS);
        } else {
            self.flipbook_last_tick = None;
            self.kill(TimerKind::Flipbook);
        }
    }

    /// The flipbook timer fired: advance, ask for the frame that shows it, and re-arm. On an idle
    /// (or occluded) window this is what paces playback; the advance itself is
    /// [`Viewer::advance_flipbook`], which every paint also runs (see [`Viewer::advance_playback`]
    /// for why it has to).
    fn tick_flipbook(&mut self) {
        if self.advance_flipbook() {
            self.redraw();
        }
        self.sync_flipbook_timer();
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
            .map_or(0.0, |t| (now - t).as_secs_f32().min(MAX_FLIPBOOK_STEP));
        s.frame_pos = (s.frame_pos + dt * s.fps).rem_euclid(s.frame_count as f32);
        let pos = s.frame_pos;
        self.flipbook_last_tick = Some(now);
        self.surface.set_flipbook_pos(pos);
        true
    }

    /// The flipbook detection hint, when the chip should be offered: the current image has an
    /// undismissed hint and flipbook mode is off. Drawn by [`crate::ui`] as a panel over the image.
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
                // One rule for both axes: when the count was tracking the full grid, keep it
                // tracking after the axis changes.
                TransportEdit::SetCols(v) | TransportEdit::SetRows(v) => {
                    let follow = s.frame_count == s.grid.cols * s.grid.rows;
                    *(if matches!(edit, TransportEdit::SetCols(_)) {
                        &mut s.grid.cols
                    } else {
                        &mut s.grid.rows
                    }) = v;
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

    // --- commands -------------------------------------------------------------------------------

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
            Action::Channel(Channel::Rgb | Channel::Rgba) => self.surface.toggle_composite(),
            Action::Channel(c) => self.surface.toggle_channel(c),
            Action::ToggleTonemap => self.surface.toggle_tonemap(),
            Action::MipDown => self.surface.step_mip(-1),
            Action::MipUp => self.surface.step_mip(1),
            Action::ExpUp => self.surface.adjust_exposure(self.cfg.exposure_step),
            Action::ExpReset => self.surface.reset_exposure(),
            Action::ExpDown => self.surface.adjust_exposure(-self.cfg.exposure_step),
            Action::ToggleOutline => self.surface.toggle_outline(),
            Action::ToggleOctagon => self.surface.toggle_octagon(),
            Action::Background(bg) => self.surface.set_background(bg),
            // Toggling full-screen resizes the window, which fires `Resized`; fall through to the
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
    /// detached processes or touch the clipboard, and none of them pumps a message loop.
    fn do_command(&mut self, cmd: crate::ui::Command) {
        use crate::ui::Command;
        let Some(image) = self.current_path.clone() else {
            // Settings isn't about the image, so it still works with nothing open.
            if cmd == Command::OpenSettings {
                self.open_settings();
            }
            return;
        };
        match cmd {
            Command::ShowInExplorer => platform::reveal_in_file_manager(&image),
            Command::CopyFile => platform::copy_file_to_clipboard(&image),
            Command::CopyPath => platform::copy_text_to_clipboard(&image.to_string_lossy()),
            Command::CopyFileName => platform::copy_text_to_clipboard(&file_name(&image)),
            Command::OpenSettings => self.open_settings(),
            Command::OpenWith(path) => {
                if let Some(app) = crate::config::entry_at(&self.cfg.open_with, &path) {
                    launch_external(app, &image);
                }
            }
        }
    }

    /// The chord a key press makes with the modifiers currently held. `Primary` is Ctrl on
    /// Windows and Linux and ⌘ on macOS (architecture.md appendix A, D9).
    fn chord(&self, key: KeyCode) -> KeyChord {
        let primary = if cfg!(target_os = "macos") {
            self.mods.super_key()
        } else {
            self.mods.control_key()
        };
        KeyChord {
            key,
            primary,
            alt: self.mods.alt_key(),
            shift: self.mods.shift_key(),
        }
    }

    /// Route a key press through the keybind table. Returns whether the press was consumed.
    fn handle_key(&mut self, key: KeyCode) -> bool {
        // There is no "is a transport field being typed into?" preamble here. ImGui answers that
        // with `want_capture_keyboard`, checked in the routing before we are ever called — so a
        // key that reaches here is, by construction, not text input. That *deletes* a class of bug
        // (a half-typed field stranded by a rebound Esc) rather than guarding against it.
        let chord = self.chord(key);
        // Flipbook-context bindings (play/pause, step) win while the mode is active, and are inert
        // outside it — the precedence the table encodes.
        let in_flipbook = self.flipbook_state().is_some();
        let Some(action) = self.keybinds.lookup(chord, in_flipbook) else {
            return false;
        };
        self.perform_key_action(action);
        true
    }

    /// Perform a bound keyboard command. Also the macOS menu bar's entry point (D16), so a menu
    /// item and its accelerator cannot diverge.
    pub(crate) fn perform_key_action(&mut self, action: KeyAction) {
        match action {
            // Both file commands run their own repaint (and the picker pumps a modal loop), so they
            // return without the shared invalidate below.
            KeyAction::OpenFile => return self.open_via_dialog(),
            KeyAction::CloseImage => return self.close_image(),
            KeyAction::Fit => self.surface.fit(),
            KeyAction::ActualSize => self.surface.one_to_one(),
            KeyAction::ZoomIn => self.surface.zoom_centered(self.cfg.zoom_step),
            KeyAction::ZoomOut => self.surface.zoom_centered(1.0 / self.cfg.zoom_step),
            // Same command as the toolbar's composite button: RGBA↔RGB on an image with alpha, and
            // the all-channels reset from a solo (or on an image without one).
            KeyAction::ChannelRgb => self.surface.toggle_composite(),
            KeyAction::ChannelR => self.surface.toggle_channel(Channel::R),
            KeyAction::ChannelG => self.surface.toggle_channel(Channel::G),
            KeyAction::ChannelB => self.surface.toggle_channel(Channel::B),
            KeyAction::ChannelA => self.surface.toggle_channel(Channel::A),
            KeyAction::ToggleTonemap => self.surface.toggle_tonemap(),
            KeyAction::MipFiner => self.surface.step_mip(-1),
            KeyAction::MipCoarser => self.surface.step_mip(1),
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
                if self.fullscreen() {
                    self.set_fullscreen(false);
                } else if self.cfg.esc_closes_window {
                    self.requests.close = true;
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

    // --- settings -------------------------------------------------------------------------------

    /// Open the settings window: seed it from the live config and let the next paint draw it.
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

    /// Feed a key press to the armed keybind row, with the live modifier state (the settings
    /// module is pure UI and never touches the window system).
    fn settings_capture(&mut self, key: KeyCode) {
        let chord = self.chord(key);
        if let Some(s) = &mut self.settings {
            s.capture_key(chord);
        }
    }

    /// The settings window's two shell-level keys: **Esc** cancels (discarding the draft), **Enter**
    /// commits and closes. Everything else the window needs, ImGui already routed.
    fn settings_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Escape => {
                self.settings = None;
                self.redraw();
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                if let Some(cfg) = self.settings.as_mut().map(|s| s.commit()) {
                    self.apply_settings(cfg);
                }
                self.settings = None;
                self.redraw();
            }
            _ => {}
        }
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
        if want {
            self.arm(TimerKind::Caret, CARET_BLINK_MS);
        } else {
            self.kill(TimerKind::Caret);
        }
    }

    /// Adopt the settings the dialog committed (OK / Apply): push each field wherever it lives, then
    /// persist. Applied *before* saving, so an unwritable `config.toml` still costs the user
    /// persistence rather than the edit. The shell then hands the same config to every other window
    /// ([`Viewer::adopt_settings`]).
    ///
    /// Three tiers, by how far a change can reach without being obnoxious:
    ///   * **Live** — watcher, zoom/exposure steps, backdrop, keybinds, menu contents.
    ///   * **Next image** — the fit/tonemap an image *opens* with, and the flipbook playback
    ///     defaults: re-fitting or re-tonemapping the picture under the user's cursor would undo
    ///     whatever they'd just set up by hand.
    ///   * **Next launch** — `open-in` is read by the owner when a forwarded open arrives, so it is
    ///     live too, but only for opens from *other* launches.
    fn apply_settings(&mut self, new: Config) {
        self.adopt_settings(new);
        // Opting into octagon persistence captures the overlay options as they are *right now* —
        // the settings draft only carries the checkbox; the live overlay is the authority.
        if self.cfg.octagon.remember {
            self.capture_octagon();
        }
        self.cfg.save();
        self.requests.settings_applied = Some(self.cfg.clone());
    }

    /// Adopt a config another window committed (or this one's, before it is saved): everything
    /// [`Viewer::apply_settings`] does except persisting it.
    pub fn adopt_settings(&mut self, new: Config) {
        // Hot-reload: start or stop the watch thread, re-arming it on the open image.
        if new.hot_reload != self.cfg.hot_reload {
            if new.hot_reload {
                let w = FileWatcher::spawn(self.proxy.clone(), self.id());
                if let Some(p) = &self.current_path {
                    w.watch(self.surface.generation(), p);
                }
                self.watcher = Some(w);
            } else {
                self.watcher = None; // dropping it stops the thread
            }
        }
        self.keybinds = Keybinds::from_config(&new.keybinds);
        self.shortcut_labels = Arc::new(self.keybinds.labels());
        apply_view_config(&mut self.surface, &new);
        self.cfg = new;
        // The toolbar's tooltips carry the (possibly rebound) shortcuts, and the backdrop buttons
        // reflect the new default; the open-with menu is rebuilt per-show, so it needs nothing.
        self.redraw();
    }

    /// Copy the octagon overlay's persistable options (color/opacity/crop/hide — never the
    /// on/off toggle) from the live surface into `cfg.octagon`, reporting whether anything
    /// actually changed. The two persistence paths below share this so the field list exists
    /// once.
    fn capture_octagon(&mut self) -> bool {
        let s = self.surface.octagon();
        let oc = &mut self.cfg.octagon;
        let changed = (oc.color, oc.line_opacity, oc.crop, oc.hide)
            != (s.color, s.line_opacity, s.crop, s.hide);
        oc.color = s.color;
        oc.line_opacity = s.line_opacity;
        oc.crop = s.crop;
        oc.hide = s.hide;
        changed
    }

    /// Persist the octagon overlay's options into `config.toml` on exit, when the user opted in
    /// (Settings ▸ Overlay). The on/off toggle is never persisted — a launch always starts with
    /// the overlay off.
    fn persist_octagon(&mut self) {
        if self.cfg.octagon.remember && self.capture_octagon() {
            self.cfg.save();
        }
    }

    // --- dialogs --------------------------------------------------------------------------------

    /// Open the system file picker (Ctrl+O, the empty-viewport double-click, the on-screen hint)
    /// and load the chosen image. Deferred to the loop's idle step (see [`Dialog`]).
    fn open_via_dialog(&mut self) {
        self.dialog = Some(Dialog::OpenImage);
    }

    /// Start a deferred dialog on its own thread. The answer comes back as
    /// [`AppEvent::DialogDone`]; see [`Dialog`] for why it must not run here.
    pub fn run_dialog(&mut self, dialog: Dialog) {
        // One picker at a time. It is app-modal, so the user cannot normally ask for a second —
        // but a stray request must not put up two panels or spawn a thread per frame.
        if self.dialog_running {
            return;
        }
        let (proxy, window, id) = (self.proxy.clone(), Arc::clone(&self.window), self.id());
        let spawned = std::thread::Builder::new()
            .name("fire-file-dialog".into())
            .spawn(move || {
                let path = pick(dialog, &window);
                // A closed event loop means the app is exiting; the answer has nowhere to go.
                let _ = proxy.send_event(AppEvent::DialogDone {
                    window: id,
                    dialog,
                    path,
                });
            });
        match spawned {
            Ok(_) => self.dialog_running = true,
            // Refusing to open a picker is better than opening one that crashes the process.
            Err(e) => eprintln!("fire: could not start the file-dialog thread: {e}"),
        }
    }

    /// Apply a finished dialog. A cancel (`None`) is a no-op beyond releasing the guard.
    pub fn dialog_done(&mut self, dialog: Dialog, path: Option<PathBuf>) {
        self.dialog_running = false;
        let Some(path) = path else {
            return;
        };
        match dialog {
            Dialog::OpenImage => self.open(OpenRequest::new(path)),
            Dialog::BrowseProgram => {
                // The settings window may have been closed while the picker was up.
                if let Some(s) = &mut self.settings {
                    s.set_program(&path.to_string_lossy());
                }
                self.redraw();
            }
        }
    }

    // --- window ---------------------------------------------------------------------------------

    fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    /// Show, un-minimize, and bring the window to the foreground (a forwarded open).
    fn raise(&self) {
        if self.window.is_minimized() == Some(true) {
            self.window.set_minimized(false);
        }
        self.window.set_visible(true);
        self.window.focus_window();
    }

    /// Client size in physical px.
    fn client(&self) -> (i32, i32) {
        let s = self.window.inner_size();
        (s.width as i32, s.height as i32)
    }

    /// The cursor, translated into **image-region** coords.
    ///
    /// All the pan/zoom math in [`crate::render::view`] is relative to the image's sub-rect, not the
    /// window. Forgetting the origin would offset every drag by the toolbar's height, so it lives in
    /// one place rather than at each call site.
    fn image_cursor(&self) -> (f32, f32) {
        let (ox, oy) = self.surface.image_origin();
        (self.cursor.0 - ox, self.cursor.1 - oy)
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
        if self.fullscreen() {
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
    /// hits zero no more redraw is requested and the window goes back to costing nothing.
    fn request_frames(&mut self, n: u8) {
        self.frames_wanted = self.frames_wanted.max(n);
        self.window.request_redraw();
    }

    /// The everyday repaint: something changed, draw it.
    fn redraw(&mut self) {
        self.request_frames(2);
    }

    /// Whether the window is full-screen — **asked of the window, never remembered**.
    ///
    /// The desktop has its own ways in and out that never pass through [`Self::set_fullscreen`]:
    /// on macOS the green traffic-light button, ⌃⌘F, the Window menu and a Mission Control swipe;
    /// on Windows the shell's own arrangements. A `bool` that only our toggle wrote goes stale the
    /// first time one of them is used, and the failure is not subtle — the window comes back
    /// windowed while the app still believes it is full-screen, so [`crate::ui::build`] keeps the
    /// chrome hidden and the toolbar is simply not there to click. winit tracks the real state on
    /// every path (its window delegate updates it from AppKit's own will-enter/did-exit
    /// notifications, not just from our call), so that is the copy to read, and reading it costs a
    /// borrow.
    fn fullscreen(&self) -> bool {
        self.window.fullscreen().is_some()
    }

    /// Repaint if the full-screen state moved without us asking.
    ///
    /// [`Self::fullscreen`] always reports the truth, but nothing *tells* us when the truth
    /// changed: winit has no full-screen event, and AppKit finishes a green-button exit *after*
    /// the last resize it generated — so the frame that resize asked for still hides the chrome,
    /// and the window then goes idle with a toolbar that isn't drawn. Nothing is wrong with the
    /// state by then; it is simply the last frame that is stale.
    ///
    /// Run at the idle point, which the loop reaches once the transition's notifications have been
    /// delivered. It costs a bool compare on an iteration that was going to happen anyway, and
    /// asks for nothing when nothing moved — an idle window still costs ~0.
    pub fn sync_fullscreen(&mut self) {
        let now = self.fullscreen();
        if now != self.fullscreen_seen {
            self.fullscreen_seen = now;
            self.redraw();
        }
    }

    /// Flip in/out of borderless full-screen (toolbar button, F11, Esc, or middle-click).
    fn toggle_fullscreen(&mut self) {
        self.set_fullscreen(!self.fullscreen());
    }

    /// Enter (`on`) or leave borderless full-screen: winit strips the decorations and covers the
    /// monitor the window is on, and restores the prior placement (maximized state included) on
    /// exit. The chrome simply isn't drawn while full-screen (see [`crate::ui::build`]), and
    /// [`Self::image_rect`] hands the whole client to the image. No-op if already in the requested
    /// state.
    fn set_fullscreen(&mut self, on: bool) {
        if on == self.fullscreen() {
            return;
        }
        // winit records the new mode before it asks the OS to animate, so the resizes that follow
        // — and the frames drawn from them — already see full-screen. Nothing to set here.
        self.window
            .set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
        self.redraw();
    }

    /// The empty state: no image loaded and none loading. The UI draws a drop / double-click hint
    /// over the image region, and a double-click there opens the file picker. During a load we stay
    /// out of this state (the previous image, or the backdrop, keeps showing) so the hint never
    /// flashes over a file the user just opened.
    fn empty_view_active(&self) -> bool {
        self.surface.current_image().is_none() && !self.loading
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
        self.set_title(crate::product::NAME);
        self.reset_to_empty();
    }

    /// Keep the window wide enough that the toolbar can still lay out (the right group plus a
    /// collapsed "»"), and tall enough for the chrome plus a sliver of image. Re-applied whenever
    /// the metrics move (DPI, stylesheet).
    fn apply_min_size(&self) {
        let m = &self.metrics;
        let cw = (420.0 * m.scale) as u32;
        let ch = (m.toolbar_h + m.status_h + 80.0 * m.scale) as u32;
        self.window
            .set_min_inner_size(Some(PhysicalSize::new(cw, ch)));
    }

    /// Re-derive everything that comes out of the stylesheet, and repaint.
    ///
    /// The single path for "the app's *look* has to change": a DPI change (metrics and the icon
    /// raster move), a light/dark switch (colors move), and — in a debug build — an edit to
    /// `ui/theme.toml` (any of it can move). Cheap: the icon atlas is only re-rastered if its
    /// physical size actually changed, and everything else is a few dozen struct writes.
    pub fn restyle(&mut self) {
        self.metrics = Metrics::new(self.dpi);
        let (dark, scale) = (self.dark, self.metrics.scale);
        self.imgui
            .restyle(|style| crate::ui::theme::apply(style, dark, scale));
        self.imgui.refresh_icons();
        self.surface
            .set_clear(crate::ui::theme::view_clear_packed(self.dark));
        self.apply_min_size();
        self.redraw();
    }

    /// Persist the placement (best effort): the last normal position/size plus the maximized
    /// flag. If we are full-screen or minimized the tracked normal placement is what gets saved,
    /// so the next launch reopens at the pre-full-screen size. Never persists a minimized state.
    fn save_window_state(&self) {
        let pos = self
            .normal_pos
            .or_else(|| self.window.outer_position().ok())
            .unwrap_or(PhysicalPosition::new(0, 0));
        let size = self.normal_size.unwrap_or_else(|| self.window.inner_size());
        if size.width == 0 || size.height == 0 {
            return;
        }
        WindowState {
            x: pos.x,
            y: pos.y,
            width: size.width as i32,
            height: size.height as i32,
            maximized: self.maximized,
        }
        .save();
    }

    /// Whether the current placement is a *normal* one worth remembering.
    fn placement_is_normal(&self) -> bool {
        !self.fullscreen() && !self.window.is_maximized() && self.window.is_minimized() != Some(true)
    }

    // --- events ---------------------------------------------------------------------------------

    /// Route one window event: lifecycle first, then the input ownership gates, then dispatch.
    pub fn window_event(&mut self, event: &WindowEvent) {
        // Lifecycle: nothing below may intercept these.
        match event {
            WindowEvent::RedrawRequested => return self.redraw_requested(),
            WindowEvent::CloseRequested => {
                self.requests.close = true;
                return;
            }
            WindowEvent::Resized(size) => {
                self.surface.resize(size.width, size.height);
                self.redraw();
                let maximized = self.window.is_maximized();
                if self.placement_is_normal() {
                    self.normal_size = Some(*size);
                }
                if maximized != self.maximized && !self.fullscreen() {
                    // The maximize/restore button: persist as it changes, not just on close.
                    self.maximized = maximized;
                    self.save_window_state();
                }
                // ImGui needs the new display size too.
                self.imgui.handle_event(event);
                return;
            }
            WindowEvent::Moved(pos) => {
                if self.placement_is_normal() {
                    self.normal_pos = Some(*pos);
                }
                return;
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The OS resizes the window for the new DPI on its own; rescale the UI. ImGui 1.92
                // re-bakes glyphs lazily, so this is a style rescale plus one icon-atlas
                // re-raster — no font atlas to rebuild.
                self.dpi = ((scale_factor * 96.0).round() as u32).max(96);
                // The swapchain has to follow the display's backing scale, not just the client
                // size the `Resized` after this carries; see the backend's `set_scale_factor`.
                self.surface.set_scale_factor(*scale_factor);
                self.imgui.set_dpi(self.dpi);
                self.imgui.handle_event(event);
                self.restyle();
                return;
            }
            WindowEvent::ThemeChanged(theme) => {
                // A light/dark switch — the only theme input the app takes from the system: every
                // color, accent included, is the stylesheet's. This re-picks the palette.
                self.dark = *theme == Theme::Dark;
                self.restyle();
                return;
            }
            WindowEvent::DroppedFile(path) => {
                // The viewer shows one image at a time; extra dropped files arrive as further
                // events and simply replace it.
                self.open(OpenRequest::new(path.clone()));
                return;
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                self.imgui.handle_event(event);
                return;
            }
            WindowEvent::Destroyed => return,
            _ => {}
        }

        // A keybind row on the settings tab is armed: this press *is* the binding. Take it before
        // ImGui sees it — Esc has to reach the capture (where it cancels), and ImGui would read it
        // as "close the modal" instead.
        let key_press = match event {
            WindowEvent::KeyboardInput { event: k, .. } if k.state == ElementState::Pressed => {
                match k.physical_key {
                    PhysicalKey::Code(code) => Some(code),
                    PhysicalKey::Unidentified(_) => None,
                }
            }
            _ => None,
        };
        if let Some(code) = key_press {
            if self.settings_capturing() {
                self.settings_capture(code);
                self.request_frames(2);
                return;
            }
        }

        // ImGui sees every remaining event first, so it can update its input state. Then two
        // booleans decide who owns the event: `wants_mouse` (the pointer is over a widget) and
        // `wants_keyboard` (a text field has focus, so keys are typing, not commands). That
        // *replaces* the entire hand-rolled hover/capture/hit-test/focus layer.
        self.imgui.handle_event(event);

        let mouse_msg = matches!(
            event,
            WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::PinchGesture { .. }
        );
        // Key-ups and IME text matter for the settle frames even though nothing below dispatches
        // them: without them, typing into a text field wouldn't repaint it.
        let input_msg = matches!(
            event,
            WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_)
        );
        // Any input can change a hover or an active state, so give ImGui its settle frames. This
        // runs *before* the ownership gates below, so an event they swallow still gets its frames.
        if mouse_msg || input_msg {
            self.request_frames(2);
        }

        // A pan/zoom drag already in flight owns the mouse to the end of the gesture, even if the
        // cursor strays over the chrome — otherwise the drag would stick the moment it crossed the
        // toolbar.
        if mouse_msg && !self.surface.is_mouse_captured() && self.imgui.wants_mouse() {
            return;
        }
        if let Some(code) = key_press {
            // The settings window is modal: while it is up, keys belong to it, not to the viewer —
            // a stray `F` must not re-fit the image behind it.
            //
            // Esc and Enter we handle ourselves. ImGui's nav deliberately does *not* close a modal
            // on Escape, and a dialog you can't escape is a trap. But while a **text field** is
            // being edited those two keys are the field's (Esc reverts it, Enter commits it) —
            // ImGui has already seen them above — so we stay out of the way, and a second press,
            // once the field has let go, reaches us.
            if self.settings.is_some() {
                if !self.imgui.wants_text_input() {
                    self.settings_key(code);
                }
                return;
            }
            // A popup menu is up. It isn't modal, so — unlike the settings window — ImGui leaves
            // `want_capture_keyboard` false and every key would fall straight through to the
            // viewer: Esc would *close the window* out from under the open menu. So the menu takes
            // the keys, and Esc dismisses it (which ImGui also does itself; doing it here as well
            // is harmless and is the part that doesn't depend on a default we don't own).
            if self.menu.is_some() {
                if code == KeyCode::Escape {
                    self.menu = None;
                    self.redraw();
                }
                return;
            }
            if self.imgui.wants_keyboard() {
                return;
            }
        }

        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                let p = self.image_cursor();
                self.surface.on_cursor_moved(p);
                if self.surface.is_zoom_dragging() {
                    self.redraw(); // the RMB drag changes the zoom %, which the status bar shows
                }
            }
            WindowEvent::MouseInput { state, button, .. } => self.on_mouse_button(*state, *button),
            WindowEvent::MouseWheel { delta, .. } => {
                // One notch = one unit; a precision touchpad reports fractions of it.
                let notches = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 120.0) as f32,
                };
                // Ctrl+wheel is the near-universal zoom gesture, so it zooms whatever the wheel is
                // configured for — which is also what keeps wheel zoom reachable for someone who
                // put folder navigation on the plain turn.
                let action = if self.mods.control_key() {
                    WheelActionCfg::Zoom
                } else {
                    self.cfg.wheel_action
                };
                match action {
                    WheelActionCfg::Zoom if notches != 0.0 => {
                        let step = self.cfg.zoom_step;
                        self.surface.zoom_at_cursor(step.powf(notches));
                        self.redraw();
                    }
                    WheelActionCfg::NavigateFolder => self.wheel_navigate(notches),
                    WheelActionCfg::Zoom => {}
                }
            }
            // Trackpad pinch (macOS): the same about-cursor zoom the wheel drives, so the
            // zoom-snap ladder and the cursor anchoring are shared rather than reimplemented
            // (D15). `delta` is an incremental magnification — AppKit's `NSEvent.magnification`
            // — so the factor is `1 + delta`, not the delta itself. winit documents it as
            // possibly NaN; a NaN reaching the zoom would poison it for the rest of the session
            // with no way back, so it is filtered here rather than deep in the view math.
            WindowEvent::PinchGesture { delta, .. } => {
                let factor = 1.0 + *delta as f32;
                if factor.is_finite() && factor > 0.0 {
                    self.surface.zoom_at_cursor(factor);
                    self.redraw();
                }
            }
            WindowEvent::KeyboardInput { .. } => {
                if let Some(code) = key_press {
                    self.handle_key(code);
                }
            }
            _ => {}
        }
    }

    /// Image mouse buttons. Only reached when the routing decided ImGui didn't want the event, so
    /// everything here acts on the image itself.
    fn on_mouse_button(&mut self, state: ElementState, button: MouseButton) {
        match (button, state) {
            (MouseButton::Left, ElementState::Pressed) => {
                // A second press right after the first is a double-click: over the empty viewport
                // it opens the file picker (matching the on-screen hint).
                let now = Instant::now();
                let slop = DOUBLE_CLICK_SLOP * self.metrics.scale;
                let double = self.last_click.is_some_and(|(t, p)| {
                    now.duration_since(t) <= DOUBLE_CLICK
                        && (p.0 - self.cursor.0).abs() <= slop
                        && (p.1 - self.cursor.1).abs() <= slop
                });
                self.last_click = if double {
                    None
                } else {
                    Some((now, self.cursor))
                };
                if double {
                    if self.empty_view_active() {
                        let (x, y) = self.cursor;
                        let (ix, iy, iw, ih) = self.image_rect();
                        if x >= ix && x < ix + iw && y >= iy && y < iy + ih {
                            self.open_via_dialog();
                        }
                    }
                    return;
                }
                // Sync the pan origin to the press point so the first move's delta is measured
                // from here, not a stale position. Matters after the context menu (or any gap
                // where we saw no move): without this the first drag lurches the image toward the
                // click. (winit captures the mouse for the duration of the press on its own.)
                let p = self.image_cursor();
                self.surface.on_cursor_moved(p);
                self.surface.begin_drag();
            }
            (MouseButton::Left, ElementState::Released) => self.surface.end_drag(),
            (MouseButton::Right, ElementState::Pressed) => {
                let p = self.image_cursor();
                self.surface.on_cursor_moved(p); // pin the pivot to the press point
                self.surface.begin_zoom_drag();
            }
            (MouseButton::Right, ElementState::Released) => {
                // A right *click* (the gesture never moved past the zoom-drag slop) opens the
                // actions menu at the cursor; an actual zoom-drag just ends.
                if !self.surface.end_zoom_drag() {
                    self.open_menu(crate::ui::MenuKind::Actions, self.cursor);
                }
            }
            // A middle-click over the image toggles full-screen.
            (MouseButton::Middle, ElementState::Pressed) => self.toggle_fullscreen(),
            _ => {}
        }
    }

    /// Draw one frame and settle the repaint debt.
    fn redraw_requested(&mut self) {
        // Bring playback up to *now* before drawing, so this frame shows the cell/GIF frame that
        // belongs to this instant whatever asked for it — a hover, a resize, a drag. The advance
        // dirties the window, and this is the repaint that clears it, so it costs no extra frame.
        self.advance_playback();
        // The event-driven pump: draw the frame we were asked for, and stop when the debt is
        // paid. If `frames_wanted` is still positive afterwards, ask for exactly one more — never
        // a self-sustaining loop.
        self.frames_wanted = self.frames_wanted.saturating_sub(1);
        self.render();
        if self.frames_wanted > 0 {
            self.window.request_redraw();
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
        use std::fmt::Write as _;
        let mut status_right = String::new();
        if let Some(f) = &self.folder {
            let _ = write!(status_right, "{} / {}", f.position(), f.len());
        }
        // Which mip level is showing, and how big it is. Only when there is a chain to walk.
        // The dimensions are the level's own, because fit, zoom and 1:1 now describe the level
        // rather than the file's level 0.
        if has_image && s.mip_count() > 1 {
            if !status_right.is_empty() {
                status_right.push_str("    ");
            }
            let (fw, fh) = s.current_image().map_or((1, 1), |i| (i.width, i.height));
            let (lw, lh) = crate::render::mips::level_dims(fw, fh, s.mip_level());
            let _ = write!(
                status_right,
                "mip {}/{}  {lw}×{lh}",
                s.mip_level(),
                s.mip_count() - 1
            );
        }
        if has_image {
            if !status_right.is_empty() {
                status_right.push_str("    ");
            }
            if is_hdr {
                let _ = write!(status_right, "EV {:+.2}    {}%", s.exposure(), zoom_pct);
            } else {
                let _ = write!(status_right, "{zoom_pct}%");
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
            fullscreen: self.fullscreen(),
            flipbook: self.flipbook_state().is_some(),
            has_animation: self.surface.frame_delay_ms().is_some(),
            mip_level: s.mip_level(),
            mip_count: s.mip_count(),
            shortcuts: Arc::clone(&self.shortcut_labels),
            status_left,
            status_right,
        }
    }

    /// Draw one frame: clear, the image into its sub-rect, then the whole UI over it, then present.
    fn render(&mut self) {
        // The chrome fill, so the parts of the frame the image doesn't cover start from a known
        // color rather than last frame's garbage.
        let bg = crate::ui::theme::chrome_bg(self.dark);
        self.surface.set_chrome_clear(bg);

        // Recomputed every frame — the transport band appearing, a resize, and a DPI change all just
        // fall out of this. Nothing to keep in sync.
        let (ix, iy, iw, ih) = self.image_rect();
        self.surface.set_image_rect(ix, iy, iw, ih);

        let snap = self.snapshot();
        let transport = self.transport_snapshot();
        let chip = self.chip_hint();
        let (cw, ch) = self.client();
        let metrics = self.metrics;
        let dark = self.dark;
        let fullscreen = self.fullscreen();
        let icon_px = self.imgui.icon_px();
        let form = self.imgui.form_style(dark);
        // Only an empty-state frame asks for the logo — an image launch never builds it, which
        // keeps the upload off the time-to-first-photon path (see Imgui::logo).
        let logo = if snap.has_image || snap.loading {
            dear_imgui_rs::TextureId::new(0)
        } else {
            self.imgui.logo()
        };

        // The settings and menu state are *edited* by the UI, so they go in by `&mut`. Move them out
        // for the duration rather than borrow fields of `self` across the frame.
        let mut settings = self.settings.take();
        let mut menu = self.menu.take();
        // Borrowed, not cloned: `Config` owns the open-with tree and the keybind map, and a frame is
        // drawn on every mouse move.
        let cfg = &self.cfg;
        let imgui = &mut self.imgui;
        let mut frame = None;
        let presented = self.surface.render_frame(|| {
            frame = imgui.frame(|ui, tex| {
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
                        logo,
                        dark,
                        client: (cw as f32, ch as f32),
                        image: (ix, iy, iw, ih),
                        fullscreen,
                    },
                )
            });
        });
        self.settings = settings;
        self.menu = menu;

        // **Playback is paced by the present, not by a timer.** Acquiring the frame blocked until
        // the display had taken the previous one, so asking for another here paces the next
        // exactly one refresh later — 120 Hz on a 120 Hz panel, whatever the sheet's fps — and
        // `advance_flipbook` samples the position once per refresh, evenly. That even sampling is
        // the whole point: the motion the eye follows is the transport bar's, and a timer cannot
        // clock it finely enough (see [`FLIPBOOK_TICK_MS`]).
        //
        // It terminates: at most one frame is owed at a time, and it is only asked for while
        // something is actually playing — a paused flipbook or a still image is back to costing
        // nothing. If the acquire *didn't* wait (an occluded window, or a not-yet-full swapchain),
        // pacing on it would spin, so we don't, and the timer carries playback until it does.
        if let Presented::Yes { waited: true } = presented {
            if self.flipbook_playing() {
                self.request_frames(1);
            }
        }

        self.sync_caret_timer();
        if let Some(frame) = frame {
            self.apply_ui(frame);
        }
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
        // The popup menus. Nothing is deferred: an ImGui popup pumps no messages, so a toolbar
        // button can simply ask for one and a chosen command can simply run.
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
        // "Browse…" opens the native file dialog, which pumps its own modal loop and so must not
        // be entered from inside this redraw — it runs from the loop's idle step.
        if frame.settings_browse {
            self.dialog = Some(Dialog::BrowseProgram);
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // Remember where/how the window was before it goes away, to restore next launch.
        self.save_window_state();
        self.persist_octagon();
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

/// The window/taskbar icon: the logo raster build.rs already embeds for the empty-window card, so
/// no OS icon resource is needed and every OS shows the same flame. `None` if winit rejects it
/// (it cannot: the raster is validated at build time), in which case the OS default shows.
fn window_icon() -> Option<winit::window::Icon> {
    use crate::render::imgui::{LOGO_EDGE, LOGO_RGBA};
    winit::window::Icon::from_rgba(LOGO_RGBA.to_vec(), LOGO_EDGE, LOGO_EDGE).ok()
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

fn file_name(path: &Path) -> String {
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
    use std::fmt::Write as _;
    if let Some(bytes) = file_size {
        let _ = write!(s, "   {}", human_size(bytes));
    }
    if img.icc.is_some() {
        s.push_str("   ICC");
    }
    if let Some((ow, oh)) = img.downscaled_from {
        let _ = write!(s, "   (from {ow}×{oh})");
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
