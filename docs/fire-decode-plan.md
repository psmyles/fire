# fire-decode - Shared Decoder Plan

**Status: exploration and design (October 2026).** Nothing here is scheduled, and none of it lands
before the late-October public releases of fire, bite and 3D Review. This document records the
decisions reached while designing it, so the work can start from them later.

The plan: move `fire-decode` out of fire into its own public repository and make it the one image
decoder behind three apps.

| App | What it uses the crate for |
| --- | --- |
| **fire** | Everything it decodes today (it remains the crate's most demanding consumer) |
| **3D Review** | Material and Tex-viewport textures, replacing `crates/render/src/texture.rs`'s decode path and `crates/psd` |
| **bite** | Import thumbnails for PNG / TGA / PSD (and EXR, once checked), plus reading its cached thumbnails. ImageMagick stays for every transform and for the formats routed to it |

Decisions are numbered **FD1-FD15** (the `FD` prefix keeps them apart from `architecture.md`'s
D1-D25). Section 5 is the migration order with its gates; Appendix A lists the code facts the
decisions rest on, so they can be re-checked before work starts.

---

## 1. Why

- **3D Review and bite each carry their own decode path**, which is the duplication this removes.
  3D Review decodes with the `image` crate, zune for JPEG, and its own copy of the psd_sdk bridge;
  bite thumbnails through ImageMagick processes and decodes its cache with `png` / `image-webp`.
- **fire-decode's recent speed work is exactly what the other two are missing.** The figures below
  are fire-decode's own measurements, from its source comments:
  - Greyscale and grey+alpha sources: `image` 0.25's `to_rgba8()` widens them through `f32` per
    pixel - ~420 ms on an 8192² greyscale TGA, nearly 10× the three-times-larger RGB file.
    3D Review's `dynamic_to_decoded` still calls `to_rgba8()`, and roughness / metal / AO / height
    maps are usually greyscale.
  - JPEG: `zune_image::Image::read` costs three extra passes, ~70 ms of an 8192² JPEG's ~400.
    3D Review still decodes JPEG through it.
  - PSD: in-place read (no copy of the document), threaded interleave, an integer path for 8/16-bit
    RGB and greyscale, and the document's own bit depth.
  - EXR: a plane-reading fast path (~1.2 s of a ~1.5 s 16K HDRI open went to the general reader).
  - Opaque-alpha scan: ~4 ms against ~25 ms on an opaque 8192² RGBA8 image.
- **fire gains too**: 3D Review's hardened psd_sdk (FD3), a shared thread budget (FD8), and a panic
  guard owned by the crate rather than each caller (FD7).

### What bite does and does not gain

bite's usual inputs are game-dev formats - PNG, TGA, PSD, sometimes JPG. Today a cold import feeds
`ThumbnailCache::load_batch` 16 files at a time; cache misses go to `magick` in commands of 8 files,
so at most two commands run at once (each allowed half the cores), and the files inside a command
are decoded one after another. Each is written as a quality-85 WebP and then read back and decoded
in-process for display. Re-imports of a cached folder are already in-process and are unaffected.

- **PNG / TGA / PSD (and EXR): expected gains.** ImageMagick decodes these at full size, a file at a
  time, behind a process spawn per 8 files; the crate decodes them in-process, one file per thread
  across the machine, with the fast paths above, and hands back the thumbnail without the
  write-then-read-back of a WebP.
- **JPG: no expected gain.** bite passes `jpeg:size=2N×2N`, so ImageMagick decodes JPEGs at a
  reduced DCT scale. fire-decode always decodes JPEG at full size. JPG stays on ImageMagick (FD9).
- **Camera raw: stays on ImageMagick.** fire-decode reads the embedded camera preview, which is the
  camera's rendering, not what ImageMagick develops - bite's preview would stop matching its output.

How much faster is measured before bite switches (section 6), not assumed.

---

## 2. Prior art - is there already a public equivalent?

Checked in October 2026. No public library matches fire-decode's combination, which is the reason
to keep and share it rather than adopt something else:

- PSD merged composite through a hardened psd_sdk, at 8/16/32-bit,
- DDS BC1-BC7 including BC6H, with the file's own mip chain,
- an EXR plane fast path,
- per-format routing to whichever decoder **measured** fastest (zune vs `image`),
- decode-bomb guards before allocation, and isolation of the FFI backends,
- one normalized RGBA output in the source's bit depth, behind a single entry point.

| Library | Language / licence | PSD | DDS | EXR | Notes | Why not |
| --- | --- | --- | --- | --- | --- | --- |
| [`image`](https://docs.rs/crate/image/latest) 0.25.10 (+ [`image-extras`](https://docs.rs/image-extras)) | Rust, MIT/Apache | No | Decode only; compressions not documented (fire's own survey found DXT1/3/5 only) | Yes | The general-purpose Rust decoder; `image-extras` adds DDS, PCX, SGI, XPM and others | fire-decode already builds on it for PNG / HDR / TGA / TIFF / GIF. No PSD, and the slow paths above are in it |
| [`zune-image`](https://github.com/etemesi254/zune-image) | Rust | Base layer, 8/16-bit, grey/RGB/RGBA only ([`zune-psd`](https://lib.rs/crates/zune-psd), last release 2024) | No | No | Speed-first, fuzzed | fire-decode already uses its JPEG / BMP / QOI / PPM decoders. No DDS / EXR / TGA |
| [OpenImageIO](https://openimageio.readthedocs.io/en/latest/builtinplugins.html) | C++, Apache-2.0 | Yes (RGB, CMYK, grey, indexed, multichannel) | BC1-BC7, cube / volume / mips | Yes | The VFX-industry reader; also TGA, raw via LibRaw, HEIF, JPEG XL | Closest in coverage, but a heavy native dependency stack built for pipelines, not time-to-first-pixel; the only Rust binding found is a small third-party project ([openimageio-rs](https://github.com/ennis/openimageio-rs)) |
| [SAIL](https://github.com/HappySeaFox/sail/blob/master/FORMATS.md) | C11, MIT | Composite only | No | Simplified RGBA API | "Fast imaging library"; C++ / Python bindings | No DDS, no Rust binding |
| [libvips](https://docs.rs/crate/libvips/latest) (Rust `libvips` 2.3.0) | C, LGPL | Via its ImageMagick loader | No | Yes | Shrink-on-load thumbnails - the strongest alternative for bite's thumbnail step specifically | Native LGPL stack, PSD only through ImageMagick, no DDS; for JPEG it would match, not beat, what bite already gets from `jpeg:size` |
| [`image_dds`](https://rust-digger.code-maven.com/crates/image_dds) 0.6.1 | Rust | - | BC1-BC7 incl. BC6H | - | Wraps a bcdec port | DDS only, and hides the format table fire-decode wants to own (already rejected in fire's `Cargo.toml`) |
| [oculante](https://github.com/woelper/oculante) | Rust app, MIT | Via `psd` | DXT1-5 | Yes | A broad-format Rust image viewer | An application; its loading is not published as a reusable crate |

---

## 3. Decision record

### FD1 - Its own public repository, crate name unchanged

`fire-decode` moves to **`github.com/psmyles/fire-decode`**, public, `MIT OR Apache-2.0`, keeping the
crate name `fire-decode` (fire's `use fire_decode::...` lines do not change). Apps depend on it by
git tag.

Considered:
- **Keep it in fire's repo, consumed by git tag** - least restructuring, but every consumer clones
  fire's whole repo (including `heif-sys`'s 57 MB of prebuilt libraries), the API stays shaped by
  fire, and fire's CI would not notice a change that breaks another consumer. It was the
  recommendation for two consumers; bite makes three.
- **Fold into slab** - rejected: slab is the app shell (windowing, settings, i18n, packaging), not
  image decoding, and it stays private until stable, which a public app cannot depend on.
- **Path dependency to a sibling checkout, or copying the code** - rejected: the first breaks CI and
  every outside clone, the second is the duplication being removed.

### FD2 - What moves, and what stays in fire

**Moves:** the decode core (routing, every pure-Rust backend, EXIF, ICC, downscale), the merged PSD
crate (FD3), GIF animation, `SheetLayout` (DDS arrays / volumes), and camera raw behind a **`raw`
feature, off by default** (fire enables it; bite leaves it off so raw thumbnails keep matching
ImageMagick).

**Stays in fire:**
- **HEIF / HEIC / AVIF.** fire routes HEIF to its own `heif-sys` before calling the crate. This keeps
  the shared repo free of the 57 MB of prebuilt libheif / libde265 / dav1d, of bindgen, and of LGPL
  code (libheif and libde265 are LGPL). The cost is two small API additions: a public
  **`finish(img, &opts)`** that runs the post-decode passes (orientation, ICC, fit, opaque scan) on
  an image fire decoded itself, and `sniff()` reporting HEIF as not handled here
  (`DecodeError::Unsupported(Format)`, FD13).
- **The installer-association test** (`installer_associations_match_the_extension_table`), which
  `include_str!`s `installer/fire.iss` and cannot compile in another repo. The extension list itself
  moves (FD9).

### FD3 - One PSD crate, merged from both apps

3D Review's `crates/psd` began as a copy of fire's `psd-sdk-sys` (it still uses the `fire_psd_`
symbols) and was hardened separately; fire's copy was sped up separately. The merge takes:

- **From 3D Review: the psd_sdk source and its patches.** Same upstream commit (`f514495`), plus
  `review.patch`: P1 bounds-checks every RLE run against source and destination; P2 computes plane
  sizes in 64-bit, null-checks allocations, bounds the RLE payload and frees on failure. fire's copy
  has neither, and a malformed PSD writing past a buffer inside the C++ is not something
  `catch_unwind` or an after-the-fact `check_dims` can catch.
- **From 3D Review: committed, hand-kept bindings** with size / offset assertions. The shared repo
  never runs bindgen or needs libclang.
- **From fire: the wrapper's speed work** - in-place read, threaded interleave, the integer path for
  8/16-bit RGB and greyscale, and returning the document's own depth.
- **The `fire_psd_` C symbol prefix stays** (it matches the crate name).
- **psd_sdk's image-resources parser is never called.** 3D Review stopped calling it because its
  `DISPLAY_INFO` branch writes through NULL for a greyscale document with alpha. fire needs the
  embedded ICC profile, which lives in that section, so the crate finds it **in Rust**: a
  bounds-checked walk of the image-resources section's `8BIM` blocks for resource **1039**
  (~40 lines, testable on byte arrays without the C++ build).
- The crate's thread budget (FD8) passes the thread count into the C++ rather than the wrapper
  choosing its own.

### FD4 - Output format, and the source described separately

`DecodeOptions` gains **`output: Native | Rgba8`**:
- `Native` is today's behaviour (8/16-bit unorm, half or 32-bit float, as the source holds).
- `Rgba8` narrows inside the crate - fused into the final widening pass where the source is already
  8-bit, otherwise one threaded pass, run **after** any `fit` so a thumbnail narrows 256 px rather
  than 8K.
- `DecodedImage::format` describes the **buffer**; a separate **`source`** (channels, bit depth,
  sample kind, format name) describes the **file**, so 3D Review's stats stay faithful once the
  crate narrows. (Today `bit_depth` describes the buffer.)
- **Float → 8-bit: clamp to 0..1 and scale, no transfer curve** - right for data textures (EXR
  height / mask maps) and what 3D Review gets from `to_rgba8()` today. An sRGB-encode option is
  added only if bite's ImageMagick comparison calls for it (section 6).
- **16 → 8-bit rounds to nearest**, matching ImageMagick (bite's current `STRIP_16` truncates).

### FD5 - Named setups, no `Default`

`DecodeOptions` loses its `Default` (which is fire's behaviour) in favour of named constructors, so
each app's intent is explicit and a new field forces a decision per setup instead of silently
inheriting fire's:

| Setup | Consumer | ICC | EXIF orientation | Output | Fit | Animation / layers |
| --- | --- | --- | --- | --- | --- | --- |
| `viewer()` | fire | On | On | `Native` | `16384, Nearest` | All GIF frames; DDS layout and mips kept |
| `texture()` | 3D Review | Off | Off | `Rgba8` | `gpu_max, Area` | First frame / layer / face only; stored mips ignored |
| `thumbnail(size)` | bite | Off | Off | `Rgba8` | `size, Area` (shrink only) | First frame / layer / face only |

- Every field stays public for one-off adjustments.
- **ICC is a cargo feature (`icc`)**; without it lcms2 is not built and the ICC field does not exist,
  so asking for ICC on a build without it fails to compile rather than being ignored.
- EXIF orientation becomes a switch (today it is unconditional). Engines' texture importers do not
  rotate by EXIF, and a rotated texture no longer matches its UVs.
- Off-ICC / off-EXIF is also what keeps bite's thumbnails matching ImageMagick: `-thumbnail` keeps the
  colour profile without converting, and ImageMagick only rotates with `-auto-orient`.

### FD6 - Resizing: `fit` with a filter

`max_dim` becomes **`fit: Option<Fit { max, filter: Nearest | Area }>`** (values per setup in FD5).
- `Nearest` keeps fire's existing RAM-guard behaviour for >16K images.
- `Area` is an area average for real reductions (thumbnails; 3D Review's rare oversize texture,
  where aliasing would read as an asset defect).
- **`Area` averages stored values, not linear light** - what ImageMagick's `-thumbnail` does by
  default (so bite's previews match), and correct for data textures. This deliberately differs from
  the mip builders (FD11), which average colour in linear light; the crate documents why.
- `downscaled_from` is reported as today; 3D Review shows it in the texture stats.

### FD7 - Panics are caught by the crate, and unwinding is required

- **`decode()` wraps itself in `catch_unwind`** and returns **`DecodeError::Panicked`**. bite is
  covered with no change of its own (today a decode panic inside `decode_many`'s scoped threads
  takes the import thread down and leaves the progress modal open); fire's own pool wrapper becomes
  a harmless second layer.
- **The crate refuses to compile under `panic = "abort"`** (via `cfg(panic = ...)`) unless an
  **`allow-abort`** feature acknowledges the guard is inert - the requirement is a build error, not a
  doc comment.
- **3D Review switches release from `abort` to `unwind`**, and its panic hook (in `review-log`)
  **aborts unless the panicking thread is inside a decode** (a thread-local flag set around the
  `decode()` call). Decode panics become ordinary errors; every other panic still ends the app
  loudly, as today - without having to add `catch_unwind` to its seven background-thread spawn
  sites. Binary size and startup are measured with its startup gate after the switch.

### FD8 - A process-wide thread budget

Today a single decode splits its big passes across up to 8 threads (`par_chunks`), the PSD C++ starts
up to 8 of its own, and the callers decode several images at once (fire: up to 4 workers;
3D Review: a thread per texture; bite: half the cores, max 8) - so threads multiply.

- The crate keeps **one process-wide budget capped at the core count**. Each parallel pass reserves
  what is free and runs on the calling thread when nothing is. One big image gets every core;
  twenty concurrent decodes get about one each. No rayon - a counter around the scoped threads the
  crate already uses.
- The PSD C++ takes its thread count from the Rust side (FD3).
- `DecodeOptions::max_threads` remains as an override (benchmarks, determinism).
- **3D Review replaces thread-per-texture with a worker pool of half the cores** (bite's
  `default_jobs` rule), keeping its path de-duplication and generation checks. The budget bounds
  CPU, not memory: twenty concurrent 4K decodes hold ~1.3 GB of RGBA8 at once.

### FD9 - Formats: accept what the crate decodes; route before decoding

- **3D Review accepts everything the crate decodes.** An FBX referencing a `.dds` (common when it was
  exported from an engine's content folder) shows its texture instead of failing; the stats name the
  real source format. The `CLAUDE.md` non-goal is reworded from "no KTX2/DDS import" to "no
  engine-cooking workflows" (no KTX2, no platform formats, no compression settings).
- **`sniff(bytes, ext) -> Format` becomes public**, and `SUPPORTED_EXTENSIONS` becomes a
  feature-aware **`supported_extensions()`** (no `.psd` without `psd`, and so on):
  - fire routes HEIF to `heif-sys` first (FD2);
  - bite routes PNG / TGA / PSD / EXR to the crate and everything else - JPG, raw, SVG, JPEG 2000,
    DPX, ... - to ImageMagick;
  - 3D Review's texture dialog filters on `supported_extensions()`.

### FD10 - Errors

- New variants: **`Unsupported(Format)`** (compiled out, or routed elsewhere such as HEIF) and
  **`Panicked`** (FD7).
- `TooLarge` keeps its meaning - a refusal, never retried by a second decoder.
- **bite retries a failed file through ImageMagick on any error except `TooLarge`.**
- 3D Review maps errors to its existing message strings.

### FD11 - Mip builder, phase 2

fire's `crates/fire/src/render/mips.rs` (multi-format, threaded, linear-light sRGB through lookup
tables, adopts a file's own chain) moves into the crate as a **`mips` feature**, after the decode
migration is done:
- gains `srgb: bool` (3D Review's raw-average mode, for data textures and the Tex viewport) and
  `max_levels` (so 3D Review can pass sokol's limit without the crate depending on sokol), and uses
  the thread budget;
- 3D Review deletes `rhi/mips.rs` and **builds chains on its decode worker** instead of inside
  `Texture::rgba8_mipped_or_white` at upload (on the render thread today, and twice for a texture
  shown in both a material slot and the Tex viewport), caching by `(path, srgb)`;
- bite leaves the feature off.

### FD12 - bite's thumbnail cache

- Thumbnails the crate makes are written as **lossless WebP through `image-webp`** (already a bite
  dependency), under the same `thumb_{hash}_{size}.webp` names - lookup, invalidation, cleanup and
  ImageMagick's preview chain are unchanged. Files are larger than quality-85 lossy; if cache sizes
  at 1024+ px prove a problem, switch to lossy through libwebp (the `webp` crate).
- **Cached thumbnails are read through the crate too**, replacing bite's own `decode_png` /
  `decode_webp`.

### FD13 - Repository setup

- **Versions:** tags `vX.Y.Z`; each app depends with `git = "...", tag = "..."` and moves up when it
  chooses. Local cross-repo work goes through `[patch."https://github.com/psmyles/fire-decode"]` in
  an untracked `.cargo/config.toml` pointing at a local checkout.
- **Toolchain:** edition 2024; the crate and all three apps move to the **latest stable Rust**
  (bite is on 1.87; `as_chunks` needs 1.88).
- **Lints:** 3D Review's level - `clippy::all` denied and `undocumented_unsafe_blocks` denied. The
  crate has little `unsafe` outside the PSD bridge, which already meets that bar.
- **Cargo features:** `psd`, `icc`, `raw`, `mips`, `allow-abort`.
- **CI (correctness only):** build, clippy and test the three consumer feature sets - viewer
  (`psd icc raw mips`), texture (`psd mips`), thumbnail (`psd`) - on Windows x64 and macOS arm64.
- **Performance gate (local):** a bench harness timing each format over a corpus directory given by
  an environment variable (fire's validation set, bite's `test_images`, 3D Review's
  `assets/test_textures` cannot all be committed). Run before each tag; **no tag if any format is
  more than 2% slower than the previous tag.** fire's TTFP benchmark remains the app-level gate when
  fire moves to a new tag.

### FD14 - Licences and consumer docs

- The repo carries the third-party licence texts for its dependencies, grouped by feature
  (psd_sdk BSD-2-Clause under `psd`, lcms2 under `icc`, and so on).
- 3D Review's `licenses/` and bite's `THIRD_PARTY_LICENSES` are updated when each adopts the crate.
- 3D Review's `CLAUDE.md`: invariant 9 adds the crate's PSD bridge as a sanctioned FFI site outside
  the workspace, `crates/psd` is removed, and the DDS non-goal is reworded (FD9).

### FD15 - Timing

Design only for now. The decode path sits under every app's core workflow and under release builds,
so nothing here starts until after the late-October releases.

---

## 4. API sketch

Illustrative only - names settle during step 3 of the migration.

```rust
pub fn sniff(bytes: &[u8], ext_hint: Option<&str>) -> Format;
pub fn supported_extensions() -> &'static [&'static str];        // feature-aware

pub fn decode(bytes: &[u8], ext_hint: Option<&str>, opts: &DecodeOptions)
    -> Result<DecodedImage, DecodeError>;                        // catch_unwind inside
pub fn decode_path(path: &Path, opts: &DecodeOptions) -> Result<DecodedImage, DecodeError>;
pub fn finish(img: &mut DecodedImage, opts: &DecodeOptions);     // for fire's HEIF

pub struct DecodeOptions {
    #[cfg(feature = "icc")] pub honor_icc: bool,
    pub honor_exif: bool,
    pub output: Output,                // Native | Rgba8
    pub fit: Option<Fit>,              // Fit { max: u32, filter: Filter /* Nearest | Area */ }
    pub frames: Frames,                // All | First
    pub max_threads: Option<usize>,
}
impl DecodeOptions {
    pub fn viewer() -> Self;
    pub fn texture(gpu_max: u32) -> Self;
    pub fn thumbnail(size: u32) -> Self;
}

pub struct DecodedImage {
    pub pixels: Vec<u8>, pub width: u32, pub height: u32,
    pub format: PixelFormat,           // the buffer
    pub source: SourceInfo,            // the file: channels, bit depth, sample kind, format name
    pub alpha_opaque: bool,
    pub icc: Option<Vec<u8>>,
    pub downscaled_from: Option<(u32, u32)>,
    pub source_mips: Option<Vec<Vec<u8>>>,
    pub layout: Option<SheetLayout>,
    pub animation: Option<Animation>,
}

pub enum DecodeError { Malformed(String), TooLarge(String), Ffi(String),
                       Unsupported(Format), Panicked, Other(String) }

#[cfg(feature = "mips")]
pub fn mip_chain(img: &DecodedImage, srgb: bool, max_levels: u32) -> Vec<Vec<u8>>;
```

---

## 5. Migration order and gates

Each step lands only when its gate passes.

| Step | Work | Gate |
| --- | --- | --- |
| 1 | Extract `crates/fire-decode` and `crates/psd-sdk-sys` into the new repo with `git filter-repo`, keeping their history (the commit messages carry the measurements behind each choice). fire switches to the git tag - no code change | fire TTFP within 2%; fire's tests pass |
| 2 | PSD merge (FD3): 3D Review's patched psd_sdk and committed bindings, fire's wrapper, the Rust ICC scan. 3D Review's crate arrives as one commit naming its origin | Both apps' PSD tests; fire TTFP on PSDs |
| 3 | API changes: named setups, `output`, `source`, `fit`, `sniff` / `finish` / `supported_extensions`, thread budget, panic guard and abort check, cargo features. fire moves to `viewer()` | fire TTFP |
| 4 | 3D Review adopts `texture()`; switches to unwind with the decode-only hook; worker pool; deletes `crates/psd` and its decode code | Startup gate and binary size; before/after pixel comparison over `assets/test_textures` (differences explained, e.g. 16-bit rounding) |
| 5 | bite: cold-import benchmark by format first; then route PNG / TGA / PSD through the crate (EXR after the ImageMagick comparison), lossless-WebP cache files, cached reads through the crate | Cold import faster per routed format; previews visually unchanged; cache size acceptable |
| 6 | Phase 2: `mips` feature; fire uses it; 3D Review builds chains on its worker | fire TTFP; 3D Review frame times while textures load |

3D Review goes before bite: it was the original motivation, and its gains do not depend on a
benchmark.

---

## 6. To measure before or during the work

- **bite cold import, by format.** `crates/bite-gui/examples/import_bench.rs` with the thumbnail
  cache cleared - its first pass is exactly the cold import - against a throwaway variant through
  the crate, over `test_images` (JPG / TGA / EXR / PNG) plus a folder of PSDs.
- **bite EXR thumbnails.** Compare `magick <exr>[0] -thumbnail ...` with the crate's clamp-no-curve
  output for the EXRs in `test_images`; add an sRGB-encode option only if they differ visibly.
- **bite cache size** with lossless WebP at 256 and 1024 px on a typical folder.
- **3D Review** binary size and startup after the switch to unwind.
- Informational: whether bite's bundled ImageMagick is a Q16-HDRI build (it then holds 16 bytes a
  pixel during decode) and whether it has a raw delegate.

Not part of this plan, noted for later: the crate's opaque-alpha scan could drive a 3D Review audit
warning for "RGBA texture whose alpha is unused".

---

## Appendix A - Code facts this plan rests on

As of fire `927ee03`, 3D Review `8f033f3`, bite `9407dc5` (October 2026).

**fire**
- `crates/fire-decode`: `lib.rs` 3,539 lines plus `dds.rs`, `downscale.rs`, `exif.rs`, `raw.rs`,
  `tiff.rs`; one entry point `decode(bytes, ext_hint, &DecodeOptions)`; `psd` / `heif` default-on
  features so CI can build the pure-Rust core without native trees.
- `crates/psd-sdk-sys`: psd_sdk at `f514495`, **unpatched**; bindings generated by **bindgen** at
  build time; the wrapper reads ICC through psd_sdk's image-resources parser.
- `crates/heif-sys`: prebuilt static libheif + libde265 + dav1d (57 MB under `vendor/`), bindgen.
- Release profile: `panic = "unwind"` on purpose (the `catch_unwind` around FFI); decode pool of up
  to 4 workers (`decode_pool.rs`), which also wraps each decode in `catch_unwind`.
- `downscale::to_fit` is nearest-neighbour by design (a RAM guard for >16K images).
- `render/mips.rs` (583 lines) runs on the decode worker; averages 8-bit sRGB in linear light.

**3D Review**
- `crates/render/src/texture.rs`: PSD → `review_psd`; JPEG → `zune_image::Image::read` +
  `convert_color` + `flatten`; everything else → `image::ImageReader` + `to_rgba8()`. Output RGBA8
  plus `source_channels` / `source_bit_depth` for the stats panel.
- `crates/psd`: psd_sdk at `f514495` **with `review.patch` (P1, P2)**; committed hand-kept
  `bindings.rs`; the image-resources parser is not called.
- Release profile: `panic = "abort"`; a panic hook in `crates/log` logs before the abort.
- `crates/app/src/texture_manager.rs`: one `std::thread` per texture decode, de-duplicated by path.
- `crates/render/src/rhi/mips.rs` (201 lines): RGBA8 only, sRGB or raw averaging, built inside
  `Texture::rgba8_mipped_or_white` at upload.
- `CLAUDE.md`: invariant 9 (FFI sites: `import`, `psd`, `optimize`); KTX2/DDS import listed as a
  non-goal.
- Workspace: edition 2024, `rust-version` 1.88, `clippy::all` and `undocumented_unsafe_blocks`
  denied.

**bite**
- `crates/bite-imagemagick/src/import.rs`: `ThumbnailCache` - header probe in parallel; misses
  thumbnailed by `magick` in commands of 8 files (`batch_size`), run in parallel up to `jobs` with
  `MAGICK_THREAD_LIMIT` set to each command's share of the cores; JPEGs get
  `-define jpeg:size=2N×2N`; `-thumbnail NxN> -quality 85` to `thumb_{hash}_{size}.webp`.
- `crates/bite-gui/src/commands.rs` `add_paths`: calls `load_batch` 16 files at a time, so at most
  two commands run at once; then decodes the cached WebPs in-process (`work::decode_many`).
- `crates/bite-gui/src/work.rs` `render_preview`: the ImageMagick preview chain runs **over the
  cached thumbnail file**.
- `thumbnailSize` per Input node: default 256, clamped 64-2048.
- Workspace: edition 2021, `rust-version` 1.87, release `panic` left at unwind.
