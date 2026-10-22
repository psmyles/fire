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
//! generation, so a slow decode can never clobber a newer one (stale-drop). A job that is already
//! superseded when a worker picks it up is skipped rather than decoded (see `DecodePool::latest`).
//!
//! **Two lanes.** The main lane (`fire-decode-*`) runs every file decode, and a PSD layer the
//! user is waiting on. The prefetch lane (`fire-layer-*`, [`PREFETCH_WORKERS`] threads at reduced
//! OS priority) runs only layers decoded *ahead* of the user, each on a single thread — so
//! pre-decoding can never take a main worker, and never delays the next file's decode. A third
//! thread, the reaper, frees what the UI thread lets go of (a session's file bytes, evicted
//! layers: hundreds of megabytes) so those frees never stall a frame.
//!
//! The pool is shared by every window in the process (there is one process — see `main.rs`).

use std::any::Any;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::{unbounded, Receiver, Sender};
use fire_decode::{DecodeError, DecodeOptions, DecodedImage, LayerCtl, LayerError, PsdLayers};
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use crate::app::AppEvent;
use crate::layers::{JobCtl, SessionCtl};

/// Plan-adopted default pool size: `min(num_cpus, 4)`.
const MAX_WORKERS: usize = 4;

/// Threads in the PSD layer prefetch lane.
const PREFETCH_WORKERS: usize = 2;

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
    /// The issuing window's live generation, when it has one (the launch decode does not): read
    /// after the image decodes, so the PSD layer index is not built (and the file's bytes not
    /// kept) for a window that has already moved on.
    pub gen_cell: Option<Arc<AtomicU64>>,
}

/// A finished decode, delivered back to the UI thread. The image is `Arc`-wrapped so the worker
/// can keep a clone and run flipbook detection *after* posting this (detection stays off the
/// time-to-first-pixel path); the UI stores its clone in the surface.
pub struct DecodeOutcome {
    pub window: Option<WindowId>,
    pub generation: u64,
    pub path: PathBuf,
    pub result: Result<Arc<DecodedImage>, DecodeError>,
    /// Levels `1..` of the image's mip chain (see [`crate::render::mips`]), built here on the
    /// worker so the UI thread's adopt is one upload; empty on failure.
    pub mips: Vec<Vec<u8>>,
    /// How many of `mips` (from level 1) came from the file itself rather than being computed
    /// here. Only a DDS carries its own chain, so this is 0 for every other format — and the
    /// mip-level control is offered only when it is above 0.
    pub authored_mips: u32,
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

/// A decoded PSD's layer stack, ready to step through — sent after its composite's `DecodeDone`
/// (the parse is metadata only, a fraction of a millisecond). Stale-dropped by `generation` like
/// a decode.
pub struct LayerIndex {
    pub window: Option<WindowId>,
    pub generation: u64,
    pub path: PathBuf,
    pub doc: Arc<PsdLayers>,
}

/// Decode one PSD layer: position `pos` (1-based, top first) is `doc.layers()[pos - 1]`.
pub struct LayerJob {
    pub window: WindowId,
    pub generation: u64,
    pub session: Arc<SessionCtl>,
    pub ctl: Arc<JobCtl>,
    pub pos: u32,
    pub doc: Arc<PsdLayers>,
    pub opts: DecodeOptions,
}

/// A decoded layer, as the viewer caches and uploads it.
pub struct LayerData {
    pub image: Arc<DecodedImage>,
    pub mips: Vec<Vec<u8>>,
    /// Top-left of `image` on the displayed canvas.
    pub origin: (u32, u32),
    /// The displayed canvas size.
    pub canvas: (u32, u32),
}

/// Why a layer job produced no image.
#[derive(Debug)]
pub enum LayerFail {
    /// No longer wanted: the cursor moved on, or the session ended.
    Superseded,
    Failed(String),
}

/// A finished layer job. Always sent — even for a job skipped before it started — so the viewer's
/// in-flight bookkeeping always closes.
pub struct LayerOutcome {
    pub window: WindowId,
    pub generation: u64,
    pub session: u64,
    pub pos: u32,
    pub result: Result<LayerData, LayerFail>,
}

/// Work for the main lane.
enum Job {
    Decode(DecodeJob),
    Layer(LayerJob),
}

/// Sender handle to the worker pool. Cheap to clone; every window holds one.
#[derive(Clone)]
pub struct DecodePool {
    tx: Sender<Job>,
    prefetch: Sender<LayerJob>,
    reaper: Sender<Box<dyn Any + Send>>,
    /// The newest generation ever submitted. Workers consult it before decoding: the queue is
    /// unbounded, so key-repeat navigation can enqueue jobs faster than they retire, and every
    /// superseded job decoded in full is a wasted allocation (up to ~1 GiB at `MAX_CPU_DIM`)
    /// parked in the event queue until the UI thread drains it.
    latest: Arc<AtomicU64>,
}

impl std::fmt::Debug for DecodePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodePool").finish_non_exhaustive()
    }
}

impl DecodePool {
    /// Spawn the worker threads. Each sends its results to the event loop through `proxy`.
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> Self {
        let (tx, rx) = unbounded::<Job>();
        let latest = Arc::new(AtomicU64::new(0));
        let workers = worker_count();
        let mut started = 0usize;
        for i in 0..workers {
            let rx = rx.clone();
            let latest = Arc::clone(&latest);
            let proxy = proxy.clone();
            let spawned = thread::Builder::new()
                .name(format!("fire-decode-{i}"))
                .spawn(move || main_worker(rx, latest, proxy));
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

        let (prefetch, prx) = unbounded::<LayerJob>();
        for i in 0..PREFETCH_WORKERS {
            let prx = prx.clone();
            let proxy = proxy.clone();
            let spawned = thread::Builder::new()
                .name(format!("fire-layer-{i}"))
                .spawn(move || {
                    crate::platform::set_current_thread_background(true);
                    while let Ok(job) = prx.recv() {
                        // Promoted before it started (the user stepped onto it while it was
                        // queued): run it at normal priority, like the main lane would.
                        let foreground = job.ctl.foreground.load(Ordering::Relaxed);
                        if foreground {
                            crate::platform::set_current_thread_background(false);
                        }
                        let keep_going = run_layer(job, &proxy);
                        if foreground {
                            crate::platform::set_current_thread_background(true);
                        }
                        if !keep_going {
                            break;
                        }
                    }
                });
            // Without a prefetch lane, layers still decode — on demand, on the main lane.
            if let Err(e) = spawned {
                eprintln!("fire: could not start layer prefetch worker {i}: {e}");
            }
        }

        let (reaper, rrx) = unbounded::<Box<dyn Any + Send>>();
        let spawned = thread::Builder::new()
            .name("fire-reaper".into())
            .spawn(move || {
                while let Ok(garbage) = rrx.recv() {
                    drop(garbage);
                }
            });
        if let Err(e) = spawned {
            eprintln!("fire: could not start the reaper thread: {e}");
        }

        Self {
            tx,
            prefetch,
            reaper,
            latest,
        }
    }

    /// Enqueue a decode. The unbounded channel only fails to send once every worker
    /// has exited (shutdown), so a dropped job here is benign.
    pub fn submit(&self, job: DecodeJob) {
        // fetch_max, not store: submit order and the generation counter agree today, but the
        // skip must never move backwards if they ever don't.
        self.latest.fetch_max(job.generation, Ordering::Relaxed);
        let _ = self.tx.send(Job::Decode(job));
    }

    /// Enqueue a layer decode: on the main lane if the user is waiting for it, else on the
    /// prefetch lane. Layer jobs are exempt from the generation skip — their own controls
    /// (`SessionCtl`/`JobCtl`) say when they are no longer wanted.
    pub fn submit_layer(&self, job: LayerJob) {
        if job.ctl.foreground.load(Ordering::Relaxed) {
            job.ctl.main_lane.store(true, Ordering::Relaxed);
            let _ = self.tx.send(Job::Layer(job));
        } else if let Err(e) = self.prefetch.send(job) {
            // No prefetch lane (it failed to start): run it on the main lane instead.
            let job = e.into_inner();
            job.ctl.main_lane.store(true, Ordering::Relaxed);
            let _ = self.tx.send(Job::Layer(job));
        }
    }

    /// Free `garbage` on the reaper thread rather than the caller's. For the UI thread's big
    /// drops — a PSD session's file bytes, evicted layers — so they never stall a frame.
    pub fn dispose(&self, garbage: Box<dyn Any + Send>) {
        if let Err(e) = self.reaper.send(garbage) {
            drop(e.into_inner()); // no reaper: free it here after all
        }
    }
}

/// A main-lane worker: file decodes and foreground layer decodes, until the pool is dropped.
fn main_worker(rx: Receiver<Job>, latest: Arc<AtomicU64>, proxy: EventLoopProxy<AppEvent>) {
    while let Ok(job) = rx.recv() {
        let keep_going = match job {
            Job::Decode(job) => run_decode(job, &latest, &proxy),
            Job::Layer(job) => run_layer(job, &proxy),
        };
        if !keep_going {
            break;
        }
    }
}

/// Run one file decode: the image, then the PSD layer index (parsed before the mip chain, sent
/// after the image) and the flipbook guess. Returns false when the event loop is gone and the worker should stop.
fn run_decode(job: DecodeJob, latest: &AtomicU64, proxy: &EventLoopProxy<AppEvent>) -> bool {
    // A superseded job's result would be stale-dropped on arrival anyway; once a newer submit
    // exists, skip the decode itself.
    if job.generation < latest.load(Ordering::Relaxed) {
        return true;
    }
    let (mut result, psd_bytes) = decode(&job);
    // Parsed before the mip chain is built, so a PSD with no layer index to keep its bytes for
    // (a PSB, no steppable layers, a window that has moved on) frees them now rather than holding
    // them through the mips as well. Sent after the image.
    let layer_doc = psd_bytes.and_then(|bytes| open_layers(&job, bytes));
    // Take the file's own chain out of the image before it is shared: only the uploader wants it,
    // and leaving it in would keep a second copy of roughly a third of level 0 alive behind every
    // `Arc` for the life of the window.
    let source_mips = result.as_mut().ok().and_then(|img| img.source_mips.take());
    let result = result.map(Arc::new);
    // The mip chain, completed here so the UI thread's adopt is one upload.
    let (mips, authored_mips) = match &result {
        Ok(img) => {
            let t = Instant::now();
            // Counted before `complete` consumes the levels: only the run it adopts is the
            // file's, everything past it is computed.
            let authored = source_mips.as_deref().map_or(0, |src| {
                crate::render::mips::authored_levels(src, img.width, img.height, img.format)
            });
            let chain = match source_mips {
                // A DDS brings its own, and those levels were authored — with a filter and a
                // gamma a box filter does not reproduce. Keep what the file supplied and compute
                // only the tail it stopped short of.
                Some(src) => crate::render::mips::complete(
                    src,
                    &img.pixels,
                    img.width,
                    img.height,
                    img.format,
                ),
                None => crate::render::mips::build(&img.pixels, img.width, img.height, img.format),
            };
            crate::render::gpu::report_timing(&format!(
                "mips — {:.2} ms ({} levels, {authored} from the file, {}×{})",
                t.elapsed().as_secs_f64() * 1e3,
                chain.len() + 1,
                img.width,
                img.height
            ));
            // `complete` hands back nothing for a short level 0, and then none of the file's
            // levels reach the texture either.
            let authored = if chain.is_empty() { 0 } else { authored };
            (chain, authored as u32)
        }
        Err(_) => (Vec::new(), 0),
    };
    // Keep a clone to run flipbook detection *after* the image is posted, so a large sheet
    // reaches the screen without waiting on the per-pixel scan. Skipped for animated sources (a
    // GIF is not a sprite sheet), for a source whose grid the *file* stated (a DDS
    // cubemap/array/volume — authored structure is never guessed at), and when the user turned
    // detection off.
    let detect_input = result
        .as_ref()
        .ok()
        .filter(|img| job.detect_flipbook && img.animation.is_none() && img.layout.is_none())
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
        mips,
        authored_mips,
        reload: job.reload,
    });
    // The only way a send fails is a closed event loop: the app is exiting.
    if proxy.send_event(AppEvent::DecodeDone(outcome)).is_err() {
        return false;
    }

    // --- Then the PSD's layer index. Nothing here can delay the image above, which is already
    // on its way.
    if let Some(doc) = layer_doc {
        let index = Box::new(LayerIndex {
            window,
            generation,
            path: path.clone(),
            doc,
        });
        if proxy.send_event(AppEvent::PsdLayers(index)).is_err() {
            return false;
        }
    }

    // --- Then detect the flipbook grid off the critical path and send the hint separately.
    // Detection never touches a window/renderer and is never allowed to kill the worker (a
    // malformed sheet mustn't take the pool down).
    if let Some(img) = detect_input {
        let guess = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
            return false;
        }
    }
    true
}

/// Parse a PSD's layer index from the bytes its composite was decoded from. `None` when it has
/// no steppable layers, cannot be parsed, or its window has moved on — the bytes are freed then.
fn open_layers(job: &DecodeJob, bytes: Vec<u8>) -> Option<Arc<PsdLayers>> {
    let stale = job
        .gen_cell
        .as_ref()
        .is_some_and(|c| c.load(Ordering::Relaxed) != job.generation);
    if stale {
        return None;
    }
    let t = Instant::now();
    match PsdLayers::open(Arc::new(bytes)) {
        Ok(Some(doc)) => {
            crate::render::gpu::report_timing(&format!(
                "psd layer index — {:.2} ms ({} layers)",
                t.elapsed().as_secs_f64() * 1e3,
                doc.layers().len()
            ));
            Some(Arc::new(doc))
        }
        Ok(None) => None,
        Err(e) => {
            eprintln!("fire: {}: layers unavailable: {e}", job.path.display());
            None
        }
    }
}

/// Run one layer job and report it. Returns false when the event loop is gone.
fn run_layer(job: LayerJob, proxy: &EventLoopProxy<AppEvent>) -> bool {
    let t = Instant::now();
    let result = decode_layer(&job);
    if let Ok(data) = &result {
        crate::render::gpu::report_timing(&format!(
            "psd layer {} — {:.2} ms ({}×{}, {})",
            job.pos,
            t.elapsed().as_secs_f64() * 1e3,
            data.image.width,
            data.image.height,
            if job.ctl.foreground.load(Ordering::Relaxed) {
                "foreground"
            } else {
                "prefetch"
            }
        ));
    }
    let outcome = Box::new(LayerOutcome {
        window: job.window,
        generation: job.generation,
        session: job.session.id,
        pos: job.pos,
        result,
    });
    proxy.send_event(AppEvent::LayerDecoded(outcome)).is_ok()
}

fn decode_layer(job: &LayerJob) -> Result<LayerData, LayerFail> {
    let wanted = || {
        !job.ctl.cancelled.load(Ordering::Relaxed)
            && (job.ctl.foreground.load(Ordering::Relaxed) || job.session.wants(job.pos))
            && !job.session.cancelled.load(Ordering::Relaxed)
    };
    // Every core while the user waits; one otherwise, so prefetch never competes with them.
    let threads = || {
        if job.ctl.foreground.load(Ordering::Relaxed) {
            0
        } else {
            1
        }
    };
    if !wanted() {
        return Err(LayerFail::Superseded);
    }
    let cancel = || !wanted();
    let ctl = LayerCtl {
        threads: &threads,
        cancel: &cancel,
    };
    let layer = job.pos.checked_sub(1).ok_or(LayerFail::Superseded)? as usize;
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        job.doc.decode_layer(layer, &job.opts, &ctl)
    }))
    .unwrap_or_else(|_| {
        Err(LayerError::Decode(DecodeError::Other(
            "layer decoder panicked".into(),
        )))
    });
    let img = match decoded {
        Ok(img) => img,
        Err(LayerError::Cancelled) => return Err(LayerFail::Superseded),
        Err(LayerError::Decode(e)) => return Err(LayerFail::Failed(e.to_string())),
    };
    if !wanted() {
        return Err(LayerFail::Superseded);
    }
    let im = &img.image;
    let mips = crate::render::mips::build_with_threads(
        &im.pixels,
        im.width,
        im.height,
        im.format,
        threads(),
    );
    if !wanted() {
        return Err(LayerFail::Superseded);
    }
    Ok(LayerData {
        image: Arc::new(img.image),
        mips,
        origin: img.origin,
        canvas: img.canvas,
    })
}

/// Decode one job, converting any panic into a `DecodeError` so a worker thread is never lost.
/// The decode crate already wraps its C/C++ FFI in `catch_unwind`; this is a belt-and-suspenders
/// boundary around the whole pure-Rust + FFI path.
///
/// Reads the file here rather than through `fire_decode::decode_path` so a PSD's bytes can be
/// kept for its layer index (returned alongside the image when the file is a PSD and its
/// composite decoded). Every other file's bytes are freed right after its decode, as before.
fn decode(job: &DecodeJob) -> (Result<DecodedImage, DecodeError>, Option<Vec<u8>>) {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let bytes = match std::fs::read(&job.path) {
            Ok(b) => b,
            Err(e) => return (Err(DecodeError::Other(e.to_string())), None),
        };
        let ext = job.path.extension().and_then(|e| e.to_str());
        let result = fire_decode::decode(&bytes, ext, &job.opts);
        let keep = result.is_ok() && bytes.starts_with(b"8BPS");
        (result, keep.then_some(bytes))
    }))
    .unwrap_or_else(|_| (Err(DecodeError::Other("decoder panicked".into())), None))
}

fn worker_count() -> usize {
    thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, MAX_WORKERS)
}
