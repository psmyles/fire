//! FFI bindings to libheif (HEIC/HEIF via libde265, AVIF via dav1d) through a thin
//! `extern "C"` C-ABI wrapper (`wrapper.h`/`wrapper.c`), plus a small safe Rust API.
//!
//! v1 decodes the primary image only (no image sequences / multi-frame, no depth/aux
//! images). 8-bit sources come back as interleaved RGBA8; >8-bit (HDR) sources come back
//! as interleaved RGBA16 scaled to full range. Any embedded ICC profile is surfaced.
//!
//! Safety: callers in fire-decode run [`decode_heif`] inside `std::panic::catch_unwind`
//! on a decode worker, so a malformed file cannot take down the viewer process.
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

mod ffi {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

/// A decoded HEIF/AVIF primary image, normalized to interleaved RGBA.
#[derive(Debug, Clone)]
pub struct HeifImage {
    pub width: u32,
    pub height: u32,
    /// Source luma bits per channel (8 / 10 / 12) — for status display.
    pub bit_depth: u8,
    /// Whether the source carried an alpha channel (pixels are RGBA either way).
    pub has_alpha: bool,
    /// `true` => `pixels` is interleaved 16-bit RGBA (native-endian u16, full 0..65535
    /// range); `false` => interleaved 8-bit RGBA.
    pub is_16bit: bool,
    /// Interleaved RGBA, row-major.
    pub pixels: Vec<u8>,
    /// Embedded ICC profile bytes, if present.
    pub icc: Option<Vec<u8>>,
}

/// A libheif decode failure, carrying the wrapper/libheif error code.
#[derive(Debug)]
pub struct HeifError(pub i32);

impl std::fmt::Display for HeifError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "libheif failed to decode the image (code {})", self.0)
    }
}

impl std::error::Error for HeifError {}

/// Decode the primary image of a HEIC/HEIF/AVIF from in-memory bytes.
pub fn decode_heif(bytes: &[u8]) -> Result<HeifImage, HeifError> {
    // SAFETY: `bytes` is a valid read-only slice for the duration of the call; libheif
    // reads it without copying but only within the call. On success the wrapper hands us
    // malloc'd buffers which we copy out and then free via fire_heif_image_free (so alloc
    // and free stay on the same /MD CRT heap).
    unsafe {
        let mut out: ffi::fire_heif_image = std::mem::zeroed();
        let rc = ffi::fire_heif_decode(bytes.as_ptr(), bytes.len(), &mut out);
        if rc != 0 {
            return Err(HeifError(rc));
        }
        // Defensive: a success code must come with a pixel buffer.
        if out.pixels.is_null() || out.pixels_len == 0 {
            ffi::fire_heif_image_free(&mut out);
            return Err(HeifError(-100));
        }
        // FFI = validation boundary: a success code must also come with non-zero dimensions.
        if out.width == 0 || out.height == 0 {
            ffi::fire_heif_image_free(&mut out);
            return Err(HeifError(-101));
        }

        let pixels = std::slice::from_raw_parts(out.pixels, out.pixels_len).to_vec();
        // Cap the C-provided ICC length before trusting it to build a slice. Real profiles are
        // far smaller; a garbage length just drops the profile rather than reading out of bounds.
        const MAX_ICC_LEN: usize = 16 * 1024 * 1024;
        let icc = if !out.icc.is_null() && out.icc_len > 0 && out.icc_len <= MAX_ICC_LEN {
            Some(std::slice::from_raw_parts(out.icc, out.icc_len).to_vec())
        } else {
            None
        };

        let img = HeifImage {
            width: out.width,
            height: out.height,
            bit_depth: out.bit_depth,
            has_alpha: out.has_alpha != 0,
            is_16bit: out.is_16bit != 0,
            pixels,
            icc,
        };
        ffi::fire_heif_image_free(&mut out);
        Ok(img)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Garbage input must come back as a clean `Err` (and this test forces the linker to
    /// pull in and link the vendored static stack, proving it links).
    #[test]
    fn garbage_is_err_not_panic() {
        let r = decode_heif(b"not a heif file at all");
        assert!(r.is_err());
    }

    /// Empty input is rejected by the wrapper's length guard.
    #[test]
    fn empty_is_err() {
        assert!(decode_heif(&[]).is_err());
    }
}
