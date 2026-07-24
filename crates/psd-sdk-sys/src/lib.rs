//! FFI bindings to psd_sdk (Molecular Matters, C++) via a thin `extern "C"` C-ABI
//! wrapper (`wrapper.h`/`wrapper.cpp`), plus a small safe Rust API on top.
//!
//! v1 decodes the MERGED/composited image (present when the PSD was saved with
//! "Maximize Compatibility"); layer browsing is deferred to v2 (#18).
//!
//! Safety: callers in fire-decode additionally run [`decode_psd`] inside
//! `std::panic::catch_unwind` on a decode worker, so a malformed PSD cannot take down
//! the viewer process (§6/§15). The C++ side also guards its entry points.
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

mod ffi {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

/// A decoded PSD merged image, normalized to interleaved RGBA at the document's own depth.
#[derive(Debug, Clone)]
pub struct PsdImage {
    pub width: u32,
    pub height: u32,
    /// Channels the composite carries after colour conversion: 1, 2, 3 or 4. **Not** the
    /// document's raw channel count — a PSD with spot channels has more of those than it
    /// has image channels, and reporting them hid the alpha the file really did have.
    pub channels: u16,
    /// Source bits per channel: 8, 16 or 32. Also the layout of [`Self::rgba`].
    pub bits_per_channel: u16,
    /// Interleaved RGBA, row-major, in the source depth: `u8`, native-endian `u16`, or
    /// native-endian `f32` (linear/HDR) for 8-, 16- and 32-bit documents respectively.
    pub rgba: Vec<u8>,
    /// Embedded ICC profile bytes, if present *and* still applicable to these pixels.
    /// Modes we convert out of (CMYK/Lab/Multichannel) drop it: the profile describes the
    /// source space, so applying it to the RGB we produced would be a second conversion.
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
    /// A bit depth with no defined plane layout — in practice 1-bit (Bitmap mode), whose
    /// planes psd_sdk sizes to zero bytes.
    UnsupportedDepth,
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
            PsdError::UnsupportedDepth => {
                write!(
                    f,
                    "unsupported PSD bit depth (Bitmap-mode documents are 1-bit)"
                )
            }
        }
    }
}

impl std::error::Error for PsdError {}

/// RAII guard that frees the opaque psd document on drop (incl. early returns).
struct DocHandle(*mut ffi::fire_psd);

impl Drop for DocHandle {
    fn drop(&mut self) {
        // SAFETY: pointer came from fire_psd_open and is freed exactly once.
        unsafe { ffi::fire_psd_free(self.0) };
    }
}

/// Decode a PSD's merged image from in-memory bytes into RGBA at the document's own depth.
pub fn decode_psd(bytes: &[u8]) -> Result<PsdImage, PsdError> {
    // SAFETY: bytes/len describe a valid read-only slice for the duration of the call;
    // the C++ side copies them internally. The handle is freed by DocHandle on every
    // return path.
    unsafe {
        let handle = ffi::fire_psd_open(bytes.as_ptr(), bytes.len());
        if handle.is_null() {
            return Err(PsdError::OpenFailed);
        }
        let guard = DocHandle(handle);

        let mut info = ffi::fire_psd_info {
            width: 0,
            height: 0,
            channels: 0,
            bits_per_channel: 0,
            color_mode: 0,
            reserved: 0,
        };
        if ffi::fire_psd_info_get(handle, &mut info) != 0 {
            return Err(PsdError::InfoFailed);
        }

        let (w, h) = (info.width, info.height);
        let bytes_per_sample = match info.bits_per_channel {
            8 => 1usize,
            16 => 2,
            32 => 4,
            _ => return Err(PsdError::UnsupportedDepth),
        };
        // FFI = validation boundary: reject zero/degenerate dimensions and size the buffer with
        // checked arithmetic, so a malformed header can never wrap to an undersized allocation
        // that the C++ merged-image read then overruns.
        let len = (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| n.checked_mul(bytes_per_sample))
            .filter(|&n| n != 0)
            .ok_or(PsdError::InfoFailed)?;
        let mut rgba = vec![0u8; len];
        let rc = ffi::fire_psd_read_merged(handle, rgba.as_mut_ptr().cast(), rgba.len());
        match rc {
            0 => {}
            2 => return Err(PsdError::NoMergedImage),
            other => return Err(PsdError::ReadFailed(other)),
        }

        // Modes we converted out of leave the embedded profile describing a space these
        // pixels are no longer in; applying it downstream would convert a second time.
        const CMYK: u16 = 4;
        const MULTICHANNEL: u16 = 7;
        const LAB: u16 = 9;
        let icc_applies = !matches!(info.color_mode, CMYK | MULTICHANNEL | LAB);
        let icc_len = if icc_applies {
            ffi::fire_psd_icc_len(handle)
        } else {
            0
        };
        // Cap the C++-reported ICC length before allocating against it, as the HEIF wrapper
        // does. Real profiles are far smaller; a garbage length drops the profile instead of
        // asking for an allocation that would abort the process.
        const MAX_ICC_LEN: usize = 16 * 1024 * 1024;
        let icc = if icc_len > 0 && icc_len <= MAX_ICC_LEN {
            let mut buf = vec![0u8; icc_len];
            if ffi::fire_psd_icc_get(handle, buf.as_mut_ptr(), buf.len()) == 0 {
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
            rgba,
            icc,
        })
    }
}
