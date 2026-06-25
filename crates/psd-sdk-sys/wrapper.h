/*
 * C-ABI wrapper over the (to-be-vendored, Phase 2) psd_sdk C++ library.
 *
 * IMPORTANT: this header is deliberately C-style. It includes only <stdint.h> /
 * <stddef.h>, never <cstdint> or any C++ standard-library header. bindgen parses it
 * with clang 18, and the installed MSVC 14.44 STL hard-errors (STL1000) when clang
 * < 19 touches its headers. Keeping the bindgen surface free of STL includes sidesteps
 * that entirely. The actual psd_sdk C++ (which DOES use the STL) is compiled by MSVC
 * cl.exe via the cc crate, not by clang, so it is unaffected.
 *
 * Phase 0: these are forward declarations only — the C++ implementation lands in
 * Phase 2. The bindings compile now, proving the cargo -> bindgen -> libclang chain.
 */
#ifndef TEXVIEW_PSD_WRAPPER_H
#define TEXVIEW_PSD_WRAPPER_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a decoded PSD document (concrete type defined C++-side). */
typedef struct texview_psd texview_psd;

/* Basic image info surfaced to Rust. */
typedef struct texview_psd_info {
    uint32_t width;
    uint32_t height;
    uint16_t channels;
    uint16_t bits_per_channel;
} texview_psd_info;

/* Open a PSD from an in-memory buffer. Returns NULL on failure. */
texview_psd* texview_psd_open(const uint8_t* bytes, size_t len);

/* Populate *out_info. Returns 0 on success, non-zero on error. */
int texview_psd_info_get(const texview_psd* doc, texview_psd_info* out_info);

/* Read the merged/composited image as 8-bit RGBA into out_pixels
 * (must be width*height*4 bytes). Returns 0 on success. */
int texview_psd_read_merged_rgba8(const texview_psd* doc, uint8_t* out_pixels, size_t out_len);

/* Length of the embedded ICC profile in bytes (0 if none). */
size_t texview_psd_icc_len(const texview_psd* doc);

/* Copy ICC profile bytes into out_icc (out_len from texview_psd_icc_len). 0 on success. */
int texview_psd_icc_get(const texview_psd* doc, uint8_t* out_icc, size_t out_len);

/* Free a document returned by texview_psd_open. */
void texview_psd_free(texview_psd* doc);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* TEXVIEW_PSD_WRAPPER_H */
