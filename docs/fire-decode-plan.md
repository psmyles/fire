# fire-decode - Shared Decoder Plan

**Status: design, revised November 2026.** fire, bite and 3D Review shipped their late-October releases, and
none of this work has started. This revision makes three changes:
- HEIF / HEIC / AVIF moves into the crate. The October version kept it in fire.
- fire's viewer gains HDR, gain-map and image-sequence support.
- All three apps move to the static C runtime on Windows.

It also refreshes every code fact (Appendix A) against each app's current head.

The plan: move `fire-decode` out of fire into its own public repository and make it the one image
decoder behind three apps.

| App | What it uses the crate for |
| --- | --- |
| **fire** | Everything it decodes, HEIF / HEIC / AVIF included, in the viewer and in `fire-shellext` (its Explorer thumbnail DLL). It remains the crate's most demanding consumer |
| **3D Review** | Material and Tex-viewport textures, in every binary that links `review-render`: the viewer, `review_shell.dll` (Explorer thumbnails and preview pane), `libreview_quicklook.dylib` (Quick Look) and the snapshot tool. It replaces `texture.rs`'s decode path and `crates/psd` |
| **bite** | Import thumbnails for PNG / TGA / PSD / EXR / HEIC / HEIF / AVIF, header probes for warm re-imports, and reading its cached thumbnails. ImageMagick stays for every transform and for the formats routed to it |

Decisions are numbered **FD1-FD20**. The `FD` prefix keeps them apart from fire's `architecture.md`
D1-D25. Section 5 is the migration order with its gates. Appendix A lists the code facts the decisions
rest on, so they can be re-checked before work starts.

### What changed since the October version

- **HEIF moves into the crate** (FD2, FD16), and `finish()` is gone.
- **New decisions:**
  - HDR and gain maps (FD17)
  - animated AVIF and HEIF image sequences (FD18)
  - `probe()` (FD19)
  - the static CRT on Windows (FD20)
  - per-setup size limits (FD5)
- **Both PSD copies are now hardened**, in different places (FD3).
- **fire's PSD-layer decoder and progress reporting move too** (FD2).
- **New consumers:** fire-shellext, and 3D Review's Explorer DLL and Quick Look dylib.
- **New Appendix A baseline:** 3D Review's and bite's histories were rewritten, so the appendix uses
  their new heads.
- **Migration order** gains a baseline step 0, splits step 2 into PSD and HEIF, and adds step 6 for
  fire's HDR work.

---

## 1. Why

- **3D Review and bite each carry their own decode path**, which is the duplication this removes.
  - 3D Review decodes with three things: the `image` crate, a hand-driven zune JPEG decoder, and its
    own copy of the psd_sdk bridge.
  - bite thumbnails through ImageMagick processes and decodes its cache with `png` / `image-webp`.
- **fire-decode's speed work is exactly what the other two are missing.** The figures below are
  fire-decode's own measurements, from its source comments:
  - **Greyscale and grey+alpha sources:** `image` 0.25's `to_rgba8()` widens them through `f32` per
    pixel. That costs ~420 ms on an 8192² greyscale TGA, nearly 10× the three-times-larger RGB file.
    3D Review's `dynamic_to_decoded` still calls `to_rgba8()`, and roughness / metal / AO / height maps
    are usually greyscale.
  - **JPEG:** `zune_image::Image::read` costs three extra passes, ~70 ms of an 8192² JPEG's ~400.
    3D Review has since moved off `Image::read` to a hand-driven `JpegDecoder`, but still runs
    `convert_color` and `flatten` after the decode. What the crate still saves is measured in step 0,
    not assumed.
  - **PSD:** in-place read (no copy of the document), threaded interleave, an integer path for
    8/16-bit RGB and greyscale, and the document's own bit depth.
  - **EXR:** a plane-reading fast path (~1.2 s of a ~1.5 s 16K HDRI open went to the general reader).
  - **Opaque-alpha scan:** ~4 ms against ~25 ms on an opaque 8192² RGBA8 image.
  - **HEIF / AVIF:** bite has no in-process decoder. Its ImageMagick decodes AV1 through libaom; the
    crate uses dav1d.
- **fire gains too:**
  - a PSD bridge carrying both apps' hardening (FD3);
  - a shared thread budget that also governs libheif and dav1d (FD8, FD16);
  - a panic guard owned by the crate rather than each caller (FD7);
  - HDR, gain-map and sequence support for HEIF (FD17, FD18).

### What bite does and does not gain

bite's usual inputs are game-dev formats: PNG, TGA, PSD, sometimes JPG. A cold import works like this:
- `ThumbnailCache::load_batch` receives 16 files at a time.
- Cache misses go to `magick` in commands of 8 files. A chunk with up to 8 misses runs one command
  limited to one thread; 9-16 misses run two commands at half the cores each.
- The files inside a command are decoded one after another.
- Each is written as a quality-85 WebP, then read back and decoded in-process for display.

Expected gains, format by format:
- **PNG / TGA / PSD (and EXR): expected gains.** ImageMagick decodes these at full size, a file at a
  time, behind a process spawn per 8 files. The crate decodes them in-process, one file per thread
  across the machine, with the fast paths above. It hands back the thumbnail without the
  write-then-read-back of a WebP.
- **HEIC / HEIF / AVIF: expected gains.** The same process-spawn and one-file-at-a-time costs apply,
  and the crate decodes AV1 with dav1d instead of libaom.
- **Warm re-imports of PSD / EXR / TIFF / HEIC / AVIF: expected gains.** bite builds a fresh
  `ThumbnailCache` for each import, and its header parser reads only PNG / BMP / WebP / JPEG / TGA.
  So every cache hit in another format runs one `magick identify` just to learn the image size.
  `probe()` (FD19) answers that in-process.
- **JPG: no expected gain.** bite passes `jpeg:size=2N×2N`, so ImageMagick decodes JPEGs at a reduced
  DCT scale. fire-decode always decodes JPEG at full size. JPG stays on ImageMagick (FD9).
- **Camera raw: stays on ImageMagick.** fire-decode reads the embedded camera preview, which is the
  camera's rendering, not what ImageMagick develops. bite's preview would stop matching its output.

How much faster is measured before bite switches (section 6), not assumed.

---

## 2. Prior art - is there already a public equivalent?

Checked in October 2026; the libheif binding row was added in November 2026. No public library matches
fire-decode's combination, which is the reason to keep and share it rather than adopt something else:

- PSD merged composite through a hardened psd_sdk, at 8/16/32-bit, plus a pure-Rust layer decoder,
- DDS BC1-BC7 including BC6H, with the file's own mip chain,
- an EXR plane fast path,
- HEIF / HEIC / AVIF from a vendored, encoder-free libheif + libde265 + dav1d, with HDR, gain maps and
  sequences (FD16-FD18),
- per-format routing to whichever decoder **measured** fastest (zune vs `image`),
- decode-bomb guards before allocation, and isolation of the FFI backends,
- one normalized RGBA output in the source's bit depth, behind a single entry point.

| Library | Language / licence | PSD | DDS | EXR | Notes | Why not |
| --- | --- | --- | --- | --- | --- | --- |
| [`image`](https://docs.rs/crate/image/latest) 0.25.10 (+ [`image-extras`](https://docs.rs/image-extras)) | Rust, MIT/Apache | No | Decode only; compressions not documented (fire's own survey found DXT1/3/5 only) | Yes | The general-purpose Rust decoder; `image-extras` adds DDS, PCX, SGI, XPM and others | fire-decode already builds on it for PNG / HDR / TGA / TIFF / GIF. No PSD, and the slow paths above are in it |
| [`zune-image`](https://github.com/etemesi254/zune-image) | Rust | Base layer, 8/16-bit, grey/RGB/RGBA only ([`zune-psd`](https://lib.rs/crates/zune-psd), last release 2024) | No | No | Speed-first, fuzzed | fire-decode already uses its JPEG / BMP / PPM / farbfeld decoders. No DDS / EXR / TGA |
| [OpenImageIO](https://openimageio.readthedocs.io/en/latest/builtinplugins.html) | C++, Apache-2.0 | Yes (RGB, CMYK, grey, indexed, multichannel) | BC1-BC7, cube / volume / mips | Yes | The VFX-industry reader; also TGA, raw via LibRaw, HEIF, JPEG XL | Closest in coverage, but a heavy native dependency stack built for pipelines, not time-to-first-pixel; the only Rust binding found is a small third-party project ([openimageio-rs](https://github.com/ennis/openimageio-rs)) |
| [SAIL](https://github.com/HappySeaFox/sail/blob/master/FORMATS.md) | C11, MIT | Composite only | No | Simplified RGBA API | "Fast imaging library"; C++ / Python bindings | No DDS, no Rust binding |
| [libvips](https://docs.rs/crate/libvips/latest) (Rust `libvips` 2.3.0) | C, LGPL | Via its ImageMagick loader | No | Yes | Shrink-on-load thumbnails - the strongest alternative for bite's thumbnail step specifically | Native LGPL stack, PSD only through ImageMagick, no DDS; for JPEG it would match, not beat, what bite already gets from `jpeg:size` |
| [`image_dds`](https://rust-digger.code-maven.com/crates/image_dds) 0.6.1 | Rust | - | BC1-BC7 incl. BC6H | - | Wraps a bcdec port | DDS only, and hides the format table fire-decode wants to own (already rejected in fire's `Cargo.toml`) |
| [oculante](https://github.com/woelper/oculante) | Rust app, MIT | Via `psd` | DXT1-5 | Yes | A broad-format Rust image viewer | An application; its loading is not published as a reusable crate |
| [`libheif-sys`](https://docs.rs/crate/libheif-sys/latest) 5.3.1 / `libheif-rs` | Rust bindings to libheif (LGPL C++) | - | - | - | Binds libheif 1.23.1; finds a system libheif (pkg-config on Linux, vcpkg on Windows) or builds libheif from embedded sources with cmake (`embedded-libheif`) | Codecs are not bundled, so libde265 and an AV1 decoder must still be supplied. It would replace only the part `heif-sys` already does with a small wrapper, and it has no gain-map handling |

---

## 3. Decision record

### FD1 - Its own public repository, crate name unchanged

`fire-decode` moves to **`github.com/psmyles/fire-decode`**.
- **Name:** the crate keeps the name `fire-decode`, so fire's `use fire_decode::...` lines do not
  change.
- **Licence:** **`MIT OR Apache-2.0`**, as `LICENSE-MIT` + `LICENSE-APACHE`. The repo's current single
  `LICENSE` becomes `LICENSE-MIT`.
- **Dependency:** apps depend on it by git tag.
- **Visibility:** the repository exists and is **private**. It is made public before any app's
  `Cargo.toml` points at it, because fire's CI and every outside clone have to fetch it.
- **Clone cost:** a consumer's clone carries the vendored HEIF stack (FD16), about 12 MB packed, one
  copy.
- **Third-party code** keeps its own licence (FD14); only the crate's own code is dual-licensed.

Considered:
- **Keep it in fire's repo, consumed by git tag.** Least restructuring, but every consumer clones
  fire's whole repo, the API stays shaped by fire, and fire's CI would not notice a change that breaks
  another consumer. It was the recommendation for two consumers; bite makes three.
- **Fold into slab** - rejected: slab is the app shell (windowing, settings, i18n, packaging), not
  image decoding, and it stays private until stable, which a public app cannot depend on.
- **Path dependency to a sibling checkout, or copying the code** - rejected: the first breaks CI and
  every outside clone, the second is the duplication being removed.
- **MIT only** - what the repo's committed `LICENSE` and the three apps' `LICENSE` files say. Rejected
  for the Rust-ecosystem dual licence that fire's and 3D Review's `Cargo.toml` already declare.

### FD2 - What moves, and what stays in fire

**Moves:**
- the decode core: routing, every pure-Rust backend, EXIF, ICC, downscale, `decode_with_progress`,
  `level_dims`;
- the merged PSD crate (FD3) and the pure-Rust PSD layer decoder (`PsdLayers`, `psd_layers.rs`);
- **`heif-sys`: HEIF / HEIC / AVIF behind a `heif` feature (FD16)**;
- GIF animation, and `SheetLayout` (DDS arrays / volumes);
- camera raw, behind a **`raw` feature, off by default**. fire enables it; bite leaves it off so raw
  thumbnails keep matching ImageMagick.

**Stays in fire:**
- `fire-thumbs` (Explorer thumbnail settings, format groups, CPU renderer) and `fire-shellext` (the
  COM thumbnail provider). They are consumers of the crate, not part of it.
- **The installer-association test** (`installer_associations_match_the_extension_table`), which
  `include_str!`s `installer/fire.iss` and cannot compile in another repo. It moves into a fire crate
  and checks against the crate's full extension list (FD9).

**Why the October split is reversed.** The October plan kept HEIF in fire to spare the shared repo the
prebuilt libraries, bindgen and LGPL code. The cost was a public `finish()` that fire would call on
images it decoded itself. With all three apps decoding HEIF, that trade no longer holds:
- `finish()` is dropped;
- fire stops routing HEIF around the crate;
- FD16 deals with the libraries, the bindings and the licence.

### FD3 - One PSD crate, merged from both apps

Both copies started from psd_sdk `f514495` and were hardened separately, in different places:

| | fire `psd-sdk-sys` (`FIRE PATCH`, edited in place) | 3D Review `crates/psd` (`review.patch`) |
| --- | --- | --- |
| RLE | Every run bounds-checked against both buffers; tail zero-filled | P1: every run bounds-checked against source and destination |
| Image-data section | - | P2: 64-bit plane sizes refused past 32 bits; allocations null-checked; RLE payload and raw planes bounded by the section before allocating; channel count refused rather than asserted; frees on failure |
| Asserts | `PSD_ASSERT` throws `psd::AssertFailure` instead of calling `assert()` | - |
| Image resources | Greyscale `channelCount - 3` wrap fixed | Parser never called |
| Bindings | bindgen at build time | Committed, hand-kept, size / offset asserts |
| Wrapper | In-place read, threaded interleave, integer path for 8/16-bit RGB and greyscale, document's own depth | Borrowed bytes (`DocHandle<'bytes>`), `try_reserve_exact` → `OutOfMemory`, 32-bit colour sRGB-encoded in C++ |
| Limits | 131072 px, 4 GiB | 30000 px, 1 GiB output, 2 GiB planar, 56 channels |

The merge takes:
- **Both patch sets.**
  - The two RLE fixes overlap. They are reconciled into one, which lands only when both apps'
    malformed-file suites pass in the crate (fire's `tests/psd.rs`; 3D Review's `tests/synthetic.rs`,
    ~17 cases).
  - P2 and the throwing assert come over as they are.
- **From 3D Review: committed bindings** with size / offset asserts. The shared repo never runs bindgen
  or needs libclang.
- **From fire: the wrapper's speed work.** From 3D Review: its borrowed-bytes read and fallible
  allocation.
- **Limits from Rust.** The wrapper takes them from `DecodeOptions::limits` (FD5) instead of compiling
  in its own constants. The planar-data and channel caps are derived in step 2a; 3D Review's are 2× the
  output limit and 56.
- **psd_sdk's image-resources parser is never called.**
  - 3D Review stopped calling it because its `DISPLAY_INFO` branch writes through NULL for a greyscale
    document with alpha.
  - fire needs the embedded ICC profile, which lives in that section. `psd_layers.rs` already finds it
    in Rust (`find_icc`, a bounds-checked walk of the `8BIM` blocks for resource 1039), and the
    composite path uses the same function.
  - fire's greyscale-wrap patch becomes unreachable and stays as a harmless guard.
- **The `fire_psd_` C symbol prefix stays** (it matches the crate name), and the crate gains
  **`links = "fire_psd"`**.
  - Both crates export the same `fire_psd_*` symbols from a static library named `fire_psd`, and
    neither has a `links` key. So cargo would let both into one binary, and the linker could resolve
    each symbol from either copy: silent mismatches, not a link error.
  - 3D Review adds the same key to `crates/psd` in step 0, and deletes that crate in the commit that
    adopts this one (step 4).
- **Thread count from the budget:** the crate's thread budget (FD8) passes the thread count into the
  C++ rather than the wrapper choosing its own.

### FD4 - Output format, and the source described separately

`DecodeOptions` gains **`output: Native | Rgba8`**:
- `Native` is today's behaviour: 8/16-bit unorm, half or 32-bit float, as the source holds.
- `Rgba8` narrows inside the crate.
  - Where the source is already 8-bit, the narrowing is fused into the final widening pass; otherwise
    it is one threaded pass.
  - It runs **after** any `fit`, so a thumbnail narrows 256 px rather than 8K.

The buffer and the file are described separately:
- `DecodedImage::format` describes the **buffer**. (Today `bit_depth` describes the buffer.)
- A separate **`source`** describes the **file**:
  - channels, bit depth (8/10/12/16/32), sample kind and format name;
  - for colour-tagged sources such as HEIF, the **transfer** (sRGB / linear / PQ / HLG), the
    **primaries** (BT.709 / Display P3 / BT.2020 / other) and the **HDR kind** (none / PQ / HLG / gain
    map).
- 3D Review's stats stay faithful once the crate narrows, and fire uses the HDR kind to offer its HDR
  toggle (FD17).

Narrowing rules:
- **Linear float → 8-bit: clamp to 0..1 and scale, no transfer curve.** This applies to linear data
  sources (EXR height / mask maps, Radiance HDR). It is what 3D Review gets from `to_rgba8()` today.
  - Display-referred HDR (PQ / HLG HEIF) never takes this path: with `hdr: Sdr`, libheif renders it to
    SDR (FD17).
  - An sRGB-encode option is added only if bite's ImageMagick comparison calls for it (section 6).
- **16 → 8-bit rounds to nearest**, matching ImageMagick (bite's current `STRIP_16` truncates).

### FD5 - Named setups, no `Default`

`DecodeOptions` loses its `Default` (which is fire's behaviour) in favour of named constructors. Each
app's intent becomes explicit, and a new field forces a decision per setup instead of silently
inheriting fire's:

| Setup | Consumer | ICC | EXIF orientation | Output | Fit | Frames / layers | HDR (FD17) | Limits |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `viewer()` | fire | On | On | `Native` | `16384, Nearest` | All GIF / HEIF-sequence frames; DDS layout and mips kept | Set by fire per file: `Linear` for PQ / HLG, and for gain maps when the HDR toggle is on | 131072 px, 4 GiB |
| `texture(gpu_max)` | 3D Review (every binary) | Off | Off | `Rgba8` | `gpu_max, Area` | First frame / layer / face only; stored mips ignored | `Sdr` | 30000 px, 1 GiB |
| `thumbnail(size)` | bite | Off | Off | `Rgba8` | `size, Area` (shrink only) | First frame / layer / face only | `Sdr` | 30000 px, 1 GiB |

- **Every field stays public** for one-off adjustments.
  - **fire-shellext** starts from `viewer()` and sets `frames: First` and `hdr: Sdr`. Explorer shows a
    photo the way other viewers do, and the surrogate does no gain-map work.
  - **3D Review's shells** call `texture()` and keep their own `fitted_within` halving afterwards,
    unchanged.
- **Limits are per setup** (`limits: Limits { max_dim, max_bytes }`).
  - bite runs many decodes at once, and 3D Review chose 30000 px / 1 GiB deliberately. fire opens one
    image at a time and needs the headroom.
  - The same values feed libheif's security limits (FD16) and the PSD C++ (FD3).
- **ICC is a cargo feature (`icc`).** Without it, lcms2 is not built and the ICC field does not exist,
  so asking for ICC on a build without it fails to compile rather than being ignored. Extracting the
  profile bytes does not need the feature; only converting does.
- **EXIF orientation becomes a switch** (today it is unconditional). Engines' texture importers do not
  rotate by EXIF, and a rotated texture no longer matches its UVs.
- **HEIF's own rotation and mirror (`irot` / `imir`, with `clap`) are always applied, in every setup.**
  - They are part of the HEIF image's definition, not metadata.
  - libheif applies them by default, and ImageMagick is expected to as well; bite's comparison checks
    this (section 6).
  - EXIF orientation is never applied to HEIF, so nothing rotates twice.
- **Off-ICC / off-EXIF is also what keeps bite's thumbnails matching ImageMagick:** `-thumbnail` keeps
  the colour profile without converting, and ImageMagick only rotates with `-auto-orient`.
- **Wide-gamut colour without an ICC profile follows the ICC switch.** This means an nclx box declaring
  Display P3 or BT.2020, which is common in Android AVIF. It is converted to sRGB in `viewer()` and left
  as stored in `texture()` / `thumbnail()`.

### FD6 - Resizing: `fit` with a filter

`max_dim` becomes **`fit: Option<Fit { max, filter: Nearest | Area }>`** (values per setup in FD5).
- `Nearest` keeps fire's existing RAM-guard behaviour for >16K images.
- `Area` is an area average for real reductions: thumbnails, and 3D Review's rare oversize texture,
  where aliasing would read as an asset defect.
- **`Area` averages stored values, not linear light.**
  - This is what ImageMagick's `-thumbnail` does by default, so bite's previews match. It is also
    correct for data textures.
  - It deliberately differs from the mip builders (FD11) and 3D Review's `fitted_within`, which average
    colour in linear light. The crate documents why.
- `downscaled_from` is reported as today; 3D Review shows it in the texture stats.

### FD7 - Panics are caught by the crate, and unwinding is required

- **`decode()` and `probe()` wrap themselves in `catch_unwind`** and return **`DecodeError::Panicked`**.
  - bite is covered with no change of its own. It has no `catch_unwind` anywhere: today a decode panic
    inside `decode_many`'s scoped threads takes the import thread down and leaves the progress modal
    open.
  - fire's own pool wrapper becomes a harmless second layer.
- **C++ exceptions are caught in C++.**
  - `catch_unwind` cannot catch a foreign exception; one that reaches Rust aborts the process.
  - Every C entry point of both bridges catches `...`. psd_sdk's wrapper already does, and the HEIF
    wrapper becomes C++ to do the same (FD16).
  - An exception escaping one of libheif's own worker threads would call `std::terminate`. The security
    limits (FD16) bound the allocations that could throw there.
- **The crate refuses to compile under `panic = "abort"`** (via `cfg(panic = ...)`) unless an
  **`allow-abort`** feature acknowledges the guard is inert. The requirement is a build error, not a
  doc comment.
- **3D Review:**
  - Its shells (`review_shell.dll`, `libreview_quicklook.dylib`) already build with the `release-shell`
    profile (`panic = "unwind"`) and catch every entry point in `guard.rs`.
  - **The viewer's `release` profile switches from `abort` to `unwind`.** The panic hook in
    `review-log` then **aborts in the viewer binary only, unless the panicking thread is inside a
    decode**. That is a thread-local flag set around the `decode()` call.
  - The shells install the same hook (`shell-windows/src/lib.rs:53`, `shell-quicklook/src/lib.rs:54`).
    So the abort behaviour is chosen per binary when the hook is installed; otherwise it would defeat
    their guards.
  - Result: decode panics become ordinary errors, and every other viewer panic still ends the app
    loudly, as today. No `catch_unwind` has to be added to its 27 runtime thread-spawn sites. The
    preview-load worker in `review-preview`, which is not guarded, is covered by `decode()`'s own catch.
  - Binary size and startup are measured with its startup gate after the switch.

### FD8 - A process-wide thread budget

Today threads multiply:
- **one decode** splits its big passes across up to 8 threads (`par_chunks`, with `thread_budget()` =
  cores capped at 8, then by `with_max_threads`);
- **the PSD C++** starts up to 8 threads of its own;
- **libheif and dav1d** start as many as they choose: the wrapper sets no thread counts, so dav1d
  defaults to every core per decode;
- **the callers** decode several images at once: fire up to 4 workers; 3D Review a thread per texture,
  plus a scoped pool for a model's own textures; bite half the cores, max 8.

The decision:
- **One process-wide budget, capped at the core count.**
  - Each parallel pass reserves what is free and runs on the calling thread when nothing is.
  - One big image gets every core; twenty concurrent decodes get about one each.
  - No rayon: it is a counter around the scoped threads the crate already uses.
  - The budget is per binary. The two Explorer DLLs that can share a surrogate process count separately.
- **Every native and layer pass draws on it:**
  - The PSD C++ takes its thread count from the Rust side (FD3).
  - `PsdLayers`' `LayerCtl::threads` comes from the budget.
  - **libheif / dav1d** threads come from a reservation held for the whole FFI call. A grid image
    decodes its tiles on N libheif threads with one codec thread each; a single-tile image gets one
    libheif thread and N codec threads (FD16).
- `DecodeOptions::max_threads` is the override (benchmarks, determinism), replacing `with_max_threads`.
- **3D Review replaces thread-per-texture with a worker pool of half the cores** (bite's
  `default_jobs` rule).
  - It keeps its path de-duplication and generation checks, and moves its model-texture pool onto the
    same workers.
  - The budget bounds CPU, not memory: twenty concurrent 4K decodes hold ~1.3 GB of RGBA8 at once.

### FD9 - Formats: accept what the crate decodes; route before decoding

- **3D Review accepts everything the crate decodes.**
  - An FBX referencing a `.dds` (common when it was exported from an engine's content folder) shows its
    texture instead of failing, and the stats name the real source format.
  - Its texture filter comes from `supported_extensions()`. Today it is a fixed list of nine (`png jpg
    jpeg tga tif tiff psd bmp gif`) that already omits the `hdr` / `pnm` its decoder handles. HEIC /
    HEIF / AVIF, DDS, EXR and the rest arrive with the change.
  - The non-goal in `docs/ARCHITECTURE.md` changes from "no KTX2/DDS import" to "no engine-cooking
    workflows" (no KTX2, no platform formats, no compression settings). `ARCHITECTURE.md` and
    `GOTCHAS.md` are the committed rules; `CLAUDE.md` is local.
- **`sniff(bytes, ext) -> Format` becomes public**, with two extension lists:
  - **`supported_extensions()`** is feature-aware: no `.psd` without `psd`, no `.heic` without `heif`,
    and so on.
  - **`all_extensions()`** is feature-independent. fire's installer test and fire-thumbs' format-table
    test use it, because they run in CI's no-features job.
- **Routing per app:**
  - fire no longer routes anything around the crate.
  - bite routes PNG / TGA / PSD / EXR / HEIC / HEIF / AVIF to the crate and everything else (JPG, raw,
    SVG, JPEG 2000, DPX, ...) to ImageMagick.
  - 3D Review's texture dialog filters on `supported_extensions()`.
- **HEIF is recognised by the `ftyp` major brand**, as today (`avif` / `avis` → AVIF; `heic`, `heix`,
  ... → HEIC; `mif1` / `msf1` / `mif2` → HEIF).

### FD10 - Errors

- **New variants:**
  - **`Unsupported(Format)`** - a format compiled out of this build;
  - **`Cancelled`** - a decode cancelled through `DecodeCtl` (FD16);
  - **`Panicked`** (FD7).
- **`TooLarge` keeps its meaning:** a refusal, never retried by a second decoder. It is also what a HEIF
  over the setup's limits returns, before libheif allocates the image.
- **bite retries a failed file through ImageMagick on any error except `TooLarge`.**
- **3D Review maps errors to its existing message strings** (`app-notifications.ftl`). Diagnostics stay
  in English, per its invariant 12.

### FD11 - Mip builder, phase 2

fire's `crates/fire/src/render/mips.rs` moves into the crate as a **`mips` feature**, after the decode
migration is done. It is multi-format and threaded, averages in linear-light sRGB through lookup
tables, and adopts a file's own chain.
- It gains `srgb: bool`, for 3D Review's raw-average mode (data textures and the Tex viewport).
- It gains `max_levels`, so 3D Review can pass sokol's limit (`MAX_LEVELS` = `sg::MAX_MIPMAPS` = 16)
  without the crate depending on sokol.
- It uses the thread budget.
- **3D Review deletes `rhi/mips.rs`** (257 lines) and builds chains on its decode workers.
  - Today chains are built inside `Texture::rgba8_mipped_or_white` at upload. That runs on the render
    thread, and separately for each cache a texture is shown in (material, Tex viewport, UV).
  - The new chains are cached by `(path, srgb)`.
- bite leaves the feature off.

### FD12 - bite's thumbnail cache

- **Thumbnails the crate makes are written as lossless WebP through `image-webp`** (already a bite
  dependency).
  - **The source's ICC profile is embedded** (the crate returns `icc` even when `honor_icc` is off).
    ImageMagick's `-thumbnail` keeps the profile today, and the preview chain runs over the cached file.
    A thumbnail without it would shift the colours of a Display P3 HEIC or an ICC-tagged PNG in the
    preview.
  - File names stay `thumb_{hash}_{size}.webp`, so lookup, invalidation, cleanup and ImageMagick's
    preview chain are unchanged.
  - Files are larger than quality-85 lossy. If cache sizes at 1024+ px prove a problem, switch to lossy
    through libwebp (the `webp` crate).
- **Cached thumbnails are read through the crate too**, replacing bite's own `decode_png` /
  `decode_webp`.
- **`probe()` replaces `header.rs`** (FD19), so a warm re-import of a crate format runs no ImageMagick
  process.
- **A HEIC / HEIF chip** (`.heic`, `.heif`) joins the Input node's format chips, **off by default** like
  AVIF. Today a folder import never picks up `.heic` / `.heif`; only the picker and drag-and-drop do.

### FD13 - Repository setup

- **Versions:** tags `vX.Y.Z`. Each app depends with `git = "...", tag = "..."` and moves up when it
  chooses. Local cross-repo work goes through `[patch."https://github.com/psmyles/fire-decode"]` in an
  untracked `.cargo/config.toml` pointing at a local checkout.
- **Toolchain:** edition 2024. The crate and all three apps move to the **latest stable Rust**. Today
  fire is on edition 2021 with no `rust-version` and bite is on 1.87; `as_chunks` needs 1.88.
- **Lints:** 3D Review's level, `clippy::all` denied and `undocumented_unsafe_blocks` denied. The
  crate's `unsafe` is concentrated in the two bridges.
- **Cargo features:** `psd`, `heif`, `icc`, `raw`, `mips`, `allow-abort`, with **no default features**.
  Each app names what it uses: FD5's explicit-intent rule applied to the build.
- **CI (correctness only):**
  - Build, clippy and test four feature sets on Windows x64 (with `crt-static`, FD20) and macOS arm64:

    | Set | Features |
    | --- | --- |
    | viewer | `psd heif icc raw mips` |
    | texture | `psd heif mips` |
    | thumbnail | `psd heif` |
    | core | none |

  - Also check that `heif` on Windows without `crt-static` fails with heif-sys's named error.
  - Neither 3D Review (no Rust CI; `scripts/check.ps1` run by hand) nor bite (no CI) gets CI from this
    plan. This repo's CI is what catches a change that breaks one of their feature sets.
- **Performance gate (local):**
  - A bench harness times each format over a corpus directory given by an environment variable:
    fire's validation set, bite's corpus, 3D Review's textures and the HEIF fixtures (step 0). These
    cannot all be committed.
  - It runs before each tag. **No tag if any format is more than 2% slower than the previous tag.**
  - fire's TTFP benchmark remains the app-level gate when fire moves to a new tag.
- **Releases:** each tag's GitHub release carries the LGPL source tarballs (FD14).

### FD14 - Licences and consumer docs

- **The repo carries the third-party licence texts for its dependencies, grouped by feature:**
  - psd_sdk BSD-2-Clause under `psd`;
  - under `heif`: **libheif and libde265, LGPL-3.0-or-later**, with `LGPL-3.0.txt` and `GPL-3.0.txt`,
    and dav1d BSD-2-Clause;
  - lcms2 under `icc`;
  - and so on.

  libheif's headers say "version 3 of the License, or (at your option) any later version". fire's
  notices and `VENDOR.txt` say `-only`, and are corrected.
- **The LGPL relink section moves here** from fire's `THIRD-PARTY-NOTICES.md`.
  - The exact static libraries are in the tree at each tag, beside their headers, and `VENDOR.txt` is
    the recipe.
  - To relink: build the libraries from modified sources, put them in a local checkout of the tag,
    point the app at it with `[patch."https://github.com/psmyles/fire-decode"]`, and build. On macOS
    the relinked app must be re-signed.
  - Each tag's GitHub release carries the upstream source tarballs of the exact versions, plus the
    vcpkg port changes. The corresponding source then stays available from this repo rather than only
    upstream. It sits outside git history, so clones stay small.
- **fire:**
  - Its notices point at this repo's tree at the tag instead of `crates/heif-sys`.
  - Its `architecture.md` "no redistributable" claim becomes true with FD20.
- **3D Review: the HEIF stack is its first LGPL component.**
  - `licenses/` gains `LGPL-3.0.txt` and `GPL-3.0.txt`.
  - `docs/THIRD-PARTY-NOTICES.md` gains the relink section and covers `review_shell.dll` and
    `libreview_quicklook.dylib`; today it describes only "a single statically-linked executable".
  - README's "you do not need any prebuilt libraries" and ARCHITECTURE D22 are reworded: psd_sdk still
    compiles from source, and the HEIF stack arrives prebuilt with the crate.
  - Invariant 9 in `ARCHITECTURE.md` adds the crate's PSD and HEIF bridges as sanctioned FFI sites
    outside the workspace.
  - `crates/psd` is removed, along with its "vendored unmodified" notice.
  - The DDS non-goal is reworded (FD9).
- **bite:**
  - `THIRD_PARTY_LICENSES` adds libheif, libde265 and dav1d as statically linked into its binaries,
    with the relink section.
  - It also fixes its Windows section, which never says that `magick.exe` statically contains libheif
    and libde265.
  - `docs/spec.md`'s "no Sharp / no LGPL deps" and its "Avoided: ... Bun (statically links ...
    LGPL-2)" are reworded: bite now statically links LGPL-3 libraries, under the relink terms above.
  - There is no opt-out build: `bundle-magick-mac.sh --no-heic` is removed, or documented as dropping
    only ImageMagick's coder.
- **No Mac App Store.** None of the three apps plans one. Static LGPL-3 code is widely held to be
  incompatible with the App Store's terms, so a future App Store build would need `heif` off.

### FD15 - Timing

Design only for now. The October releases have shipped. The decode path sits under every app's core
workflow and under release builds, so work starts with step 0's baselines whenever it is scheduled.

### FD16 - HEIF / HEIC / AVIF in the crate

fire-decode already decodes HEIF through `heif-sys` (Appendix A). This decision covers what changes when
it moves and gains two more consumers.

**The stack, and where it lives**
- **`heif-sys` keeps statically linking prebuilt libheif + libde265 + dav1d**, built without encoders
  and without plugin loading (`VENDOR.txt`'s recipe).
- **The libraries are committed as plain files.**
  - Git LFS is not an option: cargo's git dependencies do not fetch LFS objects.
  - A build-time download would make every clean build depend on the network.
- **The extracted history drops the old binaries.**
  - `git filter-repo` removes `vendor/*/lib` and `vendor/*/include` from history; code and commit
    messages stay.
  - The re-vendored stack is committed once, before the first tag, so a consumer clones one copy
    (~12 MB packed).
  - fire's own repo keeps the old libraries for its past releases' notices.
  - Each later re-vendor adds a copy to every consumer's clone, so re-vendors are deliberate, not
    routine.
- **One vcpkg baseline, with the newest libheif and dav1d at re-vendor time, for both targets.** Today
  Windows has libheif 1.23.0 / dav1d 1.5.3 and macOS 1.23.2 / 1.5.4. The recipe adds:
  - **Windows: `x64-windows-static`** (/MT) instead of `x64-windows-static-md` (FD20).
  - **macOS: `VCPKG_OSX_DEPLOYMENT_TARGET=11.0`.** Today's libraries are built for macOS 27.0
    (`LC_BUILD_VERSION minos 27.0`), while fire and 3D Review (Quick Look extensions included) target
    11.0 and bite 12.0.
  - **Versions recorded in `VENDOR.txt`** for all three libraries and the MSVC toolset (dav1d's is
    recorded nowhere today).
  - **Checks after each re-vendor:**
    - dav1d is referenced from libheif (`nm -u ... | grep dav1d`);
    - no plugin-loading code is present (`LoadLibrary` / `dlopen` / `LIBHEIF_PLUGIN_PATH` absent),
      since that would be a planted-DLL risk in a COM surrogate or the Quick Look sandbox;
    - `minos` is 11.0.
- **Targets: x64 Windows MSVC and arm64 macOS only**, which is exactly what the three apps ship.
  - `build.rs` matches `target_env = "msvc"`. Today any `x86_64` Windows target, `windows-gnu` included,
    would try to link the MSVC libraries.
  - It requires `crt-static` on Windows.
  - Anything else gets a named `compile_error!`.
- **Committed, hand-kept bindings** for `wrapper.h`, with size / offset asserts, as for PSD (FD3).
  bindgen and libclang leave the build; the wrapper is compiled with `cc` as today.

**The wrapper (ABI v2)**
- **C++ instead of C**, with `catch (...)` around every entry point (FD7).
- **A versioned C ABI, designed now for everything the later steps need**, so the hand-kept bindings
  change once:
  - thread counts and limits;
  - progress and cancel hooks;
  - a Rust-allocated output buffer;
  - nclx passthrough and `ignore_transformations`;
  - auxiliary-image handles and sequence tracks.
- **`heif_init` once per process; `heif_deinit` never.** libheif says it must not run after `exit()`,
  so it cannot go in `DllMain` or a static destructor. Today both run on every decode.
- **Output goes into a Rust-allocated buffer**, sized from the decoded planes.
  - That is one copy fewer at peak. Today there are three: libheif's image, the wrapper's `malloc`
    copy, and a Rust `to_vec()`.
  - `check_dims` then guards the real allocation.

**Guards**
- **Size guard before decoding.** The handle's width and height are checked against the setup's
  `limits` (FD5) before `heif_decode_image`. libheif notes that the signalled size may differ from the
  decoded one, so this is the early refusal; the hard bound is next.
- **libheif security limits from Rust.** `heif_context_set_security_limits` takes the setup's limits:
  image pixels, total memory, tiles, sequence frames, colour-profile size. This replaces the wrapper's
  compiled-in 4 GiB cap.

**Threads** (FD8)
- `heif_context_set_max_decoding_threads` and the codec thread count both come from the budget
  reservation, held for the whole call: tiles × 1 codec thread for a grid image, 1 × N codec threads
  for a single-tile one.
- Today nothing is set.

**Orientation and metadata**
- `irot` / `imir` / `clap` are always applied (FD5); EXIF orientation is never applied to HEIF.
- EXIF and XMP are still read where FD17 needs them (Apple's gain-map headroom).
- The ICC profile is extracted as today; converting it needs `icc`.
- The nclx box is read and reported in `source` (transfer, primaries; FD4).
- Premultiplied alpha is un-premultiplied, as today.

**Frames**
- `First` means the primary image.
- A sequence-only file (`avis` / `msf1` with no primary item, already routed to libheif) has no
  primary image, so `First` takes the first frame of its first visual track.
- `texture()` and `thumbnail()` need this from step 2b, not only once FD18 lands.

**Progress and cancel**
- **`decode_with_progress` takes a `DecodeCtl { progress, cancel }`**, whose callback is
  `&(dyn Fn(f32) + Sync)`. libheif calls back from its own threads, and the crate keeps the reported
  progress monotonic.
- **Coverage:** libheif reports only grid tiles, so a single-tile HEIC reports just start and end.
  Sequences report per frame (FD18).
- **Cancel** maps to libheif's `cancel_decoding` and returns `DecodeError::Cancelled`, so fire can
  abandon decodes it no longer needs.

**What libheif 1.23 offers for this** (checked in the vendored headers):
- security limits;
- decoding-thread and codec-thread counts;
- progress (`total` and `load_tile` steps) and cancel;
- nclx getters and output passthrough (PQ, HLG, BT.2020);
- sequence tracks in `heif_sequences.h`: tracks, timescale, repetitions, `decode_next_image`,
  `ignore_sequence_editlist`, but no frame count;
- transform properties and `ignore_transformations`;
- auxiliary images and generic item access.

**It has no gain-map API** (FD17).

Considered:
- **Git LFS** - rejected: cargo git dependencies get pointer files.
- **Libraries as release assets downloaded by `build.rs`** - rejected: every clean build needs the
  network, and the relink story would point at assets rather than the tree.
- **Building libheif / libde265 / dav1d from source in `build.rs`** - rejected: every machine would need
  cmake, meson, ninja and nasm, and clean builds would be much slower.
- **`libheif-sys` / `libheif-rs`** - rejected: codecs are not bundled (section 2).
- **Keeping HEIF in fire** - the October decision, reversed (FD2).

### FD17 - HDR and gain maps (fire's viewer)

`DecodeOptions` gains **`hdr: Sdr | Linear`**.
- `Sdr` is today's behaviour, and the value in `texture()`, `thumbnail()` and fire-shellext.
- `Linear` with `output: Rgba8` is rejected.

**What a `Linear` buffer holds**
- **Format and scale:** `Rgba16Float`, **linear light, Rec.709 primaries, scaled so that 203 cd/m²
  (the BT.2408 SDR reference white) = 1.0**.
  - fire treats float buffers as linear and runs them through its existing exposure + Reinhard / ACES
    tonemap-to-SDR path, as it does for EXR. The only shader change is for negatives (below).
  - fire has no HDR swapchain.
- **Why half float:** it halves memory against `Rgba32Float`, and PQ's 10,000 cd/m² peak is ~49 on
  this scale, well inside half float's range.
- **PQ:** the PQ EOTF gives absolute cd/m², which is divided by 203.
- **HLG:** BT.2100 display light at a 1000 cd/m² nominal peak with system gamma 1.2, then ÷ 203. HLG's
  reference white (75% signal, ~203 cd/m²) lands at ~1.0.
- **Primaries:**
  - BT.2020 or Display P3 are converted to Rec.709 in float.
  - **Out-of-gamut colours stay outside 0..1, negative values included.**
  - The conversion uses lcms2 float transforms when `icc` is on and the image carries an ICC profile,
    otherwise matrices from the nclx primaries.
  - `icc::apply` skips float buffers today, so the HDR path does its own conversion and clears `icc`.
- **fire's tonemap must accept negatives.**
  - `reinhard(c) = c / (1 + c)` (`shader.glsl:75`, and the copy in `fire-thumbs/src/render.rs`)
    returns infinity at −1 and a large positive value below it.
  - It clamps negatives first. That is step 6a, before any HDR file reaches it.

**Gain maps**

Most HDR HEIC / AVIF photos (iPhone, recent Android) are an SDR image plus a gain map: either Apple's
HDR gain-map auxiliary image, or ISO 21496-1 (`tmap`). With `Linear`, the crate applies the gain map at
the file's full headroom into the same linear output, and fire's exposure / tonemap handles the range.
- **libheif 1.23 has no gain-map API** (no `tmap` or gain-map functions in the vendored headers).
  - The crate reads the pieces through what libheif does have: auxiliary images (Apple's URN), generic
    item data and references, `altr` entity groups, and EXIF / XMP metadata blocks.
  - It implements the ISO 21496-1 metadata parser, Apple's headroom reading (MakerNote / XMP) and the
    compositing itself.
  - The newest libheif is re-checked for a gain-map API at the re-vendor (FD16), and again before
    step 6c starts.
- **Transform order:**
  - Base and gain map are decoded with `ignore_transformations`.
  - The gain map is resampled to the base's coded size and applied.
  - Then the displayed item's `clap` / `irot` / `imir` are applied.
  - Why: a portrait iPhone photo carries `irot` on the primary item and usually none on the gain map,
    so applying each item's own transforms first would transpose one against the other. A portrait
    fixture is a gate.
- **An ISO `tmap` item can be the primary item itself.** libheif 1.23 may fail to fetch it as primary,
  so the crate falls back through the `altr` group to the SDR base.
- **When a file carries both maps, the ISO map wins** (standardised metadata).

**How fire uses it**
- **PQ / HLG files open as HDR** (`Linear`); there is no SDR image to show instead.
- **Gain-map photos open as their SDR base** (`Sdr`), as today. An HDR toggle re-decodes them with
  `Linear`.
  - Reinhard maps 1.0 to 0.5, so a gain-map photo shown as HDR at 0 stops looks dimmer than its SDR
    base. That is why HDR is opt-in, not the default.
- **fire-shellext stays SDR** (FD5).
- **`texture()` / `thumbnail()` get libheif's own SDR rendering** of a PQ / HLG file (what libheif does
  with no output nclx).
  - Whether that re-encodes the signal or reinterprets it is measured in step 2b.
  - For bite it is also compared with ImageMagick (section 6).

### FD18 - Animated AVIF and HEIF sequences

- **With `frames: All` (viewer), an `avis` or other HEIF image sequence decodes to `Animation` frames,
  as GIF does.**
  - Per-frame delays come from the track's timescale and sample durations; the loop count from
    libheif's repetitions.
  - Each frame is converted like a still (FD17).
  - When a file has both a still primary image and a sequence track, `All` decodes the sequence and
    `First` the primary image (or, for a sequence-only file, the first frame; FD16).
- **`ignore_sequence_editlist` is set.** Following the edit list on an infinitely looping file would
  never reach the end of the sequence. Looping is fire's job, using the loop count.
- **Caps are GIF's: 1 GiB / 10,000 frames.**
  - Frames arrive without a frame count, so the caps are enforced as they arrive.
  - At 1080p that is ~128 frames in 8-bit (~5 s at 24 fps) and ~64 in 16-bit or half float.
- **Truncation is flagged.**
  - `Animation` gains `truncated: bool`, set when a cap cut the sequence short.
  - GIF gets it too; it truncates silently today.
  - fire can then tell the user.
- **Progress** is reported per frame (FD16).

### FD19 - `probe()`

`probe(reader, ext_hint) -> Result<Probe, DecodeError>` returns width, height and `Format` without
decoding pixels, for every format the crate decodes.
- **It reads through a reader, not a byte prefix.** HEIF's `meta` box can follow `mdat` (libheif reads
  through a reader for this), and a TIFF's first IFD can sit at the end of the file.
- **Its dimensions are what `decode()` returns with EXIF orientation off and no `fit`.**
  - For HEIF, that is after `irot`.
  - They must equal what `magick identify` reports. Step 5's gate checks this, EXR's data vs display
    window included.
- **It returns a `Format`.** bite maps it to the `%m` names its header parser uses today (`PNG`,
  `JPEG`, `TGA`, ...).
- **It catches panics** like `decode()` (FD7).
- **bite uses it in place of `header.rs`** for every format (FD12). `magick identify` remains only for
  formats routed to ImageMagick.

### FD20 - Static CRT on Windows

The HEIF stack and psd_sdk are C++. Built against the DLL runtime, they need `MSVCP140.dll`.
- `fire.exe` and `3d-review.exe` already import it, and no installer ships the redistributable: they
  rely on it being installed.
- `bite-gui.exe` and `review_shell.dll` import only `VCRUNTIME140` today, so the crate would add the
  dependency to both.

The decision:
- **All three apps build with `crt-static`**, set as `-C target-feature=+crt-static` for
  `x86_64-pc-windows-msvc` in each app's `.cargo/config.toml`.
  - Everything compiled through `cc` follows it automatically: psd_sdk, the HEIF wrapper, and the C
    dependencies built from source.
  - The HEIF libraries are re-vendored as `/MT` (FD16).
- **No runtime DLL to install.** The shell DLLs also cannot pick up an older `msvcp140.dll` that
  another vendor's handler already loaded into the shared surrogate.
- **Each app checks that it links no other prebuilt `/MD` library** before switching; a mixed-CRT link
  fails or misbehaves.
  - The gate is an import table with no `MSVCP140` / `VCRUNTIME140*` entries.

Considered:
- **Bundling `VC_redist.x64.exe` in each installer** - about 25 MB, and it needs admin rights, while
  bite installs per-user by default.
- **Relying on it being installed** - fire's state today; it fails on a clean machine.

---

## 4. API sketch

Illustrative only - names settle during step 3 of the migration.

```rust
pub fn sniff(bytes: &[u8], ext_hint: Option<&str>) -> Format;
pub fn probe(reader: impl Read + Seek, ext_hint: Option<&str>)
    -> Result<Probe, DecodeError>;                               // FD19, catch_unwind inside
pub fn supported_extensions() -> &'static [&'static str];        // feature-aware
pub fn all_extensions() -> &'static [&'static str];              // feature-independent, for tests

pub fn decode(bytes: &[u8], ext_hint: Option<&str>, opts: &DecodeOptions)
    -> Result<DecodedImage, DecodeError>;                        // catch_unwind inside
pub fn decode_with_progress(bytes: &[u8], ext_hint: Option<&str>, opts: &DecodeOptions,
                            ctl: &DecodeCtl) -> Result<DecodedImage, DecodeError>;
pub fn decode_path(path: &Path, opts: &DecodeOptions) -> Result<DecodedImage, DecodeError>;
pub fn level_dims(w: u32, h: u32, level: u32) -> (u32, u32);

pub struct DecodeCtl<'a> {
    pub progress: &'a (dyn Fn(f32) + Sync),  // monotonic; may be called from libheif's threads
    pub cancel: &'a (dyn Fn() -> bool + Sync),
}

pub struct DecodeOptions {
    #[cfg(feature = "icc")] pub honor_icc: bool,
    pub honor_exif: bool,              // EXIF only; HEIF irot / imir / clap always applied
    pub output: Output,                // Native | Rgba8
    pub hdr: Hdr,                      // Sdr | Linear (FD17)
    pub fit: Option<Fit>,              // Fit { max: u32, filter: Filter /* Nearest | Area */ }
    pub frames: Frames,                // All | First
    pub limits: Limits,                // Limits { max_dim: u32, max_bytes: u64 }
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
    pub source: SourceInfo,            // the file
    pub alpha_opaque: bool,
    pub icc: Option<Vec<u8>>,          // returned whether or not it was applied
    pub downscaled_from: Option<(u32, u32)>,
    pub source_mips: Option<Vec<Vec<u8>>>,
    pub layout: Option<SheetLayout>,
    pub animation: Option<Animation>,  // Animation { frames, loop_count: Option<u32>, truncated: bool }
}

pub struct SourceInfo {
    pub format: Format, pub channels: u8, pub bit_depth: u8,
    pub sample: Sample,                // Unorm | Float
    pub transfer: Transfer,            // Srgb | Linear | Pq | Hlg | Unknown
    pub primaries: Primaries,          // Bt709 | DisplayP3 | Bt2020 | Other
    pub hdr: HdrKind,                  // None | Pq | Hlg | GainMap
}

pub struct Probe { pub width: u32, pub height: u32, pub format: Format }

pub enum DecodeError { Malformed(String), TooLarge(String), Ffi(String),
                       Unsupported(Format), Cancelled, Panicked, Other(String) }

#[cfg(feature = "psd")]
pub use psd_layers::{PsdLayers, LayerInfo, LayerCtl, LayerImage, LayerError, BlendMode, Rect};

#[cfg(feature = "mips")]
pub fn mip_chain(img: &DecodedImage, srgb: bool, max_levels: u32) -> Vec<Vec<u8>>;
```

---

## 5. Migration order and gates

Each step lands only when its gate passes.

| Step | Work | Gate |
| --- | --- | --- |
| 0 | **Baselines and fixtures; nothing moves.** The only code change is `links = "fire_psd"` in 3D Review's `crates/psd` (FD3). Record: fire TTFP per format, HEIC / AVIF included. 3D Review: size and import table of `3d-review.exe`, `review_shell.dll` and `libreview_quicklook.dylib`, plus its startup gate. bite: cold and warm import by format, counting `magick` processes, over a rebuilt corpus (`test_images` now holds only GUI goldens). Build HEIF fixtures: 8 / 10 / 12-bit, iPhone grid, portrait (`irot`), PQ, HLG, Apple and ISO gain maps, an infinitely looping `avis`, a sequence-only file, Display P3 nclx without ICC, `meta` after `mdat`, malformed and oversized files | Numbers and fixtures recorded |
| 1 | **Extraction and the re-vendored stack.** `git filter-repo` of `fire-decode`, `psd-sdk-sys` and `heif-sys`: history kept, vendored HEIF binaries dropped (FD16). The re-vendored stack committed once (newest, one baseline, `/MT`, macOS 11). Committed bindings for both bridges, target guards, and `links = "fire_psd"`. Licence files, LGPL texts, relink section, and a release carrying the source tarballs (FD14). Repo made public. fire moves to `crt-static` (FD20) and to the tag | fire's tests pass. TTFP within 2% per format, HEIC / AVIF included. fire-shellext thumbnails a HEIC. An AVIF decodes on both targets (dav1d linked). `minos` is 11.0. `fire.exe` and `fire_shellext.dll` import no `MSVCP140` / `VCRUNTIME140*` |
| 2a | **PSD merge (FD3).** Both patch sets, fire's wrapper speed work, 3D Review's borrowed bytes and fallible allocation, the Rust ICC scan, limits from Rust. 3D Review's PSD suites are ported into the crate | Both apps' PSD suites and malformed files pass. fire TTFP on PSDs |
| 2b | **HEIF wrapper v2 (FD16).** C++ with a versioned ABI: limits, thread counts (from today's `thread_budget()` until step 3), progress / cancel hooks, Rust-allocated buffer, init once, size guard, first frame of sequence-only files. Measure libheif's SDR output for PQ / HLG files | Malformed / oversized files return errors, never abort. Fixtures correct: bit depths, grid, portrait, sequence-only. Observed threads within the budget. fire TTFP on HEIC / AVIF |
| 3 | **API changes.** Named setups with `limits`. `output`, `source`, `hdr` (only `Sdr` implemented), `fit`, `frames`, `truncated`. `sniff` / `probe` / both extension lists. The process-wide budget (libheif included), panic guard and abort check, `DecodeCtl`, cargo features. fire moves to `viewer()`, and fire-shellext to its SDR first-frame setup | fire TTFP. Per-format bench against the previous tag. Extension tests pass under every CI feature set. `probe()` dimensions equal `decode()` dimensions over the corpus |
| 4 | **3D Review adopts `texture()`** with `heif` in every binary, plus `crt-static`. It deletes `crates/psd` and its decode code in the same commit. Release switches to `unwind` with the viewer-only decode hook (FD7). Worker pool. Licences, README, D22, invariant 9 and the non-goal | Startup gate. Size and imports per binary against step 0. Before/after pixel comparison over its textures, with differences explained (e.g. 16-bit rounding). HEIC / AVIF textures load in the viewer, the preview pane, and Quick Look via an FBX with an embedded HEIC texture (the sandbox and the thumbnail DLL reach only embedded textures). Shell panics still caught. `minos` 11.0 |
| 5 | **bite.** Licences and spec first (FD14). Rust ≥ 1.88 and `crt-static`. Cold-import benchmark by format. Route PNG / TGA / PSD / HEIC / HEIF / AVIF through the crate, and EXR after its ImageMagick comparison. `probe()` replaces `header.rs`. HEIC chip. Lossless WebP with ICC. Cached reads through the crate | Cold import faster per routed format. No `magick` process on a warm re-import of crate formats. `probe()` matches `identify`. HEIC orientation matches ImageMagick. Previews visually unchanged, ICC-tagged files included. Cache size acceptable |
| 6a | **fire: tonemap safe for negative values**, in the shader and in fire-thumbs (FD17) | Unit test with negative and below −1 inputs |
| 6b | **PQ / HLG → `Linear` half float.** `source` reports the HDR kind; fire opens PQ / HLG as HDR | Synthetic PQ / HLG fixtures numerically correct (known cd/m² → known value). SDR HEIC TTFP unchanged. `texture()` / `thumbnail()` output bit-identical to the previous tag |
| 6c | **Gain maps** (the newest libheif re-checked first), plus fire's HDR toggle | Portrait fixture aligned. Apple and ISO maps both applied, and ISO wins when both are present. SDR default unchanged |
| 6d | **Sequences (FD18), plus HEIF progress / cancel** in fire's loading bar | The infinite-loop file terminates. Truncation flagged. Progress only rises. Cancel stops a long sequence |
| 7 | **Phase 2: `mips` feature (FD11).** fire uses it; 3D Review builds chains on its workers | fire TTFP. 3D Review frame times while textures load |

3D Review goes before bite: it was the original motivation, and its gains do not depend on a benchmark.

Step 6 is fire-only and can start any time after step 3, in parallel with steps 4-5. Any tag that
3D Review or bite moves to must keep `texture()` / `thumbnail()` output unchanged.

---

## 6. To measure before or during the work

**bite**
- **Cold import, by format.**
  - Tools: `crates/bite-gui/examples/import_bench.rs` with the thumbnail cache cleared. Its first pass
    is cold only if the GUI cache (`%TEMP%\bite-preview`) is empty. `bite bench-import` uses a fresh
    cache and also counts processes.
  - Compare against a throwaway variant through the crate.
  - Run over a rebuilt corpus of PNG / TGA / PSD / EXR / HEIC / AVIF / JPG; `test_images` no longer
    holds inputs.
- **Warm re-import:** `magick` processes per format, before and after `probe()`.
- **EXR thumbnails:** compare `magick <exr>[0] -thumbnail ...` with the crate's clamp-no-curve output.
  Add an sRGB-encode option only if they differ visibly.
- **HEIC / AVIF thumbnails** against `magick <file>[0] -thumbnail ...`:
  - orientation: does ImageMagick apply `irot` / `imir`, as FD5 expects;
  - how ImageMagick renders PQ / HLG and gain-map files in SDR, against the crate's `Sdr` path;
  - decode time: dav1d (crate) against libaom (bite's ImageMagick).
- **Cache size** with lossless WebP, ICC embedded, at 256 and 1024 px on a typical folder.

**HEIF**
- **libheif's own SDR rendering of PQ / HLG** (no output nclx): does it re-encode to sRGB or
  reinterpret the signal? Measured in step 2b. It decides what 3D Review textures and bite thumbnails
  of such files look like.

**3D Review**
- **Binary size and startup** after the switch to unwind, and per binary with `heif` and `crt-static`.
- **JPEG path:** what the crate still saves over its new hand-driven `JpegDecoder`.

**Build**
- **Binary-size change from `crt-static`** in fire and bite.

Not part of this plan, noted for later:
- **3D Review audit warning:** the crate's opaque-alpha scan could drive a warning for "RGBA texture
  whose alpha is unused".
- **Embedded HEIF thumbnails** for `thumbnail()` and fire-shellext: faster, but they would not match
  ImageMagick.
- **fire's JPEG XL PQ / HLG path** could move to FD17's linear convention; today it hands floats
  through.
- **An HDR swapchain in fire** (scRGB / EDR), which FD17's linear buffers would feed directly.

---

## Appendix A - Code facts this plan rests on

As of fire `6bd14c5`, 3D Review `6c3ceaf` and bite `b511984` (November 2026).

The October version cited fire `927ee03`, 3D Review `8f033f3` and bite `9407dc5`. 3D Review's and
bite's histories have since been rewritten:
- for bite, `78be8bb` is the rewritten `9407dc5`;
- 3D Review's `8f033f3` sat on a branch that was never merged (review comments, since removed);
  `a01d4c7` is the last tree the two histories share.

**fire**

*`crates/fire-decode`*
- **Files:** `lib.rs` (3,809 lines), plus `psd_layers.rs` (1,721), `dds.rs`, `tiff.rs`, `raw.rs`,
  `exif.rs` and `downscale.rs`.
- **Features:** `default = ["psd", "heif"]`. The workspace pulls the crate with
  `default-features = false`, and `fire` / `fire-shellext` re-enable both.
- **Public API:**
  - `decode`, `decode_path`, `with_max_threads`, `is_supported_extension`, `level_dims`,
    `SUPPORTED_EXTENSIONS`;
  - `decode_with_progress(..., &dyn Fn(f32))` - only EXR and Radiance HDR report;
  - `PsdLayers` - pure Rust: RAW / RLE / ZIP channels, 8/16/32-bit, no PSB;
  - `DecodeOptions { max_dim, honor_icc }`, whose `Default` is 16384 / true;
  - `DecodeError { Malformed, TooLarge, Ffi, Other }`.
- **Limits:** `MAX_DECODE_DIM` 131072 and `MAX_DECODE_BYTES` 4 GiB. Animations are capped at 1 GiB /
  10,000 frames, and GIF truncates silently at the cap.
- **Threads:** `thread_budget()` is the core count capped at 8, then capped again by
  `with_max_threads`.
- **ICC:** lcms2, skipped for float buffers (`icc::apply`).
- **JPEG XL:** PQ / HLG files go through an f32 path with no linear convention.
- **HEIF path** (`decode_heif` → heif-sys):
  - Primary image only. 8-bit → RGBA8; deeper → RGBA16 by bit replication.
  - NULL decoding options, so libheif applies `irot` / `imir` / `clap` and converts HDR to SDR.
  - EXIF is not read for ISOBMFF, so nothing rotates twice. The raw ICC profile is extracted; nclx is
    not read. Premultiplied alpha is un-premultiplied.
  - No `check_dims` (only a 4 GiB output cap in C), no security limits, no thread settings, no
    progress.
  - `heif_init` / `heif_deinit` on every decode, with `catch_unwind` around the call.
  - Sniffed by the `ftyp` major brand only.

*`crates/heif-sys`*
- **Build:** `build.rs` runs bindgen (needs libclang) over `wrapper.h`'s two-function `fire_heif_*`
  ABI. `wrapper.c` is compiled with `cc`. The crate declares `links = "heif"`.
- **Linking:** static heif + de265 + dav1d, plus `c++` on macOS.
- **Targets:** `(windows, x86_64)` → `x64-windows` (static-md); `(macos, aarch64)` → `arm64-macos`;
  anything else gets a `compile_error!`.
- **Vendored libraries:**

  | Target | libheif | libde265 | dav1d | Size |
  | --- | --- | --- | --- | --- |
  | Windows | 1.23.0 | 1.1.1 | 1.5.3 | 52 MB, debug-stripped |
  | macOS | 1.23.2 | - | 1.5.4 | 6 MB |

  - Committed as plain files (no LFS), one copy each in history (~12 MB packed).
  - The macOS objects are built `minos 27.0`.
  - No encoders; plugin loading off.
  - The recipe is prose in `VENDOR.txt`; there is no build script.

*`crates/psd-sdk-sys`*
- psd_sdk `f514495` with `FIRE PATCH` edits: throwing assert, RLE bounds + zero-fill, greyscale
  resources wrap.
- bindgen at build time. No `links` key; the static lib is named `fire_psd`.
- Composite ICC goes through psd_sdk's resources parser.
- Limits 131072 px / 4 GiB; `try` / `catch` at every C entry.

*Thumbnail crates*
- **`fire-shellext`:**
  - COM `IThumbnailProvider` (cdylib) in `dllhost.exe` (no `DisableProcessIsolation`), built with
    unwind.
  - Calls `decode(..., &DecodeOptions::default())` with `heif` on; `catch_unwind` in `GetThumbnail`.
- **`fire-thumbs`:**
  - Settings, format groups (HEIF / AVIF default off) and the CPU renderer: linear-light box filter,
    exposure, Reinhard / ACES.
  - Its tests compare its table with `SUPPORTED_EXTENSIONS`.

*The app and render*
- `decode_pool.rs` runs up to 4 workers, with `catch_unwind` around file and layer decodes.
- `render/mips.rs` is 604 lines.
- `shader.glsl`'s `reinhard(c) = c / (1 + c)` has no clamp.

*Workspace, CI and notices*
- **Workspace:** edition 2021, no `rust-version`, license `MIT OR Apache-2.0` (the LICENSE file is
  MIT). Release `panic = "unwind"`, on purpose. `rust-toolchain` is stable with
  `x86_64-pc-windows-msvc` and `aarch64-apple-darwin`.
- **CI on Windows and macOS:** a no-default-features check job, and a full job that installs LLVM for
  bindgen.
- **C++ runtime:** `fire.exe` and `fire_shellext.dll` import `MSVCP140` / `VCRUNTIME140(_1)`, and no
  installer ships the redistributable.
- **Installer test:** `installer_associations_match_the_extension_table` `include_str!`s
  `installer/fire.iss`.
- **`THIRD-PARTY-NOTICES.md`:** libheif and libde265 are listed as LGPL-3.0-only (their headers say
  "or later") and statically linked, with relinking via `crates/heif-sys/vendor`.

**3D Review**

*Texture decode*
- **`crates/render/src/texture.rs`** (593 lines):
  - PSD, by `8BPS` magic → `review_psd`.
  - JPEG, by magic or extension → a hand-driven zune `JpegDecoder` (decode → `convert_color(RGBA)` →
    `flatten`), falling back to `image` on failure.
  - Everything else → `image::ImageReader` + `to_rgba8()`, with a 30000 px cap.
  - Output is RGBA8 plus `source_channels` / `source_bit_depth`.
  - `image` features: png, hdr, tga, tiff, jpeg, pnm, bmp, gif.
- **`texture/imported.rs`** (661, new): a model's own textures, embedded or found beside it.
  - Decoded on a scoped pool of `min(cores, count)`.
  - `fitted_within` is 2×2 halving, in linear light for sRGB slots.
- **`TEXTURE_EXTENSIONS`** (`app/src/loading/mod.rs`): `png jpg jpeg tga tif tiff psd bmp gif`, with
  no hdr / pnm. There is no HEIF / DDS / EXR handling anywhere.

*`crates/psd`*
- psd_sdk `f514495` + `review.patch` (P1, extended P2), with committed `bindings.rs` and asserts.
- `fire_psd_*` symbols and static lib `fire_psd`, no `links` key.
- The image-resources parser is not called.
- Borrowed bytes and `try_reserve_exact`.
- Limits 30000 px / 1 GiB output / 2 GiB planar / 56 channels; built with `cc`'s `/MD`.

*Texture loading and mips*
- `texture_manager.rs` (762): one thread per decode, de-duplicated by path, with generation checks.
- `rhi/mips.rs` (257): RGBA8, sRGB or raw averaging, `MAX_LEVELS` 16 = `sg::MAX_MIPMAPS`. Built in
  `Texture::rgba8_mipped_or_white` at upload, on the main thread.

*Panics*
- **Profiles:** release `panic = "abort"` (viewer); `release-shell` `panic = "unwind"`
  (`review_shell.dll`, `libreview_quicklook.dylib`).
- **Hook:** `review_log::install`'s panic hook logs, then chains. The viewer and both shells call it.
- **Guards:** `catch_unwind` in each shell's `guard.rs`.
- **Threads:** 27 runtime thread-spawn sites.

*Shells*
- **`review_shell.dll`:** Explorer thumbnails in the COM surrogate; the preview pane in
  `prevhost.exe`.
- **Quick Look dylib:** inside two sandboxed app extensions (app-sandbox only).
- **Reach and sizes:**
  - Both shells reach only embedded textures.
  - Thumbnail textures are fitted to `(size × 4).clamp(512, 2048)`; previews use 2048.
  - `review-preview`'s preview-load worker is not inside `catch_unwind`.

*Rules and build*
- **Where the rules live:** `docs/ARCHITECTURE.md` and `GOTCHAS.md` are the committed rules;
  `CLAUDE.md` is local and gitignored.
- **What they say:**
  - Invariant 9's FFI sites: import, psd, optimize, the GPU site, the shells.
  - D22: "a clean checkout links without a prebuilt library".
  - README: "you do not need any prebuilt libraries".
  - KTX2 / DDS import is a non-goal.
  - There is no LGPL component; the notices describe "a single statically-linked executable".
- **Workspace:** edition 2024, `rust-version` 1.88, `clippy::all` and `undocumented_unsafe_blocks`
  denied.
- **Checks:** no Rust CI; `scripts/check.ps1` / `check.sh` run by hand; startup gate `scripts/gate.ps1`
  / `gate.sh`.
- **Targets:** macOS arm64 only, `LSMinimumSystemVersion` 11.0; the Quick Look extensions are built
  for macOS 11.0.
- **C++ runtime:** `3d-review.exe` imports `MSVCP140`; `review_shell.dll` does not.
- **Test textures:** `assets/test_textures/T_Sides_D.psd` is the only tracked one.

**bite**

*Import and thumbnails*
- **`crates/bite-imagemagick/src/import.rs` (`ThumbnailCache`):**
  - A header probe (`header.rs`: PNG / BMP / WebP / JPEG / TGA) runs in parallel.
  - Misses are thumbnailed by `magick` in commands of 8 files (`batch_size`), up to `jobs` (cores / 2,
    max 8) at once. `MAGICK_THREAD_LIMIT` is 1 for a lone command, otherwise each command's share of
    the cores.
  - JPEGs get `-define jpeg:size=2N×2N`.
  - The command is `-thumbnail NxN> -quality 85` to `thumb_{fnv1a64(path)}_{size}.webp`, with no
    `-strip`, so profiles are kept.
  - Failed batches are retried per file; 0×0 headers get `magick identify`.
  - Each import builds a fresh `ThumbnailCache`, so every cache hit outside `header.rs`'s formats runs
    `identify`.
- **`crates/bite-gui/src/commands.rs` (`add_paths`):** calls `load_batch` 16 files at a time, then
  `work::decode_many` (png 0.18 with `STRIP_16`, image-webp 0.2).
- **`render_preview`:** runs the ImageMagick chain over the cached thumbnail.
- **`thumbnailSize`:** default 256, clamped to 64-2048.
- **Panics:** there is no `catch_unwind` anywhere. A decode panic in `decode_many`'s scope ends the
  import thread without `ImportFinished` / `ImportFailed`.
- **Inputs:** a 66-extension list, avif / heic / heif included. The Input node's format chips have AVIF
  (off) but no HEIC, so folder import skips `.heic` / `.heif`.

*ImageMagick*
- **Windows:** a static `magick.exe`, 7.1.2-34 Q16-HDRI, with heic (libheif 1.23.6, libde265, AV1 via
  libaom), openexr and LibRaw.
- **macOS:** source-built 7.1.2-34 Q16-HDRI, with heic as a loadable module (`--no-heic` drops it),
  openexr, and no raw.
- **Licence docs:** `THIRD_PARTY_LICENSES` documents LGPL only for the macOS dylibs. `docs/spec.md`
  says "no LGPL deps" and lists Bun as avoided for statically linking LGPL code.

*Workspace and build*
- **Workspace:** edition 2021, `rust-version` 1.87, release `panic` unwind. No lints, no
  `rust-toolchain`, no CI.
- **Test images:** `test_images` holds only GUI goldens.
- **C++ runtime:** `bite-gui.exe` imports only `VCRUNTIME140`.
- **Targets:** macOS arm64 only, minimum 12.0; the Windows installer is per-user, x64.
- **Benchmarks:** `examples/import_bench.rs` (two passes, GUI cache directory) and `bite bench-import`
  (fresh cache, process counts).

**libheif 1.23 (the vendored headers)**
- **Present:**
  - security limits (`heif_context_set_security_limits`);
  - `heif_context_set_max_decoding_threads` and `num_codec_threads`;
  - progress (`total` / `load_tile` steps) and `cancel_decoding`;
  - nclx getters and output passthrough;
  - sequences (`heif_sequences.h`; no frame count);
  - transform properties and `ignore_transformations`;
  - auxiliary images, generic item access, `altr` groups.
- **Absent:** any gain-map / `tmap` API.
- **Licence text:** "version 3 of the License, or (at your option) any later version".
