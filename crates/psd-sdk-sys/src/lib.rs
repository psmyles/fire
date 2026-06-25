//! FFI bindings to psd_sdk (Molecular Matters, C++) via a thin `extern "C"` C-ABI
//! wrapper (`wrapper.h`/`wrapper.cpp`), plus a small safe Rust API on top.
//!
//! v1 decodes the MERGED/composited image (present when the PSD was saved with
//! "Maximize Compatibility"); layer browsing is deferred to v2 (#18).
//!
//! Safety: callers in texview-decode additionally run [`decode_psd`] inside
//! `std::panic::catch_unwind` on a decode worker, so a malformed PSD cannot take down
//! the resident daemon (§6/§15). The C++ side also guards its entry points.
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

mod ffi {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

/// A decoded PSD merged image, normalized to 8-bit RGBA.
#[derive(Debug, Clone)]
pub struct PsdImage {
    pub width: u32,
    pub height: u32,
    /// Source channel count (incl. extra alpha channels) — for status display.
    pub channels: u16,
    /// Source bits per channel (8/16/32) — for status display.
    pub bits_per_channel: u16,
    /// Interleaved RGBA, 8-bit, row-major.
    pub rgba8: Vec<u8>,
    /// Embedded ICC profile bytes, if present.
    pub icc: Option<Vec<u8>>,
}

/// Why a PSD decode failed.
#[derive(Debug)]
pub enum PsdError {
    /// psd_sdk could not parse the file / out of memory.
    OpenFailed,
    /// Header info could not be read.
    InfoFailed,
    /// No merged image — the PSD was saved without "Maximize Compatibility".
    NoMergedImage,
    /// Unexpected non-zero return from the merged-image read (code).
    ReadFailed(i32),
}

impl std::fmt::Display for PsdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PsdError::OpenFailed => write!(f, "psd_sdk failed to open the document"),
            PsdError::InfoFailed => write!(f, "could not read PSD header info"),
            PsdError::NoMergedImage => write!(
                f,
                "PSD has no composited image (re-save with Maximize Compatibility)"
            ),
            PsdError::ReadFailed(c) => write!(f, "PSD merged-image read failed (code {c})"),
        }
    }
}

impl std::error::Error for PsdError {}

/// RAII guard that frees the opaque psd document on drop (incl. early returns).
struct DocHandle(*mut ffi::texview_psd);

impl Drop for DocHandle {
    fn drop(&mut self) {
        // SAFETY: pointer came from texview_psd_open and is freed exactly once.
        unsafe { ffi::texview_psd_free(self.0) };
    }
}

/// Decode a PSD's merged image from in-memory bytes into 8-bit RGBA.
pub fn decode_psd(bytes: &[u8]) -> Result<PsdImage, PsdError> {
    // SAFETY: bytes/len describe a valid read-only slice for the duration of the call;
    // the C++ side copies them internally. The handle is freed by DocHandle on every
    // return path.
    unsafe {
        let handle = ffi::texview_psd_open(bytes.as_ptr(), bytes.len());
        if handle.is_null() {
            return Err(PsdError::OpenFailed);
        }
        let guard = DocHandle(handle);

        let mut info = ffi::texview_psd_info {
            width: 0,
            height: 0,
            channels: 0,
            bits_per_channel: 0,
        };
        if ffi::texview_psd_info_get(handle, &mut info) != 0 {
            return Err(PsdError::InfoFailed);
        }

        let (w, h) = (info.width, info.height);
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        let rc = ffi::texview_psd_read_merged_rgba8(handle, rgba.as_mut_ptr(), rgba.len());
        match rc {
            0 => {}
            2 => return Err(PsdError::NoMergedImage),
            other => return Err(PsdError::ReadFailed(other)),
        }

        let icc_len = ffi::texview_psd_icc_len(handle);
        let icc = if icc_len > 0 {
            let mut buf = vec![0u8; icc_len];
            if ffi::texview_psd_icc_get(handle, buf.as_mut_ptr(), buf.len()) == 0 {
                Some(buf)
            } else {
                None
            }
        } else {
            None
        };

        drop(guard); // explicit: free the document now that data is copied out
        Ok(PsdImage {
            width: w,
            height: h,
            channels: info.channels,
            bits_per_channel: info.bits_per_channel,
            rgba8: rgba,
            icc,
        })
    }
}
