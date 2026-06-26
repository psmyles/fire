//! Off-thread decode worker pool (Option A from the plan — no async runtime).
//!
//! Decoding a large PSD/EXR can take tens of milliseconds; doing it on the UI thread
//! would freeze the window between the click and the image. Instead, `open` shows the
//! window with a placeholder immediately and hands a [`DecodeJob`] to this pool. A worker
//! decodes on a background thread and posts the result back to the UI thread by
//! `PostMessage`-ing the window with [`crate::win::WM_APP_DECODE_DONE`] and a boxed
//! [`DecodeOutcome`] in the LPARAM (workers never touch the window or the renderer — same
//! discipline as the pipe-server thread).
//!
//! Each job carries the issuing window's monotonic `generation`; the UI uploads a result
//! only if it is still the window's latest generation, so a slow decode can never clobber
//! a newer one (stale-drop). A superseded job is still decoded — its result is just
//! dropped on arrival — which wastes a little work but keeps the pool dead simple.

use std::path::PathBuf;
use std::thread;

use crossbeam_channel::{unbounded, Sender};
use fire_decode::{decode_path, DecodeError, DecodeOptions, DecodedImage};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::win::WM_APP_DECODE_DONE;

/// Plan-adopted default pool size: `min(num_cpus, 4)`.
const MAX_WORKERS: usize = 4;

/// A unit of decode work handed to a worker thread.
#[derive(Debug)]
pub struct DecodeJob {
    /// The issuing window's generation at submit time; used for stale-drop.
    pub generation: u64,
    pub path: PathBuf,
    pub opts: DecodeOptions,
}

/// A finished decode, delivered back to the UI thread (boxed, via the message LPARAM).
pub struct DecodeOutcome {
    pub generation: u64,
    pub path: PathBuf,
    pub result: Result<DecodedImage, DecodeError>,
}

/// Sender handle to the worker pool; held by the `App` for the window's lifetime.
pub struct DecodePool {
    tx: Sender<DecodeJob>,
}

impl DecodePool {
    /// Spawn the worker threads. Each posts results back to `hwnd` (the UI window) via
    /// `PostMessage`; `hwnd` is passed as an `isize` so it crosses the thread boundary
    /// (a raw HWND is just an integer).
    pub fn new(hwnd: isize) -> Self {
        let (tx, rx) = unbounded::<DecodeJob>();
        let workers = worker_count();
        for i in 0..workers {
            let rx = rx.clone();
            thread::Builder::new()
                .name(format!("fire-decode-{i}"))
                .spawn(move || {
                    // Exits when the pool (and thus `tx`) is dropped at shutdown.
                    while let Ok(job) = rx.recv() {
                        let result = decode(&job);
                        let outcome = Box::new(DecodeOutcome {
                            generation: job.generation,
                            path: job.path,
                            result,
                        });
                        let lparam = Box::into_raw(outcome) as isize;
                        // SAFETY: the box outlives the post; the UI thread reclaims it in
                        // the wndproc. If the post fails (window gone), reclaim here so we
                        // don't leak.
                        let posted = unsafe {
                            PostMessageW(hwnd as HWND, WM_APP_DECODE_DONE, 0, lparam)
                        };
                        if posted == 0 {
                            drop(unsafe { Box::from_raw(lparam as *mut DecodeOutcome) });
                            break; // window is gone; stop working
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
