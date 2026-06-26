//! Hot-reload: re-decode the displayed image when its file changes on disk.
//!
//! A single long-lived thread per window owns an OS file watch (the `notify` crate, which on
//! Windows is `ReadDirectoryChangesW`). The UI thread re-targets it on every open/navigate via
//! [`FileWatcher::watch`]; when the watched file's *contents* change, the thread posts
//! [`crate::win::WM_APP_FILE_CHANGED`] to the frame, which re-runs the decode. Like the decode
//! pool and folder scan, this thread never touches the window or renderer — it only `PostMessage`s
//! (here with no payload; the UI re-decodes its own current path).
//!
//! Three things keep it well-behaved:
//! - **Watch the directory, not the file.** Editors save atomically (write a temp, rename over the
//!   target), which breaks a watch on the file itself; watching the parent dir non-recursively and
//!   filtering by name survives that and is the `notify`-recommended pattern.
//! - **Debounce.** A save often lands as several write bursts; we wait for a quiet period so we
//!   decode the finished file once, not a half-written one repeatedly.
//! - **Content-change guard (mtime + size).** We fire only when the file's modified-time or length
//!   actually changes from the last seen baseline. This ignores pure metadata/last-access touches
//!   and — crucially — our own decode reads, so a reload can never trigger another reload.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{select, unbounded, Receiver, Sender};
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::win::WM_APP_FILE_CHANGED;

/// Quiet period after the last filesystem event before we trigger a reload. Long enough to
/// coalesce a multi-burst save into one reload (and to let the writer finish), short enough that
/// the update still feels immediate.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// Idle block when nothing is pending. The two recv arms wake us on real activity; this is just
/// the ceiling on how long `select!` parks (a stray hourly wake is harmless).
const IDLE: Duration = Duration::from_secs(3600);

/// A request from the UI thread: watch `path` (the now-current image) and tag any reload it
/// triggers with `generation` (stale-drop, mirroring decodes and folder scans).
struct WatchCmd {
    generation: u64,
    path: PathBuf,
}

/// Handle to the watcher thread, held by the `App` for the window's lifetime. Dropping it closes
/// the command channel, which ends the thread (releasing the OS watch).
pub struct FileWatcher {
    cmd_tx: Sender<WatchCmd>,
}

impl FileWatcher {
    /// Spawn the watcher thread. It posts [`WM_APP_FILE_CHANGED`] to `frame` (passed as `isize`
    /// so the raw HWND crosses the thread boundary, like the decode pool).
    pub fn spawn(frame: isize) -> Self {
        let (cmd_tx, cmd_rx) = unbounded::<WatchCmd>();
        let _ = std::thread::Builder::new()
            .name("fire-file-watch".into())
            .spawn(move || run(frame, cmd_rx));
        Self { cmd_tx }
    }

    /// Point the watcher at `path` (the image now on screen), tagging future reloads with
    /// `generation`. Cheap and non-blocking; the thread does the (re)watch.
    pub fn watch(&self, generation: u64, path: &Path) {
        let _ = self.cmd_tx.send(WatchCmd { generation, path: path.to_path_buf() });
    }
}

/// What the watcher is currently tracking. Rebuilt on every [`WatchCmd`].
struct TargetState {
    /// Generation to stamp on a posted reload (the UI drops it if no longer current).
    generation: u64,
    /// Full path of the watched image (used to stat for the content-change guard).
    path: PathBuf,
    /// Lowercased file name, for matching events within the watched directory.
    name_lc: String,
    /// Last seen `(modified-time, size)`; a reload fires only when this changes.
    baseline: Option<(SystemTime, u64)>,
}

fn run(frame: isize, cmd_rx: Receiver<WatchCmd>) {
    // notify delivers events to this channel; a closure (not a raw Sender) avoids needing
    // notify's optional crossbeam-channel feature.
    let (event_tx, event_rx) = unbounded::<notify::Result<Event>>();
    let mut watcher: RecommendedWatcher = match notify::recommended_watcher(move |res| {
        let _ = event_tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("fire: file watcher unavailable, hot-reload disabled: {e}");
            return;
        }
    };

    let mut watched_dir: Option<PathBuf> = None;
    let mut target: Option<TargetState> = None;
    // When `Some`, a content change is pending and we fire once `Instant::now()` passes it.
    let mut deadline: Option<Instant> = None;

    loop {
        let timeout = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => IDLE,
        };
        select! {
            recv(cmd_rx) -> msg => match msg {
                // A new target supersedes any pending reload from the previous one.
                Ok(cmd) => { retarget(&mut watcher, &mut watched_dir, &mut target, cmd); deadline = None; }
                // The App dropped the sender → the window is gone → stop watching.
                Err(_) => return,
            },
            recv(event_rx) -> ev => {
                if let (Ok(Ok(event)), Some(t)) = (ev, target.as_mut()) {
                    if event_is_relevant(&event, &t.name_lc) {
                        let now_meta = file_meta(&t.path);
                        // Fire only on a real content change; a failed stat (file mid-rename)
                        // leaves the baseline so the follow-up rename-to event still fires.
                        if now_meta.is_some() && now_meta != t.baseline {
                            t.baseline = now_meta;
                            deadline = Some(Instant::now() + DEBOUNCE);
                        }
                    }
                }
            },
            default(timeout) => {
                if let (Some(t), Some(d)) = (target.as_ref(), deadline) {
                    if Instant::now() >= d {
                        post_changed(frame, t.generation);
                        deadline = None;
                    }
                }
            },
        }
    }
}

/// Re-point the OS watch and the tracked target at `cmd.path`. The directory watch is only
/// rebuilt when the parent actually changes, so paging through one folder keeps a single watch.
fn retarget(
    watcher: &mut RecommendedWatcher,
    watched_dir: &mut Option<PathBuf>,
    target: &mut Option<TargetState>,
    cmd: WatchCmd,
) {
    let dir = parent_dir(&cmd.path);
    if watched_dir.as_deref() != Some(dir.as_path()) {
        if let Some(old) = watched_dir.take() {
            let _ = watcher.unwatch(&old);
        }
        match watcher.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => *watched_dir = Some(dir),
            Err(e) => eprintln!("fire: cannot watch {}: {e}", dir.display()),
        }
    }
    let name_lc = cmd
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // Baseline at arm time so only changes *from now* fire (not the file's pre-existing state).
    let baseline = file_meta(&cmd.path);
    *target = Some(TargetState { generation: cmd.generation, path: cmd.path, name_lc, baseline });
}

/// Whether `event` is a create/modify touching the watched file name. Removes and access events
/// are ignored (don't reload a deleted file; don't react to reads). The Windows backend reports
/// most writes as `Modify(Any)`, so that must count.
fn event_is_relevant(event: &Event, name_lc: &str) -> bool {
    let kind_ok = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Name(_))
            | EventKind::Modify(ModifyKind::Any)
            | EventKind::Modify(ModifyKind::Other)
            | EventKind::Any
            | EventKind::Other
    );
    kind_ok
        && event.paths.iter().any(|p| {
            p.file_name().map(|n| n.to_string_lossy().to_lowercase() == name_lc).unwrap_or(false)
        })
}

/// `(modified-time, size)` for the content-change guard; `None` if the file can't be stat'd.
fn file_meta(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// Directory to watch for `path`. A bare relative file name has an empty parent; watch `.` (the
/// cwd) so the file's directory is still observed.
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Post the reload wakeup to the UI thread. No payload — the UI re-decodes its own current path;
/// `generation` (in WPARAM) lets it drop the event if the user has since navigated away.
fn post_changed(frame: isize, generation: u64) {
    // SAFETY: posting to the frame HWND. If the window is gone the post simply fails; LPARAM is
    // 0 so there is nothing to reclaim.
    unsafe {
        PostMessageW(frame as HWND, WM_APP_FILE_CHANGED, generation as usize, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, RemoveKind};

    fn ev(kind: EventKind, path: &str) -> Event {
        Event { kind, paths: vec![PathBuf::from(path)], attrs: Default::default() }
    }

    #[test]
    fn relevant_matches_target_name_case_insensitively() {
        let name = "photo.png".to_string(); // stored lowercased by retarget
        assert!(event_is_relevant(&ev(EventKind::Modify(ModifyKind::Any), r"C:\d\PHOTO.PNG"), &name));
        assert!(event_is_relevant(&ev(EventKind::Create(CreateKind::Any), r"C:\d\photo.png"), &name));
    }

    #[test]
    fn irrelevant_for_other_files_and_for_removes() {
        let name = "photo.png".to_string();
        // A different file changing in the same watched directory must not reload us.
        assert!(!event_is_relevant(&ev(EventKind::Modify(ModifyKind::Any), r"C:\d\other.png"), &name));
        // A delete of the target is ignored — we keep showing the last good image.
        assert!(!event_is_relevant(&ev(EventKind::Remove(RemoveKind::Any), r"C:\d\photo.png"), &name));
    }

    #[test]
    fn parent_dir_uses_cwd_for_a_bare_name() {
        assert_eq!(parent_dir(Path::new("img.png")), PathBuf::from("."));
        assert_eq!(parent_dir(Path::new(r"C:\d\img.png")), PathBuf::from(r"C:\d"));
    }
}
