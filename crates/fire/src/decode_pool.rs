//! Off-thread decode worker pool (Option A from the plan — no async runtime).
//!
//! Decoding a large PSD/EXR can take tens of milliseconds; doing it on the UI thread would
//! freeze the window between the click and the image. Instead, `open` shows the window with a
//! placeholder immediately and hands a [`DecodeJob`] to this pool. A worker decodes on a
//! background thread and sends the result back to the UI thread as an [`AppEvent::DecodeDone`]
//! through the event loop's proxy (workers never touch a window or the renderer — same
//! discipline as the instance-socket server thread).
//!
//! Each job carries a process-wide monotonic `generation` (see [`fresh_generation`]) plus the
//! window it was issued for; the UI uploads a result only if it is still that window's latest
//! generation, so a slow decode can never clobber a newer one (stale-drop). A superseded job is
//! still decoded — its result is just dropped on arrival — which wastes a little work but keeps
//! the pool dead simple.
//!
//! The pool is shared by every window in the process (there is one process — see `main.rs`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{unbounded, Sender};
use fire_decode::{decode_path, DecodeError, DecodeOptions, DecodedImage};
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use crate::app::AppEvent;

/// Plan-adopted default pool size: `min(num_cpus, 4)`.
const MAX_WORKERS: usize = 4;

/// The process-wide decode generation counter. Every open, navigate, reload and close takes a
/// fresh value, and every cross-thread result carries the one it was issued under; a result whose
/// generation is no longer its window's current one is stale and dropped. Process-wide rather than
/// per-window so the pool's skip check below needs no per-window bookkeeping.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The next decode generation. Strictly increasing for the life of the process.
pub fn fresh_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed) + 1
}

/// A unit of decode work handed to a worker thread.
#[derive(Debug)]
pub struct DecodeJob {
    /// The window the result is for. `None` for the launch path's decode, which is submitted
    /// before any window exists and lands in the first one created.
    pub window: Option<WindowId>,
    /// The issuing window's generation at submit time; used for stale-drop.
    pub generation: u64,
    pub path: PathBuf,
    pub opts: DecodeOptions,
    /// True if this is a hot-reload of the displayed file (vs. a fresh open/navigate). The UI
    /// uses it to keep the current view when the re-decoded image has the same dimensions.
    pub reload: bool,
    /// Whether to run sprite-sheet auto-detection after posting the image (the
    /// `flipbook.auto-detect` config key). False skips the per-pixel scan entirely — no
    /// [`AppEvent::FlipbookGuess`] is sent, so no hint chip can appear.
    pub detect_flipbook: bool,
}

/// A finished decode, delivered back to the UI thread. The image is `Arc`-wrapped so the worker
/// can keep a clone and run flipbook detection *after* posting this (detection stays off the
/// time-to-first-pixel path); the UI stores its clone in the surface.
pub struct DecodeOutcome {
    pub window: Option<WindowId>,
    pub generation: u64,
    pub path: PathBuf,
    pub result: Result<Arc<DecodedImage>, DecodeError>,
    /// Echoed from the job; see [`DecodeJob::reload`].
    pub reload: bool,
}

/// The flipbook auto-detection result for a decoded image, delivered to the UI thread *after* the
/// image itself (a separate [`AppEvent::FlipbookGuess`]) so the analysis — which for a large sheet
/// scans every pixel — never delays the image reaching the screen. `guess` is the detected grid,
/// or `None` for a non-sheet image. Stale-dropped by `generation` like a decode.
pub struct FlipbookGuess {
    pub window: Option<WindowId>,
    pub generation: u64,
    pub path: PathBuf,
    pub guess: Option<crate::flipbook::Grid>,
}

/// Sender handle to the worker pool. Cheap to clone; every window holds one.
#[derive(Clone)]
pub struct DecodePool {
    tx: Sender<DecodeJob>,
    /// The newest generation ever submitted. Workers consult it before decoding: the queue is
    /// unbounded, so key-repeat navigation can enqueue jobs faster than they retire, and every
    /// superseded job decoded in full is a wasted allocation (up to ~1 GiB at `MAX_CPU_DIM`)
    /// parked in the event queue until the UI thread drains it.
    latest: Arc<AtomicU64>,
}

impl DecodePool {
    /// Spawn the worker threads. Each sends its results to the event loop through `proxy`.
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> Self {
        let (tx, rx) = unbounded::<DecodeJob>();
        let latest = Arc::new(AtomicU64::new(0));
        let workers = worker_count();
        let mut started = 0usize;
        for i in 0..workers {
            let rx = rx.clone();
            let latest = Arc::clone(&latest);
            let proxy = proxy.clone();
            let spawned = thread::Builder::new()
                .name(format!("fire-decode-{i}"))
                .spawn(move || {
                    // Exits when the pool (and thus every `tx`) is dropped at shutdown.
                    while let Ok(job) = rx.recv() {
                        // A superseded job's result would be stale-dropped on arrival anyway;
                        // once a newer submit exists, skip the decode itself.
                        if job.generation < latest.load(Ordering::Relaxed) {
                            continue;
                        }
                        let result = decode(&job).map(Arc::new);
                        // Keep a clone to run flipbook detection *after* the image is posted, so a
                        // large sheet reaches the screen without waiting on the per-pixel scan.
                        // Skipped for animated sources (a GIF is not a sprite sheet), and when the
                        // user has turned auto-detection off.
                        let detect_input = result
                            .as_ref()
                            .ok()
                            .filter(|img| job.detect_flipbook && img.animation.is_none())
                            .map(Arc::clone);
                        let generation = job.generation;
                        let window = job.window;
                        let path = job.path.clone();

                        // --- Send the decoded image immediately (time-to-first-pixel path) ---
                        let outcome = Box::new(DecodeOutcome {
                            window,
                            generation,
                            path: job.path,
                            result,
                            reload: job.reload,
                        });
                        // The only way a send fails is a closed event loop: the app is exiting.
                        if proxy.send_event(AppEvent::DecodeDone(outcome)).is_err() {
                            break;
                        }

                        // --- Then detect the flipbook grid off the critical path and send the hint
                        // separately. Detection never touches a window/renderer and is never
                        // allowed to kill the worker (a malformed sheet mustn't take the pool down).
                        if let Some(img) = detect_input {
                            let guess =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    crate::flipbook::detect(&path, &img)
                                }))
                                .unwrap_or(None);
                            let hint = Box::new(FlipbookGuess {
                                window,
                                generation,
                                path,
                                guess,
                            });
                            if proxy.send_event(AppEvent::FlipbookGuess(hint)).is_err() {
                                break;
                            }
                        }
                    }
                });
            // A thread the OS refused to start must not abort the viewer before its window
            // exists (the folder-scan and watcher threads already degrade this way); any
            // workers that did start carry the load.
            match spawned {
                Ok(_) => started += 1,
                Err(e) => eprintln!("fire: could not start decode worker {i}: {e}"),
            }
        }
        if started == 0 {
            eprintln!("fire: no decode workers could be started; images will not decode");
        }
        Self { tx, latest }
    }

    /// Enqueue a decode. The unbounded channel only fails to send once every worker
    /// has exited (shutdown), so a dropped job here is benign.
    pub fn submit(&self, job: DecodeJob) {
        // fetch_max, not store: submit order and the generation counter agree today, but the
        // skip must never move backwards if they ever don't.
        self.latest.fetch_max(job.generation, Ordering::Relaxed);
        let _ = self.tx.send(job);
    }
}

/// Decode one job, converting any panic into a `DecodeError` so a worker thread is
/// never lost. The decode crate already wraps its C/C++ FFI in `catch_unwind`; this is
/// a belt-and-suspenders boundary around the whole pure-Rust + FFI path.
fn decode(job: &DecodeJob) -> Result<DecodedImage, DecodeError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_path(&job.path, &job.opts)
    }))
    .unwrap_or_else(|_| Err(DecodeError::Other("decoder panicked".into())))
}

fn worker_count() -> usize {
    thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, MAX_WORKERS)
}
