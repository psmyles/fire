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
    uint16_t channels;
    uint16_t bits_per_channel;
} fire_psd_info;

/* Open a PSD from an in-memory buffer. Returns NULL on failure. */
fire_psd* fire_psd_open(const uint8_t* bytes, size_t len);

/* Populate *out_info. Returns 0 on success, non-zero on error. */
int fire_psd_info_get(const fire_psd* doc, fire_psd_info* out_info);

/* Read the merged/composited image as 8-bit RGBA into out_pixels
 * (must be width*height*4 bytes). Returns 0 on success. */
int fire_psd_read_merged_rgba8(const fire_psd* doc, uint8_t* out_pixels, size_t out_len);

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
