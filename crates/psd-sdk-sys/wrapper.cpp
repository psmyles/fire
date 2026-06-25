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

#include <cstring>
#include <vector>
#include <new>

namespace {

// psd_sdk reads exclusively through the abstract File interface (async by contract).
// We back it with an in-memory buffer and serve reads synchronously.
class MemoryFile : public psd::File {
public:
    MemoryFile(psd::Allocator* allocator, const uint8_t* data, size_t size)
        : psd::File(allocator), m_data(data), m_size(size) {}

private:
    bool DoOpenRead(const wchar_t*) PSD_OVERRIDE { return true; }
    bool DoOpenWrite(const wchar_t*) PSD_OVERRIDE { return false; }
    bool DoClose(void) PSD_OVERRIDE { return true; }

    ReadOperation DoRead(void* buffer, uint32_t count, uint64_t position) PSD_OVERRIDE {
        if (position > m_size) {
            return nullptr;
        }
        const uint64_t available = m_size - position;
        const uint32_t toCopy = (count <= available) ? count : static_cast<uint32_t>(available);
        std::memcpy(buffer, m_data + position, toCopy);
        // Non-null sentinel: the read already completed; WaitForRead just acknowledges it.
        return reinterpret_cast<ReadOperation>(1);
    }
    bool DoWaitForRead(ReadOperation&) PSD_OVERRIDE { return true; }

    WriteOperation DoWrite(const void*, uint32_t, uint64_t) PSD_OVERRIDE { return nullptr; }
    bool DoWaitForWrite(WriteOperation&) PSD_OVERRIDE { return true; }

    uint64_t DoGetSize(void) const PSD_OVERRIDE { return m_size; }

    const uint8_t* m_data;
    size_t m_size;
};

// Everything parsed for one document, owned behind a single opaque handle.
struct Doc {
    psd::MallocAllocator allocator;
    std::vector<uint8_t> bytes;      // owned copy; MemoryFile points into this
    MemoryFile* file = nullptr;
    psd::Document* document = nullptr;
    psd::ImageDataSection* imageData = nullptr;
    psd::ImageResourcesSection* resources = nullptr;
};

// Read one channel's value at planar index `i`, normalized to 8-bit, by source bit depth.
inline uint8_t sample_channel(const void* data, unsigned int bits, size_t i) {
    if (bits == 8) {
        return static_cast<const uint8_t*>(data)[i];
    }
    if (bits == 16) {
        return static_cast<uint8_t>(static_cast<const uint16_t*>(data)[i] >> 8);
    }
    // 32-bit float (typically linear 0..1). Clamp + scale; the proper linear→sRGB
    // encode is handled later in the render color pipeline.
    float v = static_cast<const float*>(data)[i];
    if (v < 0.0f) v = 0.0f;
    if (v > 1.0f) v = 1.0f;
    return static_cast<uint8_t>(v * 255.0f + 0.5f);
}

} // namespace

struct texview_psd {
    Doc d;
};

extern "C" {

texview_psd* texview_psd_open(const uint8_t* bytes, size_t len) {
    if (!bytes || len == 0) {
        return nullptr;
    }
    texview_psd* handle = new (std::nothrow) texview_psd();
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
            texview_psd_free(handle);
            return nullptr;
        }
        d.file->OpenRead(L"memory");
        d.document = psd::CreateDocument(d.file, &d.allocator);
        if (!d.document) {
            texview_psd_free(handle);
            return nullptr;
        }
        // Merged image is only present when the PSD was saved with Maximize Compatibility.
        if (d.document->imageDataSection.length != 0) {
            d.imageData = psd::ParseImageDataSection(d.document, d.file, &d.allocator);
        }
        // Image resources carry the embedded ICC profile (best-effort).
        if (d.document->imageResourcesSection.length != 0) {
            d.resources = psd::ParseImageResourcesSection(d.document, d.file, &d.allocator);
        }
        return handle;
    } catch (...) {
        texview_psd_free(handle);
        return nullptr;
    }
}

int texview_psd_info_get(const texview_psd* doc, texview_psd_info* out_info) {
    if (!doc || !out_info || !doc->d.document) {
        return 1;
    }
    const psd::Document* document = doc->d.document;
    out_info->width = document->width;
    out_info->height = document->height;
    out_info->channels = static_cast<uint16_t>(document->channelCount);
    out_info->bits_per_channel = static_cast<uint16_t>(document->bitsPerChannel);
    return 0;
}

int texview_psd_read_merged_rgba8(const texview_psd* doc, uint8_t* out_pixels, size_t out_len) {
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
    if (out_len < pixels * 4) {
        return 3;
    }

    const unsigned int bits = document->bitsPerChannel;
    const unsigned int imageCount = d.imageData->imageCount;
    const psd::PlanarImage* images = d.imageData->images;
    if (imageCount == 0 || !images[0].data) {
        return 4;
    }

    const bool isGray = (document->colorMode == psd::colorMode::GRAYSCALE) || (imageCount == 1);
    const bool hasAlpha = (document->colorMode == psd::colorMode::RGB) && (imageCount >= 4)
                          && images[3].data;

    try {
        for (size_t i = 0; i < pixels; ++i) {
            uint8_t r, g, b;
            uint8_t a = 255;
            if (isGray) {
                const uint8_t v = sample_channel(images[0].data, bits, i);
                r = g = b = v;
            } else {
                r = sample_channel(images[0].data, bits, i);
                g = (imageCount >= 2 && images[1].data) ? sample_channel(images[1].data, bits, i) : r;
                b = (imageCount >= 3 && images[2].data) ? sample_channel(images[2].data, bits, i) : r;
                if (hasAlpha) {
                    a = sample_channel(images[3].data, bits, i);
                }
            }
            out_pixels[i * 4 + 0] = r;
            out_pixels[i * 4 + 1] = g;
            out_pixels[i * 4 + 2] = b;
            out_pixels[i * 4 + 3] = a;
        }
    } catch (...) {
        return 5;
    }
    return 0;
}

size_t texview_psd_icc_len(const texview_psd* doc) {
    if (!doc) {
        return 0;
    }
    const psd::ImageResourcesSection* r = doc->d.resources;
    if (!r || !r->iccProfile) {
        return 0;
    }
    return r->sizeOfICCProfile;
}

int texview_psd_icc_get(const texview_psd* doc, uint8_t* out_icc, size_t out_len) {
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

void texview_psd_free(texview_psd* doc) {
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
