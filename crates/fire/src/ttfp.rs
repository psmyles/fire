//! The time-to-first-pixel stamp — measurement only, inert unless asked for.
//!
//! When `FIRE_TTFP_OUT` names a file, the first presented frame that carries image pixels writes
//! the milliseconds since the *kernel created this process* to that file and exits, so a harness
//! (`scripts/ttfp.ps1` on Windows, `scripts/ttfp.sh` on macOS) can loop launches. Process creation,
//! not `main()`: `Instant::now()` at the top of `main` cannot see the loader and CRT time, which
//! the user pays for just the same. Release builds have no console, so the number goes to a file,
//! not stderr.
//!
//! The stamp sits at the one place the frame is handed to the display, gated on an image being on
//! the surface, so every build measured is measured at the same conceptual point.

/// Milliseconds from process creation to now, for the startup breakdown (`FIRE_TIMING`).
///
/// The same clock the stamp uses, exposed so `main` can report what it cost to *get* to `main` —
/// the loader and runtime setup that the phase timings, which all start from an `Instant` taken
/// inside the process, structurally cannot see. On the mac launch path that share is large enough
/// that a breakdown without it does not add up.
pub fn ms_since_start() -> f64 {
    ms_since_process_creation()
}

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

/// Milliseconds from the kernel's process-creation time to now.
///
/// `proc_pidinfo(PROC_PIDTBSDINFO)` is the true equivalent of the Windows arm's `GetProcessTimes`:
/// the kernel's own record of when this process began, so — unlike an `Instant` taken at the top
/// of `main` — it includes dyld, which on a first (cold) launch of a bundle is a real share of the
/// number. Both halves of the subtraction are wall-clock: `pbi_start_tv*` is the `gettimeofday`
/// value at exec, and `SystemTime::now` reads the same clock.
#[cfg(target_os = "macos")]
fn ms_since_process_creation() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    // SAFETY: the buffer is a `proc_bsdinfo` and the size passed is its own; `proc_pidinfo` writes
    // at most that many bytes and reports how many it wrote.
    let (info, wrote) = unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let n = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        );
        (info, n)
    };
    if wrote != std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int {
        // No origin means no measurement. A NaN propagates to the file and the harness rejects
        // the run, which is the honest outcome — a plausible-looking number would not be.
        return f64::NAN;
    }
    let started = info.pbi_start_tvsec as f64 * 1e3 + info.pbi_start_tvusec as f64 / 1e3;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1e3;
    now - started
}

/// No process-creation clock on this platform, and no way to fake one honestly.
///
/// The previous version of this arm initialised a `OnceLock<Instant>` *inside* the measurement and
/// so reported ~0.000 ms on every run — a number that looked like a result. A NaN is rejected by
/// the harness instead, which is what "not measured here" should look like. `fire` ships on
/// Windows and macOS, both of which have a real arm above.
#[cfg(not(any(windows, target_os = "macos")))]
fn ms_since_process_creation() -> f64 {
    f64::NAN
}
