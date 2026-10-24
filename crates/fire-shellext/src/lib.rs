//! `fire_shellext.dll` — Fire's Explorer thumbnail provider.
//!
//! An in-process COM server with one class, [`fire_thumbs::CLSID`], implementing
//! `IInitializeWithStream` + `IThumbnailProvider`. Explorer hands it the file as a stream, asks for
//! a `cx`-pixel thumbnail, and gets back a 32-bit DIB section. Everything between is the viewer's:
//! `fire_decode::decode` reads the file exactly as the viewer would (same sniffing, same EXIF
//! orientation, same ICC transform), and `fire_thumbs::render` draws it the way the viewer's shader
//! does — see that module for the colour pipeline.
//!
//! **Stream, not file path.** `IInitializeWithStream` is the initializer that lets Explorer run the
//! provider *out of process*, in its COM surrogate (`dllhost.exe`): a file that crashes a decoder
//! takes down that surrogate and Explorer starts another. The registration (`fire_thumbs::registry`)
//! leaves process isolation on for that reason. What a stream loses is the file name — and camera
//! raws are routed partly by extension — so the name is recovered from `IStream::Stat`, which the
//! shell's file streams answer.
//!
//! **Nothing unwinds out.** A panic crossing an `extern "system"` boundary aborts the host, so the
//! whole extraction runs under `catch_unwind` (the decode crate already wraps its FFI decoders the
//! same way, and the release profile keeps `panic = "unwind"` for it). Every failure — unreadable,
//! too large, not an image — is `E_FAIL`, which Explorer answers by showing the file's icon.
//!
//! **Settings are read per thumbnail.** `[thumbnails]` comes out of `config.toml` on every call; a
//! surrogate lives for minutes and the read is a few kilobytes, so a changed setting applies to the
//! very next thumbnail Explorer asks for.

#![cfg(windows)]

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use fire_thumbs::render::Thumbnail;
use fire_thumbs::ThumbnailsCfg;
use windows::core::{implement, Interface, Ref, Result, BOOL, GUID, HRESULT};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, ERROR_ALREADY_INITIALIZED,
    ERROR_FILE_TOO_LARGE, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, E_UNEXPECTED, S_FALSE,
    S_OK,
};
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::System::Com::{
    CoTaskMemFree, IClassFactory, IClassFactory_Impl, IStream, STATFLAG_DEFAULT, STATSTG,
    STREAM_SEEK_SET,
};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IThumbnailProvider, IThumbnailProvider_Impl, WTSAT_ARGB, WTSAT_RGB, WTS_ALPHATYPE,
};

/// Live objects plus `LockServer` locks: what `DllCanUnloadNow` asks about.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// One count in [`LIVE`] for as long as the object holding it lives.
struct Live;

impl Live {
    fn new() -> Self {
        LIVE.fetch_add(1, Ordering::SeqCst);
        Live
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

// --- the provider ------------------------------------------------------------------------------

#[implement(IInitializeWithStream, IThumbnailProvider)]
struct Provider {
    /// The file, from `Initialize`. Behind a mutex only because the trait methods take `&self`;
    /// the shell calls one object from one thread.
    stream: Mutex<Option<IStream>>,
    _live: Live,
}

impl IInitializeWithStream_Impl for Provider_Impl {
    fn Initialize(&self, pstream: Ref<'_, IStream>, _grfmode: u32) -> Result<()> {
        let stream = pstream.ok()?.clone();
        let mut slot = self.stream.lock().map_err(|_| E_UNEXPECTED)?;
        // The documented answer to a second Initialize.
        if slot.is_some() {
            return Err(ERROR_ALREADY_INITIALIZED.to_hresult().into());
        }
        *slot = Some(stream);
        Ok(())
    }
}

impl IThumbnailProvider_Impl for Provider_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> Result<()> {
        if phbmp.is_null() || pdwalpha.is_null() {
            return Err(E_POINTER.into());
        }
        let stream = self
            .stream
            .lock()
            .map_err(|_| E_UNEXPECTED)?
            .clone()
            .ok_or(E_UNEXPECTED)?;
        let thumb =
            catch_unwind(AssertUnwindSafe(|| thumbnail(&stream, cx))).map_err(|_| E_FAIL)??;
        let bitmap = to_bitmap(&thumb)?;
        // SAFETY: both checked non-null above; the caller owns the bitmap from here.
        unsafe {
            *phbmp = bitmap;
            *pdwalpha = if thumb.has_alpha {
                WTSAT_ARGB
            } else {
                WTSAT_RGB
            };
        }
        Ok(())
    }
}

/// Read, decode and draw. Every error is the reason Explorer falls back to the icon.
fn thumbnail(stream: &IStream, cx: u32) -> Result<Thumbnail> {
    let cfg = ThumbnailsCfg::load();
    let (bytes, ext) = read_all(stream, cfg.max_file_bytes())?;
    let img = fire_decode::decode(
        &bytes,
        ext.as_deref(),
        &fire_decode::DecodeOptions::default(),
    )
    .map_err(|_| E_FAIL)?;
    // The file and the decoded image can each be large; only one of them need be alive while the
    // thumbnail is drawn.
    drop(bytes);
    fire_thumbs::render::render(&img, cx, &cfg).ok_or_else(|| E_FAIL.into())
}

/// The bytes Explorer reads per round trip. The stream may be a proxy into Explorer's process, so
/// a read is a cross-process call: few large ones, not many small ones.
const CHUNK: usize = 8 << 20;

/// The whole stream, and the file's extension (lower-case) when the stream knows its name. Refused
/// up front when it is larger than `limit`, before a byte is read.
fn read_all(stream: &IStream, limit: Option<u64>) -> Result<(Vec<u8>, Option<String>)> {
    let mut st = STATSTG::default();
    // SAFETY: `st` is a valid out-struct; the name it may return is ours to free, and is freed here.
    let name = unsafe {
        stream.Stat(&mut st, STATFLAG_DEFAULT)?;
        let name = (!st.pwcsName.is_null())
            .then(|| st.pwcsName.to_string().ok())
            .flatten();
        CoTaskMemFree(Some(st.pwcsName.0 as *const c_void));
        name
    };
    let ext = name
        .as_deref()
        .and_then(|n| Path::new(n).extension())
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);

    let too_large = || -> windows::core::Error { ERROR_FILE_TOO_LARGE.to_hresult().into() };
    if limit.is_some_and(|l| st.cbSize > l) {
        return Err(too_large());
    }
    let mut buf = Vec::new();
    let size = usize::try_from(st.cbSize).map_err(|_| too_large())?;
    // A size from a file header is not trusted with an allocation that could abort the process:
    // `try_reserve` turns a refusal into an error Explorer can survive.
    buf.try_reserve_exact(size).map_err(|_| E_OUTOFMEMORY)?;

    // SAFETY: plain stream calls; `Read` writes at most `want` bytes into the buffer it is given,
    // and only the `got` it reports are then counted as initialized.
    unsafe {
        stream.Seek(0, STREAM_SEEK_SET, None)?;
        loop {
            let spare = buf.spare_capacity_mut();
            let mut got = 0u32;
            if spare.is_empty() {
                // Everything Stat promised is in. Look for more through a small buffer rather than
                // growing `buf`, which for a large file would mean doubling it — a copy of the
                // whole thing — only to find the end of the stream.
                let mut probe = [0u8; 4096];
                stream
                    .Read(
                        probe.as_mut_ptr().cast(),
                        probe.len() as u32,
                        Some(&mut got),
                    )
                    .ok()?;
                if got == 0 {
                    break;
                }
                // A stream longer than its own Stat said: keep reading, a chunk at a time.
                buf.try_reserve(CHUNK).map_err(|_| E_OUTOFMEMORY)?;
                buf.extend_from_slice(&probe[..got as usize]);
            } else {
                let want = spare.len().min(CHUNK) as u32;
                stream
                    .Read(spare.as_mut_ptr().cast(), want, Some(&mut got))
                    .ok()?;
                if got == 0 {
                    break;
                }
                buf.set_len(buf.len() + got as usize);
            }
            // A stream longer than its own Stat said is held to the same limit.
            if limit.is_some_and(|l| buf.len() as u64 > l) {
                return Err(too_large());
            }
        }
    }
    Ok((buf, ext))
}

/// The thumbnail as a top-down 32-bit DIB section, which is what the shell takes.
fn to_bitmap(t: &Thumbnail) -> Result<HBITMAP> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: t.width as i32,
            // Negative: rows top-down, the order `bgra` is in.
            biHeight: -(t.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `bmi` describes a `width × height` 32-bit DIB, which is exactly `bgra.len()` bytes;
    // the section is ours until handed to the caller.
    unsafe {
        let bitmap = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        if bits.is_null() {
            let _ = DeleteObject(bitmap.into());
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(t.bgra.as_ptr(), bits.cast::<u8>(), t.bgra.len());
        Ok(bitmap)
    }
}

// --- the class factory and the exports ---------------------------------------------------------

#[implement(IClassFactory)]
struct Factory {
    _live: Live,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<'_, windows::core::IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        if ppvobject.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked non-null.
        unsafe { *ppvobject = std::ptr::null_mut() };
        if !punkouter.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let provider: IThumbnailProvider = Provider {
            stream: Mutex::new(None),
            _live: Live::new(),
        }
        .into();
        // SAFETY: both pointers checked non-null; `query` AddRefs what it hands out.
        unsafe { provider.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        if flock.as_bool() {
            LIVE.fetch_add(1, Ordering::SeqCst);
        } else {
            LIVE.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// COM's entry point: the class factory for Fire's one class.
///
/// # Safety
/// Called by COM with valid (or null, which is answered with an error) pointers.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if rclsid.is_null() || riid.is_null() {
        return E_INVALIDARG;
    }
    if *rclsid != GUID::from_u128(fire_thumbs::CLSID) {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory { _live: Live::new() }.into();
    factory.query(riid, ppv)
}

/// Whether COM may unload the DLL: only with no object alive and no server lock held.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}
