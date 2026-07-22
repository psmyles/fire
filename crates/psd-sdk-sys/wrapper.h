/*
 * C-ABI wrapper over the vendored psd_sdk C++ library (Molecular Matters).
 *
 * IMPORTANT: this header is deliberately C-style. It includes only <stdint.h> /
 * <stddef.h>, never <cstdint> or any C++ standard-library header. bindgen parses it
 * with clang 18, and the installed MSVC 14.44 STL hard-errors (STL1000) when clang
 * < 19 touches its headers. Keeping the bindgen surface free of STL includes sidesteps
 * that entirely. The actual psd_sdk C++ (which DOES use the STL) is compiled by MSVC
 * cl.exe via the cc crate, not by clang, so it is unaffected.
 *
 * The implementation lives in wrapper.cpp; these declarations are the FFI surface
 * bindgen turns into Rust.
 */
#ifndef FIRE_PSD_WRAPPER_H
#define FIRE_PSD_WRAPPER_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a decoded PSD document (concrete type defined C++-side). */
typedef struct fire_psd fire_psd;

/* Basic image info surfaced to Rust. */
typedef struct fire_psd_info {
    uint32_t width;
    uint32_t height;
    /* Channels the COMPOSITE actually carries, after colour conversion: 1 (gray),
     * 2 (gray+alpha), 3 (colour) or 4 (colour+alpha). Deliberately NOT the document's
     * raw channelCount, which counts spot/extra channels too — an RGB document with one
     * spot channel has channelCount 5, and reporting that made the viewer treat a file
     * WITH transparency as having none (its alpha UI keys on 2 or 4). */
    uint16_t channels;
    /* Source bits per channel: 8, 16 or 32. Also the layout of the buffer
     * fire_psd_read_merged fills: uint8_t, uint16_t or float respectively. */
    uint16_t bits_per_channel;
    /* PSD colour mode (psd::colorMode::Enum). Surfaced so Rust can tell whether the
     * embedded ICC profile still describes the pixels: for CMYK/Lab/Multichannel we
     * convert to RGB here, so the profile (a CMYK/Lab one) must NOT then be applied. */
    uint16_t color_mode;
    uint16_t reserved;
} fire_psd_info;

/* Open a PSD from an in-memory buffer. Returns NULL on failure. */
fire_psd* fire_psd_open(const uint8_t* bytes, size_t len);

/* Populate *out_info. Returns 0 on success, non-zero on error. */
int fire_psd_info_get(const fire_psd* doc, fire_psd_info* out_info);

/* Read the merged/composited image as interleaved RGBA at the document's bit depth:
 * 8 -> uint8_t, 16 -> uint16_t, 32 -> float (linear/HDR). out_len is in BYTES and must
 * be width*height*4*(bits_per_channel/8). Returns 0 on success. */
int fire_psd_read_merged(const fire_psd* doc, void* out_pixels, size_t out_len);

/* Length of the embedded ICC profile in bytes (0 if none). */
size_t fire_psd_icc_len(const fire_psd* doc);

/* Copy ICC profile bytes into out_icc (out_len from fire_psd_icc_len). 0 on success. */
int fire_psd_icc_get(const fire_psd* doc, uint8_t* out_icc, size_t out_len);

/* Free a document returned by fire_psd_open. */
void fire_psd_free(fire_psd* doc);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* FIRE_PSD_WRAPPER_H */
