//! The platform leaves: everything in the shell that still has to name an OS.
//!
//! The rule (architecture.md §12.1) is that nothing *else* in `fire` mentions one. Each item here is
//! one small function behind a `cfg`, with a no-op or portable fallback for the other OS, so the
//! shell above reads as one program:
//!
//! * the launcher's Run = Normal/Minimized/Maximized (a Windows `STARTUPINFO` field);
//! * the one-shot foreground grant on the forward path (`AllowSetForegroundWindow`);
//! * the clipboard, for the actions menu's Copy File / Copy Path / Copy File Name;
//! * "Show in Explorer" / "Reveal in Finder";
//! * where the UI font lives.

use std::path::Path;

/// How the launcher asked the window to be shown.
///
/// Every variant is matched by the viewer on both OSes, but only [`launcher_show`]'s Windows arm
/// ever *constructs* one — off Windows it always answers `None`, which is what the dead-code
/// allowance is for. The alternative, `cfg`-ing the variants themselves, would make the viewer's
/// match arms platform-specific too, which is the opposite of what this module is for.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchShow {
    Normal,
    Maximized,
    Minimized,
}

/// The show state the launcher requested — a Windows shortcut's "Run" field (Normal / Minimized /
/// Maximized), or what `CreateProcess` passed as `nCmdShow`. `None` if the launcher didn't
/// specify one (then the remembered state is used). Always `None` off Windows: Finder has no
/// such setting.
pub fn launcher_show() -> Option<LaunchShow> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetStartupInfoW, STARTF_USESHOWWINDOW, STARTUPINFOW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SW_FORCEMINIMIZE, SW_MAXIMIZE, SW_MINIMIZE, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED,
            SW_SHOWMINNOACTIVE,
        };
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        unsafe { GetStartupInfoW(&mut si) };
        if si.dwFlags & STARTF_USESHOWWINDOW == 0 {
            return None;
        }
        let cmd = si.wShowWindow as i32;
        if cmd == SW_SHOWMAXIMIZED || cmd == SW_MAXIMIZE {
            Some(LaunchShow::Maximized)
        } else if cmd == SW_SHOWMINIMIZED
            || cmd == SW_SHOWMINNOACTIVE
            || cmd == SW_MINIMIZE
            || cmd == SW_FORCEMINIMIZE
        {
            Some(LaunchShow::Minimized)
        } else {
            Some(LaunchShow::Normal)
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Grant the running instance the right to take the foreground when it opens our path.
///
/// A process that doesn't own the foreground normally cannot raise its own window: Windows
/// ignores `SetForegroundWindow` from it. The forwarding launch — which Explorer just gave the
/// foreground — hands it over with `AllowSetForegroundWindow` right before writing the request;
/// the owner then focuses the window promptly on receipt. `ASFW_ANY` rather than the owner's PID:
/// the grant is consumed by the next window to take the foreground anyway, and it saves a
/// pipe-handle query. No such mechanism exists (or is needed) on macOS.
pub fn grant_foreground_to_owner() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
        AllowSetForegroundWindow(ASFW_ANY);
    }
}

/// The system UI font, if it is where we expect it. ImGui's built-in font stands in otherwise.
pub fn ui_font_path() -> Option<&'static Path> {
    #[cfg(windows)]
    {
        Some(Path::new(r"C:\Windows\Fonts\segoeui.ttf"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Path::new("/System/Library/Fonts/SFNS.ttf"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

/// Open the file manager with `image` selected ("Show in Explorer" / "Reveal in Finder").
/// Best-effort: a failure is logged, never fatal.
pub fn reveal_in_file_manager(image: &Path) {
    let result = {
        #[cfg(windows)]
        {
            // `raw_arg` writes the canonical `/select,"<path>"` form verbatim — Explorer's switch
            // parser wants exactly that (a normal quoted arg would wrap the whole `/select,…`
            // token and break it).
            use std::os::windows::process::CommandExt;
            let arg = format!("/select,\"{}\"", image.display());
            std::process::Command::new("explorer.exe")
                .raw_arg(arg)
                .spawn()
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open")
                .arg("-R")
                .arg(image)
                .spawn()
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let dir = image.parent().unwrap_or(image);
            std::process::Command::new("xdg-open").arg(dir).spawn()
        }
    };
    if let Err(e) = result {
        eprintln!("fire: failed to reveal {}: {e}", image.display());
    }
}

/// Put `text` on the clipboard (the "Copy Path" / "Copy File Name" actions).
pub fn copy_text_to_clipboard(text: &str) {
    #[cfg(windows)]
    {
        win_clipboard::copy_text(text);
    }
    #[cfg(target_os = "macos")]
    {
        // `pbcopy` is on every Mac; a one-shot child is simpler than an AppKit pasteboard leaf.
        use std::io::Write as _;
        let child = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn();
        match child {
            Ok(mut c) => {
                if let Some(mut stdin) = c.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                let _ = c.wait();
            }
            Err(e) => eprintln!("fire: clipboard unavailable: {e}"),
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = text;
        eprintln!("fire: clipboard is not supported on this platform");
    }
}

/// Put `image` on the clipboard as a *file* (the "Copy File" action), so a paste into the file
/// manager produces the file itself rather than its name. `CF_HDROP` on Windows, a file URL on
/// the macOS pasteboard; on any other platform there is no such concept here and it falls back to
/// copying the path as text, which is the nearest thing a paste can do with it.
pub fn copy_file_to_clipboard(image: &Path) {
    #[cfg(windows)]
    {
        win_clipboard::copy_file(image);
    }
    #[cfg(target_os = "macos")]
    {
        // Text is still the right fallback: a path that is not UTF-8, or a pasteboard that
        // refuses the write, should leave the user with *something* they can paste.
        if !mac_clipboard::copy_file(image) {
            copy_text_to_clipboard(&image.to_string_lossy());
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        copy_text_to_clipboard(&image.to_string_lossy());
    }
}

#[cfg(target_os = "macos")]
mod mac_clipboard {
    use std::path::Path;

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString, NSURL};

    /// Write `image` to the general pasteboard as a file URL — the representation Finder, Mail
    /// and the Open dialogs all read as "a file", so ⌘V in Finder copies the image rather than
    /// pasting its name. Returns whether the pasteboard took it.
    ///
    /// Unlike the text path this cannot shell out: `pbcopy` writes bytes as a string, and a file
    /// URL is a *type*, not a string that happens to start with `file:`.
    pub fn copy_file(image: &Path) -> bool {
        // NSString is UTF-8/UTF-16; a path that is not valid UTF-8 has no NSString form, and the
        // caller's text fallback is the better answer than a mangled one.
        let Some(path) = image.to_str() else {
            return false;
        };
        // SAFETY: all four calls take what they are declared to take, and the pasteboard is
        // touched from the UI thread (the actions menu runs there), which is where AppKit wants
        // it. `writeObjects:` copies what it is given; nothing outlives this call.
        unsafe {
            let url: Retained<NSURL> = NSURL::fileURLWithPath(&NSString::from_str(path));
            // `from_retained` rather than `from_ref`: an `NSArray` of protocol objects has to own
            // its elements, because a protocol alone does not promise the object is retainable.
            let writer: Retained<ProtocolObject<dyn NSPasteboardWriting>> =
                ProtocolObject::from_retained(url);
            let pasteboard = NSPasteboard::generalPasteboard();
            // Required before every write: the pasteboard's previous owner keeps its types
            // otherwise, and a stale text flavour would win over the file we are adding.
            pasteboard.clearContents();
            pasteboard.writeObjects(&NSArray::from_vec(vec![writer]))
        }
    }
}

#[cfg(windows)]
mod win_clipboard {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{GlobalFree, HWND};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::UI::Shell::DROPFILES;

    /// Clipboard format ids (stable Win32 values) not surfaced by windows-sys under the enabled
    /// features, so we define them directly.
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP: u32 = 15;

    /// Publish one clipboard format, `fill`ing a freshly allocated `HGLOBAL` of `bytes`.
    ///
    /// The ownership rule is the whole reason this exists once rather than per format: the
    /// `HGLOBAL` belongs to *us* until `SetClipboardData` succeeds, and to the clipboard the
    /// instant it does — so every failure path before that point must free it, and none after
    /// may. Best-effort throughout: a failure leaves the clipboard no worse than the
    /// `EmptyClipboard` we already issued. Opened with no owner window (null): the clipboard does
    /// not need one to accept data, and it keeps this leaf free of window handles.
    ///
    /// # Safety
    /// `fill` must not write past `bytes` from the pointer it is given.
    unsafe fn set_clipboard(format: u32, bytes: usize, fill: impl FnOnce(*mut u8)) {
        let owner: HWND = ptr::null_mut();
        if OpenClipboard(owner) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !h.is_null() {
            let base = GlobalLock(h) as *mut u8;
            if base.is_null() {
                GlobalFree(h);
            } else {
                fill(base);
                GlobalUnlock(h);
                if SetClipboardData(format, h).is_null() {
                    GlobalFree(h); // ownership didn't transfer; release it
                }
            }
        }
        CloseClipboard();
    }

    pub fn copy_text(text: &str) {
        let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = std::mem::size_of_val(utf16.as_slice());
        // SAFETY: the buffer is `utf16.len()` u16s long, exactly what we ask for and exactly what
        // we write.
        unsafe {
            set_clipboard(CF_UNICODETEXT, bytes, |base| {
                ptr::copy_nonoverlapping(utf16.as_ptr(), base as *mut u16, utf16.len());
            });
        }
    }

    /// Layout per the `DROPFILES` contract: the header, then the wide path (with its NUL), then
    /// one extra NUL ending the (single-entry) list.
    pub fn copy_file(image: &Path) {
        let path: Vec<u16> = image
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let header = std::mem::size_of::<DROPFILES>();
        let bytes = header + (path.len() + 1) * std::mem::size_of::<u16>();
        // SAFETY: `bytes` covers the header, the path and its two NULs; the writes below stay
        // inside it.
        unsafe {
            set_clipboard(CF_HDROP, bytes, |base| {
                ptr::write_bytes(base, 0, bytes); // zero the header fields + the trailing NUL
                let df = base as *mut DROPFILES;
                (*df).pFiles = header as u32; // byte offset from the header to the path list
                (*df).fWide = 1; // paths are UTF-16
                ptr::copy_nonoverlapping(path.as_ptr(), base.add(header) as *mut u16, path.len());
            });
        }
    }
}
