//! Off-thread decode worker pool (Option A from the plan — no async runtime).
//!
//! Decoding a large PSD/EXR can take tens of milliseconds; doing it on the winit
//! thread would freeze the window between the click and the image. Instead, `open`
//! shows the window with a placeholder immediately and hands a [`DecodeJob`] to this
//! pool. A worker decodes on a background thread and posts the result back to the event
//! loop as [`UserEvent::DecodeDone`] via the `EventLoopProxy` (workers never touch wgpu
//! or the window — same discipline as the pipe-server thread).
//!
//! Each job carries the issuing window's monotonic `generation`; the app uploads a
//! result only if it is still the window's latest generation, so a slow decode can
//! never clobber a newer one (stale-drop). A superseded job is still decoded — its
//! result is just dropped on arrival — which wastes a little work but keeps the pool
//! dead simple (no cancellation plumbing) for v1.

use std::path::PathBuf;
use std::thread;

use crossbeam_channel::{unbounded, Sender};
use fire_decode::{decode_path, DecodeError, DecodeOptions, DecodedImage};
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use crate::UserEvent;

/// Plan-adopted default pool size: `min(num_cpus, 4)`.
const MAX_WORKERS: usize = 4;

/// A unit of decode work handed to a worker thread.
#[derive(Debug)]
pub struct DecodeJob {
    /// Window the result is destined for (forward-compat for multi-window, Phase 4).
    pub window_id: WindowId,
    /// The issuing window's generation at submit time; used for stale-drop.
    pub generation: u64,
    pub path: PathBuf,
    pub opts: DecodeOptions,
}

/// A finished decode, delivered back to the event loop.
pub struct DecodeOutcome {
    pub window_id: WindowId,
    pub generation: u64,
    pub path: PathBuf,
    pub result: Result<DecodedImage, DecodeError>,
}

// Hand-rolled so a debug print of the carrying `UserEvent` never dumps the decoded
// pixel buffer (megabytes) — show dimensions on success instead.
impl std::fmt::Debug for DecodeOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("DecodeOutcome");
        d.field("window_id", &self.window_id)
            .field("generation", &self.generation)
            .field("path", &self.path);
        match &self.result {
            Ok(img) => d.field("result", &format_args!("Ok({}x{})", img.width, img.height)),
            Err(e) => d.field("result", &format_args!("Err({e})")),
        };
        d.finish()
    }
}

/// Sender handle to the worker pool; held by the `App` for the daemon's lifetime.
pub struct DecodePool {
    tx: Sender<DecodeJob>,
}

impl DecodePool {
    /// Spawn the worker threads. Each posts results back through `proxy`.
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let (tx, rx) = unbounded::<DecodeJob>();
        let workers = worker_count();
        for i in 0..workers {
            let rx = rx.clone();
            let proxy = proxy.clone();
            thread::Builder::new()
                .name(format!("fire-decode-{i}"))
                .spawn(move || {
                    // Exits when the pool (and thus `tx`) is dropped at shutdown.
                    while let Ok(job) = rx.recv() {
                        let result = decode(&job);
                        let outcome = DecodeOutcome {
                            window_id: job.window_id,
                            generation: job.generation,
                            path: job.path,
                            result,
                        };
                        if proxy.send_event(UserEvent::DecodeDone(outcome)).is_err() {
                            break; // event loop is gone; stop working
                        }
                    }
                })
                .expect("failed to spawn decode worker");
        }
        Self { tx }
    }

    /// Enqueue a decode. The unbounded channel only fails to send once every worker
    /// has exited (shutdown), so a dropped job here is benign.
    pub fn submit(&self, job: DecodeJob) {
        let _ = self.tx.send(job);
    }
}

/// Decode one job, converting any panic into a `DecodeError` so a worker thread is
/// never lost. The decode crate already wraps its C/C++ FFI in `catch_unwind`; this is
/// a belt-and-suspenders boundary around the whole pure-Rust + FFI path.
fn decode(job: &DecodeJob) -> Result<DecodedImage, DecodeError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_path(&job.path, &job.opts)))
        .unwrap_or_else(|_| Err(DecodeError::Other("decoder panicked".into())))
}

fn worker_count() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, MAX_WORKERS)
}
