/*
 * Implementation of the C-ABI wrapper declared in wrapper.h, over libheif's C API.
 * Compiled by MSVC cl.exe (via the cc crate), so it may freely include libheif headers.
 */
#include "wrapper.h"

#include <stdlib.h>
#include <string.h>

#include <libheif/heif.h>

/* Release the libheif objects acquired during a decode, in reverse order. */
static void cleanup(struct heif_image* img, struct heif_image_handle* handle,
                    struct heif_context* ctx) {
    if (img) heif_image_release(img);
    if (handle) heif_image_handle_release(handle);
    if (ctx) heif_context_free(ctx);
    heif_deinit();
}

int fire_heif_decode(const uint8_t* data, size_t len, fire_heif_image* out) {
    if (!out) return -1;
    memset(out, 0, sizeof(*out));
    if (!data || len == 0) return -2;

    /* Register the built-in decoder plugins (libde265, dav1d). Refcounted and
     * thread-safe, so concurrent decode workers calling this is fine. */
    heif_init(NULL);

    struct heif_context* ctx = heif_context_alloc();
    if (!ctx) {
        cleanup(NULL, NULL, NULL);
        return -3;
    }

    struct heif_error err =
        heif_context_read_from_memory_without_copy(ctx, data, len, NULL);
    if (err.code != heif_error_Ok) {
        cleanup(NULL, NULL, ctx);
        return err.code ? (int)err.code : -4;
    }

    struct heif_image_handle* handle = NULL;
    err = heif_context_get_primary_image_handle(ctx, &handle);
    if (err.code != heif_error_Ok || !handle) {
        cleanup(NULL, handle, ctx);
        return err.code ? (int)err.code : -5;
    }

    int bits = heif_image_handle_get_luma_bits_per_pixel(handle);
    // Clamped at *both* ends. libheif only reports 8/10/12, but the low clamp alone left the top
    // open, and the 16-bit path below derives its shifts from this: bits > 16 makes `shift`
    // negative, and a negative shift is undefined behaviour, not a large one.
    if (bits < 8) bits = 8;
    if (bits > 16) bits = 16;
    int has_alpha = heif_image_handle_has_alpha_channel(handle);

    /* 8-bit sources decode straight to interleaved RGBA8; >8-bit (HDR HEIC/AVIF)
     * decode to little-endian 16-bit RGBA, which we scale to full range below. */
    int use16 = bits > 8;
    enum heif_chroma chroma =
        use16 ? heif_chroma_interleaved_RRGGBBAA_LE : heif_chroma_interleaved_RGBA;

    struct heif_image* img = NULL;
    err = heif_decode_image(handle, &img, heif_colorspace_RGB, chroma, NULL);
    if (err.code != heif_error_Ok || !img) {
        cleanup(img, handle, ctx);
        return err.code ? (int)err.code : -6;
    }

    int w = heif_image_get_width(img, heif_channel_interleaved);
    int h = heif_image_get_height(img, heif_channel_interleaved);
    int stride = 0;
    const uint8_t* plane =
        heif_image_get_plane_readonly(img, heif_channel_interleaved, &stride);
    if (w <= 0 || h <= 0 || !plane || stride <= 0) {
        cleanup(img, handle, ctx);
        return -7;
    }

    /* Size the output with checked arithmetic, mirroring the Rust side's `checked_mul`. libheif's
     * own security limits make an overflow here unreachable in practice (it would have had to
     * allocate the source plane first), but an unchecked `w * bpp * h` that wrapped would hand
     * `malloc` an undersized length and the row loop below would then run straight off the end of
     * it. The bound is the same 4 GiB budget fire-decode applies to every other backend. */
    size_t bpp = use16 ? 8u : 4u; /* bytes per RGBA pixel */
    const size_t MAX_OUT_BYTES = (size_t)4 << 30;
    if ((size_t)w > MAX_OUT_BYTES / bpp) {
        cleanup(img, handle, ctx);
        return -8;
    }
    size_t row_bytes = (size_t)w * bpp;
    if ((size_t)h > MAX_OUT_BYTES / row_bytes) {
        cleanup(img, handle, ctx);
        return -8;
    }
    size_t out_len = row_bytes * (size_t)h;
    uint8_t* pixels = (uint8_t*)malloc(out_len);
    if (!pixels) {
        cleanup(img, handle, ctx);
        return -8;
    }

    if (!use16) {
        /* Copy each row tightly, dropping any stride padding. */
        for (int y = 0; y < h; ++y) {
            memcpy(pixels + (size_t)y * row_bytes, plane + (size_t)y * stride, row_bytes);
        }
    } else {
        /* Scale right-aligned `bits`-bit samples up to full 0..65535 by bit
         * replication (max value maps to 65535 exactly). Samples are native-endian
         * u16 on this LE target, matching the _LE chroma requested above. */
        int shift = 16 - bits;     /* 6 for 10-bit, 4 for 12-bit */
        int rshift = bits - shift; /* replicate the top bits into the low ones */
        for (int y = 0; y < h; ++y) {
            const uint16_t* src = (const uint16_t*)(plane + (size_t)y * stride);
            uint16_t* dst = (uint16_t*)(pixels + (size_t)y * row_bytes);
            size_t n = (size_t)w * 4u; /* 4 channels per pixel */
            for (size_t i = 0; i < n; ++i) {
                uint16_t v = src[i];
                dst[i] = (uint16_t)((v << shift) | (v >> rshift));
            }
        }
    }

    /* Embedded ICC (raw) profile, if any. A non-ICC (nclx) profile reports size 0. */
    uint8_t* icc = NULL;
    size_t icc_len = heif_image_handle_get_raw_color_profile_size(handle);
    if (icc_len > 0) {
        icc = (uint8_t*)malloc(icc_len);
        if (icc) {
            struct heif_error ie = heif_image_handle_get_raw_color_profile(handle, icc);
            if (ie.code != heif_error_Ok) {
                free(icc);
                icc = NULL;
                icc_len = 0;
            }
        } else {
            icc_len = 0;
        }
    } else {
        icc_len = 0;
    }

    out->width = (uint32_t)w;
    out->height = (uint32_t)h;
    out->bit_depth = (uint8_t)bits;
    out->has_alpha = (uint8_t)(has_alpha ? 1 : 0);
    out->is_16bit = (uint8_t)(use16 ? 1 : 0);
    out->pixels = pixels;
    out->pixels_len = out_len;
    out->icc = icc;
    out->icc_len = icc_len;

    cleanup(img, handle, ctx);
    return 0;
}

void fire_heif_image_free(fire_heif_image* img) {
    if (!img) return;
    if (img->pixels) free(img->pixels);
    if (img->icc) free(img->icc);
    memset(img, 0, sizeof(*img));
}
