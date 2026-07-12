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
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{unbounded, Sender};
use fire_decode::{decode_path, DecodeError, DecodeOptions, DecodedImage};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::win::{WM_APP_DECODE_DONE, WM_APP_FLIPBOOK_GUESS};

/// Plan-adopted default pool size: `min(num_cpus, 4)`.
const MAX_WORKERS: usize = 4;

/// A unit of decode work handed to a worker thread.
#[derive(Debug)]
pub struct DecodeJob {
    /// The issuing window's generation at submit time; used for stale-drop.
    pub generation: u64,
    pub path: PathBuf,
    pub opts: DecodeOptions,
    /// True if this is a hot-reload of the displayed file (vs. a fresh open/navigate). The UI
    /// uses it to keep the current view when the re-decoded image has the same dimensions.
    pub reload: bool,
    /// Whether to run sprite-sheet auto-detection after posting the image (the
    /// `flipbook.auto-detect` config key). False skips the per-pixel scan entirely — no
    /// [`WM_APP_FLIPBOOK_GUESS`] is posted, so no hint chip can appear.
    pub detect_flipbook: bool,
}

/// A finished decode, delivered back to the UI thread (boxed, via the message LPARAM). The image
/// is `Arc`-wrapped so the worker can keep a clone and run flipbook detection *after* posting this
/// (detection stays off the time-to-first-pixel path); the UI stores its clone in the surface.
pub struct DecodeOutcome {
    pub generation: u64,
    pub path: PathBuf,
    pub result: Result<Arc<DecodedImage>, DecodeError>,
    /// Echoed from the job; see [`DecodeJob::reload`].
    pub reload: bool,
}

/// The flipbook auto-detection result for a decoded image, delivered to the UI thread *after* the
/// image itself (a separate [`WM_APP_FLIPBOOK_GUESS`] message) so the analysis — which for a large
/// sheet scans every pixel — never delays the image reaching the screen. `guess` is the detected
/// grid, or `None` for a non-sheet image. Stale-dropped by `generation` like a decode.
pub struct FlipbookGuess {
    pub generation: u64,
    pub path: PathBuf,
    pub guess: Option<crate::flipbook::Grid>,
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
                        let path = job.path.clone();

                        // --- Post the decoded image immediately (time-to-first-pixel path) ---
                        let outcome = Box::new(DecodeOutcome {
                            generation,
                            path: job.path,
                            result,
                            reload: job.reload,
                        });
                        let lparam = Box::into_raw(outcome) as isize;
                        // SAFETY: the box outlives the post; the UI thread reclaims it in
                        // the wndproc. If the post fails (window gone), reclaim here so we
                        // don't leak.
                        let posted =
                            unsafe { PostMessageW(hwnd as HWND, WM_APP_DECODE_DONE, 0, lparam) };
                        if posted == 0 {
                            drop(unsafe { Box::from_raw(lparam as *mut DecodeOutcome) });
                            break; // window is gone; stop working
                        }

                        // --- Then detect the flipbook grid off the critical path and post the hint
                        // separately. Detection never touches the window/renderer and is never
                        // allowed to kill the worker (a malformed sheet mustn't take the pool down).
                        if let Some(img) = detect_input {
                            let guess =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    crate::flipbook::detect(&path, &img)
                                }))
                                .unwrap_or(None);
                            let hint = Box::new(FlipbookGuess {
                                generation,
                                path,
                                guess,
                            });
                            let hint_lparam = Box::into_raw(hint) as isize;
                            let posted = unsafe {
                                PostMessageW(hwnd as HWND, WM_APP_FLIPBOOK_GUESS, 0, hint_lparam)
                            };
                            if posted == 0 {
                                drop(unsafe { Box::from_raw(hint_lparam as *mut FlipbookGuess) });
                                break; // window is gone; stop working
                            }
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
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_path(&job.path, &job.opts)
    }))
    .unwrap_or_else(|_| Err(DecodeError::Other("decoder panicked".into())))
}

fn worker_count() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, MAX_WORKERS)
}
