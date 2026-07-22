// C-ABI implementation over psd_sdk (Molecular Matters, C++). Compiled by MSVC cl.exe
// via the cc crate (NOT by clang/bindgen), so using the C++ STL here is fine — the
// clang-18/MSVC-STL STL1000 constraint only applies to bindgen's parse of wrapper.h,
// which stays C-style.
//
// v1 scope (#18): the MERGED/composited image only (present when the PSD was saved with
// "Maximize Compatibility"). Layer browsing is deferred to v2.

#include "wrapper.h"

#include "Psd.h"
#include "PsdMallocAllocator.h"
#include "PsdAllocator.h"
#include "PsdFile.h"
#include "PsdDocument.h"
#include "PsdParseDocument.h"
#include "PsdColorMode.h"
#include "PsdPlanarImage.h"
#include "PsdImageDataSection.h"
#include "PsdParseImageDataSection.h"
#include "PsdImageResourcesSection.h"
#include "PsdParseImageResourcesSection.h"
#include "PsdColorModeDataSection.h"
#include "PsdParseColorModeDataSection.h"

#include <cmath>
#include <cstring>
#include <vector>
#include <new>

namespace {

// psd_sdk reads exclusively through the abstract File interface (async by contract).
// We back it with an in-memory buffer and serve reads synchronously.
class MemoryFile : public psd::File {
public:
    MemoryFile(psd::Allocator* allocator, const uint8_t* data, size_t size)
        : psd::File(allocator), m_data(data), m_size(size), m_shortRead(false) {}

    // Whether any read ran past end-of-file, i.e. the document is truncated. psd_sdk has no way
    // to tell us a parse went off the end (it does not check the ReadOperation we hand back, and
    // its offsets come from the file's own headers), so the caller checks this after parsing and
    // rejects the document. See DoRead.
    bool HadShortRead(void) const { return m_shortRead; }

private:
    bool DoOpenRead(const wchar_t*) PSD_OVERRIDE { return true; }
    bool DoOpenWrite(const wchar_t*) PSD_OVERRIDE { return false; }
    bool DoClose(void) PSD_OVERRIDE { return true; }

    ReadOperation DoRead(void* buffer, uint32_t count, uint64_t position) PSD_OVERRIDE {
        if (position > m_size) {
            m_shortRead = true;
            std::memset(buffer, 0, count);
            return nullptr;
        }
        const uint64_t available = m_size - position;
        const uint32_t toCopy = (count <= available) ? count : static_cast<uint32_t>(available);
        std::memcpy(buffer, m_data + position, toCopy);
        // A truncated file cannot be served in full. The buffer psd_sdk handed us is raw malloc'd
        // memory, so the tail we did not fill would otherwise stay *uninitialized* — and psd_sdk,
        // believing the read succeeded, would sample it straight into the composite, painting
        // whatever the heap happened to hold into the image. Zero the remainder and record the
        // truncation so fire_psd_open can refuse the document outright.
        if (toCopy < count) {
            std::memset(static_cast<uint8_t*>(buffer) + toCopy, 0, count - toCopy);
            m_shortRead = true;
        }
        // Non-null sentinel: the read already completed; WaitForRead just acknowledges it.
        return reinterpret_cast<ReadOperation>(1);
    }
    bool DoWaitForRead(ReadOperation&) PSD_OVERRIDE { return true; }

    WriteOperation DoWrite(const void*, uint32_t, uint64_t) PSD_OVERRIDE { return nullptr; }
    bool DoWaitForWrite(WriteOperation&) PSD_OVERRIDE { return true; }

    uint64_t DoGetSize(void) const PSD_OVERRIDE { return m_size; }

    const uint8_t* m_data;
    size_t m_size;
    bool m_shortRead;
};

// Size guard, mirroring fire-decode's MAX_DECODE_DIM / MAX_DECODE_BYTES.
//
// It has to live here, not in Rust: ParseImageDataSection below allocates psd_sdk's planar
// channel buffers straight from the dimensions in the file header, which is *before* Rust ever
// sees a dimension it could validate. A 30-byte PSD claiming 60000x60000 would ask psd_sdk for
// ~10 GB, and an allocation that fails aborts the process — nothing on the Rust side, not even
// catch_unwind, can intervene. Refuse it at the point of allocation instead.
const uint64_t MAX_PSD_DIM = 131072;              // per axis
const uint64_t MAX_PSD_BYTES = 4ull << 30;        // 4 GiB of planar channel data

bool psd_size_is_sane(const psd::Document* document) {
    const uint64_t w = document->width;
    const uint64_t h = document->height;
    if (w == 0 || h == 0 || w > MAX_PSD_DIM || h > MAX_PSD_DIM) {
        return false;
    }
    const uint64_t channels = document->channelCount ? document->channelCount : 1;
    const uint64_t bytesPerSample = (document->bitsPerChannel + 7u) / 8u;
    // w*h <= 2^34 and channels/bytesPerSample are small, so this cannot overflow 64 bits.
    return w * h * channels * bytesPerSample <= MAX_PSD_BYTES;
}

// Everything parsed for one document, owned behind a single opaque handle.
struct Doc {
    psd::MallocAllocator allocator;
    std::vector<uint8_t> bytes;      // owned copy; MemoryFile points into this
    MemoryFile* file = nullptr;
    psd::Document* document = nullptr;
    psd::ImageDataSection* imageData = nullptr;
    psd::ImageResourcesSection* resources = nullptr;
    psd::ColorModeDataSection* colorModeData = nullptr;  // INDEXED palette lives here
};

// Only these depths have a defined plane layout. 1-bit (BITMAP mode) must be refused
// rather than guessed at: psd_sdk sizes its planes with `bitsPerChannel / 8u`, which is
// *zero* for 1 bit, so every plane is a zero-byte allocation. Sampling one would read
// straight off the end of the heap.
inline bool bits_are_supported(unsigned int bits) {
    return bits == 8 || bits == 16 || bits == 32;
}

// Read one channel's value at planar index `i`, normalized to 0..1 by source bit depth.
//
// 8- and 16-bit samples are display-encoded (sRGB) and land in 0..1. 32-bit samples are
// linear and deliberately NOT clamped: that is the HDR path, and values above 1.0 are
// signal, not error.
inline float sample_norm(const void* data, unsigned int bits, size_t i) {
    if (bits == 8) {
        return static_cast<const uint8_t*>(data)[i] * (1.0f / 255.0f);
    }
    if (bits == 16) {
        // Photoshop stores 16-bit samples as 15-bit+1 integers in the range 0...32768 —
        // see the comment in vendor/Psd/PsdParseImageDataSection.cpp. Treating them as
        // full-range 0..65535 (an `x >> 8` narrowing does exactly that) renders every
        // 16-bit document at HALF BRIGHTNESS: white, 32768, comes out 128.
        const uint16_t v = static_cast<const uint16_t*>(data)[i];
        return (v >= 32768u) ? 1.0f : v * (1.0f / 32768.0f);
    }
    return static_cast<const float*>(data)[i];
}

// sRGB transfer function, for the colour spaces we convert through linear light (Lab).
inline float linear_to_srgb(float c) {
    if (c <= 0.0f) return 0.0f;
    if (c >= 1.0f) return 1.0f;
    return (c <= 0.0031308f) ? (12.92f * c) : (1.055f * std::pow(c, 1.0f / 2.4f) - 0.055f);
}

// How many of a document's channels carry COLOUR, i.e. where the alpha channel starts.
// Everything past this index is alpha (index colorChannels) then spot/extra channels.
inline unsigned int color_channel_count(unsigned int colorMode, unsigned int channelCount) {
    switch (colorMode) {
        case psd::colorMode::BITMAP:
        case psd::colorMode::GRAYSCALE:
        case psd::colorMode::INDEXED:
        case psd::colorMode::DUOTONE:
            return 1;
        case psd::colorMode::RGB:
        case psd::colorMode::LAB:
            return 3;
        case psd::colorMode::CMYK:
            return 4;
        case psd::colorMode::MULTICHANNEL:
        default:
            // Every plane is its own ink; there is no alpha to find.
            return channelCount;
    }
}

// CIE L*a*b* (PSD's is D50-referred) to display-encoded sRGB. l/a/b arrive as the raw
// samples normalized to 0..1, i.e. L* = l*100 and a*/b* = (a|b)*255 - 128.
inline void lab_to_srgb(float l, float a, float b, float* out) {
    const float L = l * 100.0f;
    const float A = a * 255.0f - 128.0f;
    const float B = b * 255.0f - 128.0f;

    const float fy = (L + 16.0f) / 116.0f;
    const float fx = fy + A / 500.0f;
    const float fz = fy - B / 200.0f;
    // f⁻¹, with the linear segment below the 6/29 knee.
    const float d = 6.0f / 29.0f;
    const float finv[3] = {
        (fx > d) ? fx * fx * fx : 3.0f * d * d * (fx - 4.0f / 29.0f),
        (fy > d) ? fy * fy * fy : 3.0f * d * d * (fy - 4.0f / 29.0f),
        (fz > d) ? fz * fz * fz : 3.0f * d * d * (fz - 4.0f / 29.0f),
    };
    // D50 reference white (PSD Lab is D50-referred, not D65).
    const float X = 0.96422f * finv[0];
    const float Y = 1.00000f * finv[1];
    const float Z = 0.82521f * finv[2];
    // XYZ(D50) -> linear sRGB, Bradford-adapted.
    out[0] = linear_to_srgb(3.1338561f * X - 1.6168667f * Y - 0.4906146f * Z);
    out[1] = linear_to_srgb(-0.9787684f * X + 1.9161415f * Y + 0.0334540f * Z);
    out[2] = linear_to_srgb(0.0719453f * X - 0.2289914f * Y + 1.4052427f * Z);
}

// Write one normalized sample into the caller's buffer in the document's own depth.
// 8/16-bit clamp (they are display-encoded); 32-bit float passes through, HDR intact.
inline void store_sample(void* out, unsigned int bits, size_t idx, float v) {
    if (bits == 8) {
        const float c = (v < 0.0f) ? 0.0f : (v > 1.0f ? 1.0f : v);
        static_cast<uint8_t*>(out)[idx] = static_cast<uint8_t>(c * 255.0f + 0.5f);
    } else if (bits == 16) {
        const float c = (v < 0.0f) ? 0.0f : (v > 1.0f ? 1.0f : v);
        static_cast<uint16_t*>(out)[idx] = static_cast<uint16_t>(c * 65535.0f + 0.5f);
    } else {
        static_cast<float*>(out)[idx] = v;
    }
}

} // namespace

struct fire_psd {
    Doc d;
};

extern "C" {

fire_psd* fire_psd_open(const uint8_t* bytes, size_t len) {
    if (!bytes || len == 0) {
        return nullptr;
    }
    fire_psd* handle = new (std::nothrow) fire_psd();
    if (!handle) {
        return nullptr;
    }
    // A malformed PSD must never unwind a C++ exception across the FFI boundary into
    // Rust (that is UB). Catch everything and surface it as a null handle.
    try {
        Doc& d = handle->d;
        d.bytes.assign(bytes, bytes + len);
        d.file = new (std::nothrow) MemoryFile(&d.allocator, d.bytes.data(), d.bytes.size());
        if (!d.file) {
            fire_psd_free(handle);
            return nullptr;
        }
        d.file->OpenRead(L"memory");
        d.document = psd::CreateDocument(d.file, &d.allocator);
        if (!d.document) {
            fire_psd_free(handle);
            return nullptr;
        }
        // Checked before anything is sized from the header (see psd_size_is_sane).
        if (!psd_size_is_sane(d.document)) {
            fire_psd_free(handle);
            return nullptr;
        }
        // Refuse depths with no defined plane layout *before* ParseImageDataSection sizes
        // anything from them. At 1 bit (BITMAP mode) psd_sdk's `bitsPerChannel / 8u` is
        // zero, so it would allocate zero-byte planes that any later sample walks off.
        if (!bits_are_supported(d.document->bitsPerChannel)) {
            fire_psd_free(handle);
            return nullptr;
        }
        // Merged image is only present when the PSD was saved with Maximize Compatibility.
        if (d.document->imageDataSection.length != 0) {
            d.imageData = psd::ParseImageDataSection(d.document, d.file, &d.allocator);
        }
        // INDEXED documents keep their 256-entry palette here; without it the composite's
        // single plane is a table of indices, and rendering it as grey is noise.
        if (d.document->colorModeDataSection.length != 0) {
            d.colorModeData = psd::ParseColorModeDataSection(d.document, d.file, &d.allocator);
        }
        // Image resources carry the embedded ICC profile (best-effort).
        if (d.document->imageResourcesSection.length != 0) {
            d.resources = psd::ParseImageResourcesSection(d.document, d.file, &d.allocator);
        }
        // A truncated document parses "successfully" — psd_sdk follows the header's own offsets
        // and never learns it ran off the end — so the composite would be built from the zeros we
        // substituted for the missing bytes. Refuse it rather than display a half-invented image.
        if (d.file->HadShortRead()) {
            fire_psd_free(handle);
            return nullptr;
        }
        return handle;
    } catch (...) {
        fire_psd_free(handle);
        return nullptr;
    }
}

int fire_psd_info_get(const fire_psd* doc, fire_psd_info* out_info) {
    if (!doc || !out_info || !doc->d.document) {
        return 1;
    }
    const psd::Document* document = doc->d.document;
    const unsigned int mode = document->colorMode;
    // How many planes the composite actually has. Prefer the parsed section's count over
    // the header's, since that is what the read below indexes.
    const unsigned int planes =
        doc->d.imageData ? doc->d.imageData->imageCount : document->channelCount;
    const unsigned int colorChannels = color_channel_count(mode, planes);
    const bool hasAlpha = planes > colorChannels && doc->d.imageData
                          && doc->d.imageData->images[colorChannels].data;
    // Everything except greyscale-family modes composites to colour. This is the count of
    // what we HAND BACK, not the document's channelCount — see the field's doc comment.
    const bool isGray = (mode == psd::colorMode::GRAYSCALE) || (mode == psd::colorMode::DUOTONE)
                        || (mode == psd::colorMode::BITMAP);

    out_info->width = document->width;
    out_info->height = document->height;
    out_info->channels = static_cast<uint16_t>((isGray ? 1 : 3) + (hasAlpha ? 1 : 0));
    out_info->bits_per_channel = static_cast<uint16_t>(document->bitsPerChannel);
    out_info->color_mode = static_cast<uint16_t>(mode);
    out_info->reserved = 0;
    return 0;
}

int fire_psd_read_merged(const fire_psd* doc, void* out_pixels, size_t out_len) {
    if (!doc || !out_pixels || !doc->d.document) {
        return 1;
    }
    const Doc& d = doc->d;
    if (!d.imageData) {
        return 2; // no merged image (PSD saved without Maximize Compatibility)
    }
    const psd::Document* document = d.document;
    const uint32_t width = document->width;
    const uint32_t height = document->height;
    const size_t pixels = static_cast<size_t>(width) * height;

    const unsigned int bits = document->bitsPerChannel;
    if (!bits_are_supported(bits)) {
        return 6; // refused at open; belt-and-braces so the sample walk cannot be reached
    }
    if (out_len < pixels * 4 * (bits / 8u)) {
        return 3;
    }

    const unsigned int imageCount = d.imageData->imageCount;
    const psd::PlanarImage* images = d.imageData->images;
    if (imageCount == 0 || !images[0].data) {
        return 4;
    }

    const unsigned int mode = document->colorMode;
    const unsigned int colorChannels = color_channel_count(mode, imageCount);
    // Alpha is the plane immediately after the colour planes — for EVERY colour mode, not
    // just RGB. Keying it on "is there a 4th plane" instead used a spot channel as opacity
    // on an RGB document that had one, and dropped alpha entirely on greyscale.
    const bool hasAlpha = imageCount > colorChannels && images[colorChannels].data;

    // A plane we are told to read but that was never allocated: treat the document as
    // malformed rather than sampling a null pointer.
    for (unsigned int c = 0; c < colorChannels && c < imageCount; ++c) {
        if (!images[c].data) {
            return 4;
        }
    }

    // INDEXED needs its palette: 256 R bytes, then 256 G, then 256 B.
    const uint8_t* palette = nullptr;
    if (mode == psd::colorMode::INDEXED) {
        if (!d.colorModeData || !d.colorModeData->colorData
            || d.colorModeData->sizeOfColorData < 768) {
            return 7; // indexed document with no usable palette
        }
        palette = d.colorModeData->colorData;
    }

    try {
        for (size_t i = 0; i < pixels; ++i) {
            float rgb[3];
            switch (mode) {
                case psd::colorMode::INDEXED: {
                    // The single plane holds palette indices, not intensities. Always
                    // 8-bit (Photoshop has no 16-bit indexed mode).
                    const unsigned int idx = static_cast<const uint8_t*>(images[0].data)[i];
                    rgb[0] = palette[idx] * (1.0f / 255.0f);
                    rgb[1] = palette[256 + idx] * (1.0f / 255.0f);
                    rgb[2] = palette[512 + idx] * (1.0f / 255.0f);
                    break;
                }
                case psd::colorMode::CMYK: {
                    // PSD stores CMYK *inverted*: 255 means no ink, 0 means full ink. So
                    // the stored values are already (1-ink) and compose straight into RGB —
                    // R = C·K, and so on. The old code mapped C->R, M->G, Y->B and dropped
                    // K entirely, which turned pure cyan into red.
                    const float c = sample_norm(images[0].data, bits, i);
                    const float m = sample_norm(images[1].data, bits, i);
                    const float y = sample_norm(images[2].data, bits, i);
                    const float k = sample_norm(images[3].data, bits, i);
                    rgb[0] = c * k;
                    rgb[1] = m * k;
                    rgb[2] = y * k;
                    break;
                }
                case psd::colorMode::LAB: {
                    lab_to_srgb(sample_norm(images[0].data, bits, i),
                                sample_norm(images[1].data, bits, i),
                                sample_norm(images[2].data, bits, i), rgb);
                    break;
                }
                case psd::colorMode::GRAYSCALE:
                case psd::colorMode::DUOTONE: {
                    // Duotone is stored as greyscale plus ink metadata we do not model;
                    // showing the grey is the conventional fallback.
                    rgb[0] = rgb[1] = rgb[2] = sample_norm(images[0].data, bits, i);
                    break;
                }
                default: {
                    // RGB, and MULTICHANNEL as a best effort (its first three inks).
                    rgb[0] = sample_norm(images[0].data, bits, i);
                    rgb[1] = (colorChannels > 1 && imageCount > 1)
                                 ? sample_norm(images[1].data, bits, i) : rgb[0];
                    rgb[2] = (colorChannels > 2 && imageCount > 2)
                                 ? sample_norm(images[2].data, bits, i) : rgb[0];
                    break;
                }
            }
            const float a = hasAlpha ? sample_norm(images[colorChannels].data, bits, i) : 1.0f;
            store_sample(out_pixels, bits, i * 4 + 0, rgb[0]);
            store_sample(out_pixels, bits, i * 4 + 1, rgb[1]);
            store_sample(out_pixels, bits, i * 4 + 2, rgb[2]);
            store_sample(out_pixels, bits, i * 4 + 3, a);
        }
    } catch (...) {
        return 5;
    }
    return 0;
}

size_t fire_psd_icc_len(const fire_psd* doc) {
    if (!doc) {
        return 0;
    }
    const psd::ImageResourcesSection* r = doc->d.resources;
    if (!r || !r->iccProfile) {
        return 0;
    }
    return r->sizeOfICCProfile;
}

int fire_psd_icc_get(const fire_psd* doc, uint8_t* out_icc, size_t out_len) {
    if (!doc || !out_icc) {
        return 1;
    }
    const psd::ImageResourcesSection* r = doc->d.resources;
    if (!r || !r->iccProfile) {
        return 2;
    }
    if (out_len < r->sizeOfICCProfile) {
        return 3;
    }
    std::memcpy(out_icc, r->iccProfile, r->sizeOfICCProfile);
    return 0;
}

void fire_psd_free(fire_psd* doc) {
    if (!doc) {
        return;
    }
    Doc& d = doc->d;
    if (d.imageData) {
        psd::DestroyImageDataSection(d.imageData, &d.allocator);
    }
    if (d.resources) {
        psd::DestroyImageResourcesSection(d.resources, &d.allocator);
    }
    if (d.colorModeData) {
        psd::DestroyColorModeDataSection(d.colorModeData, &d.allocator);
    }
    if (d.document) {
        psd::DestroyDocument(d.document, &d.allocator);
    }
    if (d.file) {
        d.file->Close();
        delete d.file;
        d.file = nullptr;
    }
    delete doc;
}

} // extern "C"
