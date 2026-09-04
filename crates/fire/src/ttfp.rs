//! The time-to-first-pixel stamp — measurement only, inert unless asked for.
//!
//! When `FIRE_TTFP_OUT` names a file, the first presented frame that carries image pixels writes
//! the milliseconds since the *kernel created this process* to that file and exits, so a harness
//! (`scripts/ttfp.ps1`) can loop launches. Process creation, not `main()`: `Instant::now()` at the
//! top of `main` cannot see the loader and CRT time, which the user pays for just the same. Release
//! builds have no console, so the number goes to a file, not stderr.
//!
//! The stamp sits at the one place the frame is handed to the display, gated on an image being on
//! the surface, so every build measured is measured at the same conceptual point.

/// Record the first image-bearing present and exit, if `FIRE_TTFP_OUT` is set. Otherwise a cheap
/// no-op (one static read).
pub fn stamp_first_pixel() {
    use std::sync::OnceLock;
    static OUT: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let Some(path) = OUT.get_or_init(|| std::env::var_os("FIRE_TTFP_OUT").map(Into::into)) else {
        return;
    };
    let ms = ms_since_process_creation();
    let _ = std::fs::write(path, format!("{ms:.3}\n"));
    std::process::exit(0);
}

/// Milliseconds from the kernel's process-creation time to now.
#[cfg(windows)]
fn ms_since_process_creation() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    // FILETIME is 100 ns ticks since 1601-01-01; the Unix epoch is this many ticks later.
    const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;

    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: the pseudo-handle from GetCurrentProcess is always valid; the out-params are ours.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exit,
            &mut kernel,
            &mut user,
        );
    }
    let created = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
    // `SystemTime::now()` is GetSystemTimePreciseAsFileTime on Windows — the same clock.
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let now = (now_unix.as_nanos() / 100) as u64 + UNIX_EPOCH_AS_FILETIME;
    now.saturating_sub(created) as f64 / 10_000.0
}

/// Milliseconds since the process started, as best this OS can say: the process's own clock,
/// which cannot see the loader time. Good enough to compare two builds against each other.
#[cfg(not(windows))]
fn ms_since_process_creation() -> f64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
        * 1e3
}
