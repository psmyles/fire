//! The shell: one process, one winit event loop, N viewer windows.
//!
//! [`Fire`] is the `ApplicationHandler`. It owns the process-wide pieces — the GPU device (built
//! with the first window), the decode pool, the timer queue, the stylesheet watcher — and a map of
//! [`Viewer`]s keyed by window. Every event is routed to its window's viewer; every cross-thread
//! message (a decode landing, a forwarded open, a file change) arrives as an [`AppEvent`] through
//! the loop's proxy and is routed the same way.
//!
//! **Rendering stays event-driven.** A frame is drawn only on `RedrawRequested`, which only follows
//! input, a decode landing, or a timer; timers are deadlines the loop sleeps on
//! (`ControlFlow::WaitUntil`, see [`timers`]), never threads. No input, no timer, no event → no
//! frame → an idle window costs ~0.
//!
//! **Every handler runs behind a panic firewall.** A panic in a handler must not unwind into
//! winit's dispatcher — the Win32 shell's wndproc had the same rule — so each entry point catches,
//! logs, and carries on with the window alive.

pub mod timers;
pub mod viewer;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use fire_ipc::OpenRequest;
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::WindowId;

use crate::config::{Config, OpenIn};
use crate::decode_pool::{DecodeOutcome, DecodePool, FlipbookGuess};
use crate::render::gpu::Gpu;
use timers::{TimerQueue, Timers};
use viewer::{Dialog, Viewer};

pub use viewer::MAX_CPU_DIM;

/// A message from another thread (or from the instance socket) for the event loop.
pub enum AppEvent {
    /// An open request forwarded by another launch over the instance socket.
    Open(OpenRequest),
    /// A finished decode from a worker.
    DecodeDone(Box<DecodeOutcome>),
    /// A finished flipbook auto-detection from a worker, sent *after* its `DecodeDone` so the
    /// per-pixel scan never delays the image.
    FlipbookGuess(Box<FlipbookGuess>),
    /// A finished sibling-image scan from the folder-scan thread.
    FolderScanned(WindowId, Box<FolderScan>),
    /// The displayed image's file changed on disk (hot-reload). `generation` is the watcher's
    /// generation at arm time (stale-drop); no payload — the viewer re-decodes its own current path.
    FileChanged { window: WindowId, generation: u64 },
    /// `ui/theme.toml` changed on disk and the new stylesheet is already live (see
    /// [`crate::hotstyle`], debug builds only). Every window re-derives what the stylesheet feeds.
    /// Handled unconditionally so the reload path is the same code a release build would run;
    /// only the *watcher* that sends it is debug-gated.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    ThemeReloaded,
    /// A native file dialog finished (or was cancelled). It runs on a worker thread — see
    /// [`viewer::Dialog`] for why it cannot run inside a handler — so its answer comes back
    /// through the loop like any other cross-thread result.
    DialogDone {
        window: WindowId,
        dialog: Dialog,
        path: Option<PathBuf>,
    },
    /// A macOS menu-bar item that Fire performs itself (D16). It carries a [`KeyAction`] rather
    /// than a command of its own so the menu and the keyboard cannot drift apart: both end in
    /// `Viewer::perform_key_action`.
    #[cfg(target_os = "macos")]
    MenuCommand(crate::keybinds::KeyAction),
}

impl std::fmt::Debug for AppEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppEvent::Open(r) => write!(f, "Open({})", r.path.display()),
            AppEvent::DecodeDone(o) => write!(f, "DecodeDone({})", o.path.display()),
            AppEvent::FlipbookGuess(g) => write!(f, "FlipbookGuess({})", g.path.display()),
            AppEvent::FolderScanned(w, s) => {
                write!(f, "FolderScanned({w:?}, {} entries)", s.entries.len())
            }
            AppEvent::FileChanged { window, generation } => {
                write!(f, "FileChanged({window:?}, gen {generation})")
            }
            AppEvent::DialogDone {
                window,
                dialog,
                path,
            } => match path {
                Some(p) => write!(f, "DialogDone({window:?}, {dialog:?}, {})", p.display()),
                None => write!(f, "DialogDone({window:?}, {dialog:?}, cancelled)"),
            },
            AppEvent::ThemeReloaded => write!(f, "ThemeReloaded"),
            #[cfg(target_os = "macos")]
            AppEvent::MenuCommand(a) => write!(f, "MenuCommand({})", a.name()),
        }
    }
}

/// A completed sibling-image scan. The viewer turns it into a folder cursor once it confirms it's
/// still current.
pub struct FolderScan {
    /// The image whose folder was scanned. Both the cursor's starting index *and* the staleness
    /// check: see `Viewer::folder_scanned` for why this, rather than a decode generation.
    pub path: PathBuf,
    /// Sorted sibling image paths in the folder.
    pub entries: Vec<PathBuf>,
}

/// The launch path, whose decode was submitted in `main` *before* the event loop and the window
/// existed — so the file is being read while the window and the GPU come up. The first window
/// adopts it: the decode lands there.
pub struct Initial {
    pub path: PathBuf,
    pub generation: u64,
}

/// The application: the event-loop handler and everything shared between windows.
pub struct Fire {
    /// A fatal error to report once the event loop has returned; see `create_viewer`.
    fatal: Option<String>,
    cfg: Config,
    proxy: EventLoopProxy<AppEvent>,
    pool: DecodePool,
    timers: Timers,
    /// The one GPU, once the first window has joined its bring-up thread.
    gpu: Option<Rc<Gpu>>,
    /// The bring-up thread started at the top of `main`, joined by the first window.
    gpu_pending: Option<std::thread::JoinHandle<Result<Gpu, String>>>,
    viewers: HashMap<WindowId, Viewer>,
    /// Windows in creation order — the first is where a pre-window decode lands, the last is the
    /// fallback target for a forwarded open when no window has focus.
    order: Vec<WindowId>,
    focused: Option<WindowId>,
    initial: Option<Initial>,
    started: bool,
    /// Stylesheet hot-reload guard (debug builds only); dropping it ends that watch thread.
    #[cfg(debug_assertions)]
    _hotstyle: Option<crate::hotstyle::HotStyle>,
}

impl Fire {
    pub fn new(
        cfg: Config,
        proxy: EventLoopProxy<AppEvent>,
        pool: DecodePool,
        initial: Option<Initial>,
        gpu_pending: std::thread::JoinHandle<Result<Gpu, String>>,
    ) -> Self {
        // Debug only: watch `ui/theme.toml` in the source tree, so editing the stylesheet
        // restyles every window without a rebuild. Compiled out of release.
        #[cfg(debug_assertions)]
        let hotstyle = crate::hotstyle::spawn(proxy.clone());
        Fire {
            fatal: None,
            cfg,
            proxy,
            pool,
            timers: Rc::new(std::cell::RefCell::new(TimerQueue::default())),
            gpu: None,
            gpu_pending: Some(gpu_pending),
            viewers: HashMap::new(),
            order: Vec::new(),
            focused: None,
            initial,
            started: false,
            #[cfg(debug_assertions)]
            _hotstyle: hotstyle,
        }
    }

    /// A fatal error the loop could not report itself, for `main` to show after `run_app`.
    pub fn take_fatal(&mut self) -> Option<String> {
        self.fatal.take()
    }

    /// The GPU: the bring-up thread's result the first time, then the shared handle.
    fn gpu(&mut self) -> Result<Rc<Gpu>, String> {
        if let Some(g) = &self.gpu {
            return Ok(Rc::clone(g));
        }
        let t = Instant::now();
        let handle = self
            .gpu_pending
            .take()
            .ok_or("the GPU failed to initialize earlier")?;
        let gpu = handle
            .join()
            .map_err(|_| "the GPU bring-up thread panicked".to_string())??;
        crate::render::gpu::report_timing(&format!(
            "gpu join wait — {:.2} ms",
            t.elapsed().as_secs_f64() * 1e3
        ));
        let gpu = Rc::new(gpu);
        self.gpu = Some(Rc::clone(&gpu));
        Ok(gpu)
    }

    /// Open a new viewer window. `initial` is the launch path whose decode is already in flight;
    /// `open` is a request to decode now. On failure with no window up at all, report and exit —
    /// there is nothing to show.
    fn create_viewer(
        &mut self,
        el: &ActiveEventLoop,
        initial: Option<Initial>,
        open: Option<OpenRequest>,
    ) {
        // Everything the viewer needs, gathered before the call: the GPU is handed over as a
        // closure so the window comes up *alongside* the bring-up thread and the join happens
        // only once there is a window to draw into (see `Viewer::new`).
        let (cfg, pool, timers, proxy) = (
            self.cfg.clone(),
            self.pool.clone(),
            Rc::clone(&self.timers),
            self.proxy.clone(),
        );
        match Viewer::new(el, || self.gpu(), cfg, pool, timers, proxy) {
            Ok(mut viewer) => {
                if let Some(init) = initial {
                    viewer.adopt_initial(init);
                }
                if let Some(req) = open {
                    viewer.open(req);
                }
                let id = viewer.id();
                viewer.show();
                self.order.push(id);
                self.viewers.insert(id, viewer);
            }
            Err(e) => {
                eprintln!("fire: could not open a window: {e}");
                if self.viewers.is_empty() {
                    // The release build has no console, so stderr goes nowhere and a silent exit
                    // would be indistinguishable from a crash: a dialog is the only channel the
                    // user actually sees at this point. It is *recorded* rather than shown here,
                    // and `main` puts it up once the loop has returned — a message box is modal
                    // too, and a modal loop opened from inside a handler is the crash `Dialog`
                    // describes. There is no window left to be modal to anyway.
                    self.fatal = Some(format!(
                        "{} could not open a window.\n\n{e}",
                        crate::product::NAME
                    ));
                    el.exit();
                }
            }
        }
    }

    /// The window a forwarded open lands in under `reuse-window`: the focused one, else the most
    /// recently created.
    fn reuse_target(&self) -> Option<WindowId> {
        self.focused
            .filter(|id| self.viewers.contains_key(id))
            .or_else(|| self.order.last().copied())
    }

    /// The window a message for `window` is delivered to: that window, or — for the launch
    /// path's decode, issued before any window existed — the first one.
    fn target(&mut self, window: Option<WindowId>) -> Option<&mut Viewer> {
        let id = window.or_else(|| self.order.first().copied())?;
        self.viewers.get_mut(&id)
    }

    fn handle_user_event(&mut self, el: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Open(req) => match self.cfg.open_in {
                OpenIn::NewWindow => self.create_viewer(el, None, Some(req)),
                OpenIn::ReuseWindow => match self.reuse_target() {
                    Some(id) => {
                        if let Some(v) = self.viewers.get_mut(&id) {
                            v.open(req);
                        }
                    }
                    None => self.create_viewer(el, None, Some(req)),
                },
            },
            // The menu bar acts on the focused window, the same one a keystroke would have gone
            // to. With no window there is nothing to act on: the item is a no-op rather than a
            // reason to make one.
            #[cfg(target_os = "macos")]
            AppEvent::MenuCommand(action) => {
                if let Some(id) = self.reuse_target() {
                    if let Some(v) = self.viewers.get_mut(&id) {
                        v.perform_key_action(action);
                    }
                }
            }
            AppEvent::DecodeDone(outcome) => {
                if let Some(v) = self.target(outcome.window) {
                    v.decode_done(*outcome);
                }
            }
            AppEvent::FlipbookGuess(guess) => {
                if let Some(v) = self.target(guess.window) {
                    v.flipbook_guess_done(*guess);
                }
            }
            AppEvent::FolderScanned(id, scan) => {
                if let Some(v) = self.viewers.get_mut(&id) {
                    v.folder_scanned(*scan);
                }
            }
            AppEvent::FileChanged { window, generation } => {
                if let Some(v) = self.viewers.get_mut(&window) {
                    v.reload(generation);
                }
            }
            AppEvent::DialogDone {
                window,
                dialog,
                path,
            } => {
                // The window may have closed while the picker was up; then there is nothing to
                // apply the answer to.
                if let Some(v) = self.viewers.get_mut(&window) {
                    v.dialog_done(dialog, path);
                }
            }
            AppEvent::ThemeReloaded => {
                for v in self.viewers.values_mut() {
                    v.restyle();
                }
            }
        }
    }

    /// What a viewer asked the shell for during the event just dispatched to it: closing itself,
    /// a settings change every other window should adopt too.
    fn after_dispatch(&mut self, el: &ActiveEventLoop, id: WindowId) {
        let Some(v) = self.viewers.get_mut(&id) else {
            return;
        };
        let requests = v.take_requests();
        if let Some(cfg) = requests.settings_applied {
            // Settings are per-user, not per-window: the window that applied them already has
            // them; every other window adopts the same config (and nobody re-saves it).
            for (other, viewer) in self.viewers.iter_mut() {
                if *other != id {
                    viewer.adopt_settings(cfg.clone());
                }
            }
            self.cfg = cfg;
        }
        if requests.close {
            self.close_window(el, id);
        }
    }

    fn close_window(&mut self, el: &ActiveEventLoop, id: WindowId) {
        // Dropping the viewer saves its placement, stops its watcher and releases its GPU and
        // ImGui objects.
        self.viewers.remove(&id);
        self.order.retain(|w| *w != id);
        if self.focused == Some(id) {
            self.focused = None;
        }
        if self.viewers.is_empty() {
            el.exit();
        }
    }

    /// Start any native dialog a viewer asked for. Cheap now — each one goes to its own thread
    /// and answers with [`AppEvent::DialogDone`] — but still done here at the idle point rather
    /// than mid-event, so a request made during a redraw does not put a picker up underneath it.
    fn run_dialogs(&mut self) {
        let pending: Vec<(WindowId, Dialog)> = self
            .viewers
            .iter_mut()
            .filter_map(|(id, v)| v.take_dialog().map(|d| (*id, d)))
            .collect();
        for (id, dialog) in pending {
            if let Some(v) = self.viewers.get_mut(&id) {
                v.run_dialog(dialog);
            }
        }
    }

    /// Fire every timer that has fallen due.
    fn dispatch_timers(&mut self) {
        let due = self.timers.borrow_mut().pop_due(Instant::now());
        for (id, kind, seq) in due {
            if let Some(v) = self.viewers.get_mut(&id) {
                v.on_timer(kind, seq);
            }
        }
    }

    /// Park the loop until the next deadline, or indefinitely if there is none. This is the whole
    /// of the event-driven invariant's enforcement: with no timer armed the process sleeps in the
    /// OS until an event arrives.
    fn set_wait(&self, el: &ActiveEventLoop) {
        match self.timers.borrow().next_deadline() {
            Some(at) => el.set_control_flow(ControlFlow::WaitUntil(at)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// The panic firewall around every handler: a panic must never unwind into winit's dispatcher.
/// On a caught panic we log and carry on — the window stays alive, exactly as the Win32 shell's
/// wndproc firewall behaved.
fn firewall(what: &str, f: impl FnOnce()) {
    if std::panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
        eprintln!("fire: recovered from a panic in {what}");
    }
}

impl ApplicationHandler<AppEvent> for Fire {
    fn new_events(&mut self, _el: &ActiveEventLoop, _cause: StartCause) {}

    fn resumed(&mut self, el: &ActiveEventLoop) {
        // winit calls this once at start on the desktop (and again after a suspend on mobile,
        // which fire never sees). Create the first window exactly once.
        if self.started {
            return;
        }
        self.started = true;
        // AppKit does a great deal between `run_app` and the first `resumed` — `finishLaunching`,
        // activation, the launch Apple event — and none of the phase timers below can see any of
        // it, because they all start inside the window creation this is about to do. Reported from
        // the process-creation clock so it lines up with the TTFP number rather than with an
        // `Instant` of its own.
        crate::render::gpu::report_timing(&format!(
            "process start → first resumed — {:.2} ms",
            crate::ttfp::ms_since_start()
        ));
        firewall("startup", || {
            let initial = self.initial.take();
            // On macOS a launch-by-open arrives as an Apple event *before* this point, not as an
            // argument (see `openfiles`), so the file to show may be waiting here rather than in
            // `initial`. Give it to the first window as it is created: opening blank and loading
            // a frame later would be a visible flash, and in `new-window` mode would strand an
            // empty window in front of the one holding the image.
            #[cfg(target_os = "macos")]
            let mut opens = crate::openfiles::start().into_iter();
            #[cfg(target_os = "macos")]
            let first = initial.is_none().then(|| opens.next()).flatten();
            #[cfg(not(target_os = "macos"))]
            let first = None;

            self.create_viewer(el, initial, first);

            // A multi-file open (several images dropped on the Dock icon at once) obeys the
            // `open-in` setting for the rest, exactly as forwarded launches do.
            #[cfg(target_os = "macos")]
            for req in opens {
                self.handle_user_event(el, AppEvent::Open(req));
            }
        });
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: AppEvent) {
        firewall("a cross-thread event", || self.handle_user_event(el, event));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        firewall("a window event", || {
            if let WindowEvent::Focused(true) = event {
                self.focused = Some(id);
            }
            if let Some(v) = self.viewers.get_mut(&id) {
                v.window_event(&event);
            }
            self.after_dispatch(el, id);
        });
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        firewall("the idle step", || {
            self.run_dialogs();
            self.dispatch_timers();
            // A dialog may have asked its window to close (Esc after Open… cancels nothing; but
            // a settings Browse… can land while a close is pending) — drain requests again.
            let ids: Vec<WindowId> = self.order.clone();
            for id in ids {
                self.after_dispatch(el, id);
            }
            self.set_wait(el);
        });
    }
}
