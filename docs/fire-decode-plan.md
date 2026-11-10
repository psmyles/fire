# fire-decode - Shared Decoder Plan

**Status: in progress.** Steps 0-5 are done on the Mac, each with its Windows half still to do;
`docs/PLAN.md` tracks them. The design was revised in November 2026, after fire, bite and 3D Review
shipped their late-October releases. That revision made three changes:
- HEIF / HEIC / AVIF moves into the crate. The October version kept it in fire.
- fire's viewer gains HDR, gain-map and image-sequence support.
- All three apps move to the static C runtime on Windows.

It also refreshes every code fact (Appendix A) against each app's current head. A second check on
10 November settled five more points (listed below).

The plan: move `fire-decode` out of fire into its own public repository and make it the one image
decoder behind three apps.

| App | What it uses the crate for |
| --- | --- |
| **fire** | Everything it decodes, HEIF / HEIC / AVIF included, in the viewer, in `fire-shellext` (its Explorer thumbnail DLL) and in `fire-cli` (Asset Check's image checker). It remains the crate's most demanding consumer |
| **3D Review** | Material and Tex-viewport textures, in every binary that links `review-render`: the viewer, `review_shell.dll` (Explorer thumbnails and preview pane), `libreview_quicklook.dylib` (Quick Look) and the snapshot tool. It replaces `texture.rs`'s decode path and `crates/psd` |
| **bite** | Import thumbnails for PNG / TGA / PSD / EXR / HEIC / HEIF / AVIF, header probes for warm re-imports, and reading its cached thumbnails, in `bite-gui` and the `bite` CLI (both through `bite-imagemagick`). ImageMagick stays for every transform and for the formats routed to it |

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

### Settled on 10 November, after checking each app's head

- **fire-cli gets its own setup, `inspect()`** (FD5): full size, no ICC conversion, and EXIF
  orientation off, so it reports the stored size Unity imports.
- **`frames` is split in two** (FD5). `frames` covers animation; the new `surfaces` covers a DDS
  file's layers, faces or slices, with `stored_mips` beside it. fire-shellext and fire-cli can then
  keep a DDS file's whole sheet while taking only an animation's first frame.
- **`texture()` has no fit and takes no argument** (FD5, FD6). 3D Review keeps uploading an
  oversize texture from its first mip that fits.
- **`texture()` returns float sources as float** (`Rgba8OrFloat`, FD4). 3D Review narrows them per
  slot, sRGB-encoding colour as its PSD path does today.
- **bite retries `TooLarge` through ImageMagick** (FD10), so images over `thumbnail()`'s limits
  keep getting thumbnails.
- **`Sdr` converts PQ / HLG to SDR itself** (FD17, step 6b). libheif hands back the encoded HDR
  signal unconverted (measured in step 2b), which looks flat and dark as sRGB. The crate decodes it
  to light, puts 203 cd/m² at white and sRGB-encodes it, so textures and thumbnails of these files
  look like the picture. bite's previews of them will differ from ImageMagick's.

3D Review has already done its step 0 `links` key, its Windows step 0 baselines and the static CRT
(FD20, recorded as its D25).

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

**Measured in step 5** (macOS): a cold import is 63-92% faster for every routed format, and a warm
re-import of PSD, EXR, HEIC or AVIF takes a millisecond or two instead of up to 0.8 s. JPG is
unchanged, as expected.

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
- `fire-thumbs` (Explorer thumbnail settings, format groups, CPU renderer), `fire-shellext` (the
  COM thumbnail provider) and `fire-cli` (Asset Check's image checker). They are consumers of the
  crate, not part of it.
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
  - **How they were reconciled (step 2a):** both bound every run against both buffers. A malformed
    run is now refused (`DecodeFailed`), as 3D Review did and as a truncated file already was.
    fire's zero-fill of the rest of the plane stays, for psd_sdk callers that ignore the result.
    fire's test that kept such an image now expects the refusal.
  - P2 and the throwing assert come over as they are.
  - **Also from 3D Review's checks:** a channel count below the colour mode's planes (RGB under 3,
    CMYK under 4) is refused. fire used to fill a missing RGB plane from the first one.
- **From 3D Review: committed bindings** with size / offset asserts. The shared repo never runs bindgen
  or needs libclang.
- **From fire: the wrapper's speed work.** From 3D Review: its borrowed-bytes read and fallible
  allocation.
  - 3D Review's 8-bit narrowing in C++ is not taken: it truncates 16-bit and sRGB-encodes 32-bit
    colour. The wrapper returns the document's own depth, as fire's does, and 3D Review narrows float
    itself (FD4).
- **Limits from Rust.** The wrapper takes them from `DecodeOptions::limits` (FD5) instead of compiling
  in its own constants. The planar-data and channel caps are derived in step 2a; 3D Review's are 2× the
  output limit and 56.
  - **As built (step 2a):** `fire_psd_decode_merged` takes `{ max_dim, max_output_bytes,
    max_planar_bytes, max_channels }`. The planar cap is 2× the output's, and channels are capped at
    56, for every setup. Until step 3 adds `limits`, fire-decode passes today's 131072 px / 4 GiB.
  - **The C ABI is staged, as 3D Review's was** (ABI version 2): open reads the header only; the
    decode checks it against the limits before psd_sdk sizes a plane; the read interleaves at the
    document's depth.
- **psd_sdk stays at `f514495`.** Upstream's three later commits (to `bfa1173`, August 2026) add PSB
  and smart objects to the document and layer parsers. They don't touch the two files the merged
  decode goes through, so they would bring an unused layer parser and no PSB merged image.
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

`DecodeOptions` gains **`output: Native | Rgba8 | Rgba8OrFloat`**:
- `Native` is today's behaviour: 8/16-bit unorm, half or 32-bit float, as the source holds.
- `Rgba8` narrows inside the crate.
  - Where the source is already 8-bit, the narrowing is fused into the final widening pass; otherwise
    it is one threaded pass.
  - It runs **after** any `fit`, so a thumbnail narrows 256 px rather than 8K.
- `Rgba8OrFloat` narrows unorm sources as `Rgba8` does, and returns float sources as `Native` would
  (half or 32-bit float RGBA). It is `texture()`'s output (FD5).
  - Only 3D Review knows whether a slot samples a texture as sRGB colour or as data, so it narrows
    float itself.

The buffer and the file are described separately:
- `DecodedImage::format` describes the **buffer**. (Today `bit_depth` mostly describes the buffer;
  PNG and DDS already report the file's own depth.)
- A separate **`source`** describes the **file**:
  - channels, bit depth (1/2/4 for low-depth and palette PNGs, otherwise 8/10/12/16/32), whether it
    is a palette image, sample kind and format name;
  - for colour-tagged sources such as HEIF, the **transfer** (sRGB / linear / PQ / HLG), the
    **primaries** (BT.709 / Display P3 / BT.2020 / other) and the **HDR kind** (none / PQ / HLG / gain
    map).
- 3D Review's stats stay faithful once the crate narrows, and fire uses the HDR kind to offer its HDR
  toggle (FD17).
- **`channels` keeps 3D Review's meaning** (`source_channels`): what the pixels carry. A CMYK JPEG
  is 3, and a PSD's extra channels beyond its transparency don't count. 3D Review reads `2 | 4` as
  "has alpha".

Narrowing rules:
- **Linear float → 8-bit (`Rgba8`): clamp to 0..1 and scale, no transfer curve.** This is bite's
  thumbnail path for EXR and Radiance HDR, and what `image`'s `to_rgba8()` does with float.
  - Display-referred HDR (PQ / HLG HEIF) never takes this path: with `hdr: Sdr`, the crate renders it
    to SDR itself, sRGB-encoded (FD17).
  - An sRGB-encode option is added only if bite's ImageMagick comparison calls for it (section 6).
  - 3D Review doesn't take this path (`Rgba8OrFloat`). It narrows float per slot. Colour slots clamp
    and sRGB-encode, as its PSD path does for 32-bit documents today (`crates/psd/src/wrapper.cpp`).
    Data slots clamp with no curve.
- **16 → 8-bit rounds to nearest**, matching ImageMagick. bite's current `STRIP_16` truncates, and
  so does 3D Review's PSD path.

**As built in 3D Review (step 4):** its `DecodedImage` stays RGBA8 and carries a float source's two
readings, the sRGB-encoded one for colour slots and the clamped one for data. The colour reading is
byte-identical to what its PSD bridge gave a 32-bit document. **The Tex viewport shows the colour
reading**, as it showed a 32-bit PSD before: linear light shown as sRGB looks too dark.

### FD5 - Named setups, no `Default`

`DecodeOptions` loses its `Default` (which is fire's behaviour) in favour of named constructors. Each
app's intent becomes explicit, and a new field forces a decision per setup instead of silently
inheriting fire's:

| Setup | Consumer | ICC | EXIF orientation | Output | Fit | Frames | Surfaces | HDR (FD17) | Limits |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `viewer()` | fire | On | On | `Native` | `16384, Nearest` | `All`: every GIF / HEIF-sequence frame | `Sheet`; stored mips kept | Set by fire per file: `Linear` for PQ / HLG, and for gain maps when the HDR toggle is on | 131072 px, 4 GiB |
| `inspect()` | fire-cli | Off | Off | `Native` | None | `First` | `Sheet`; stored mips ignored | `Sdr` | 131072 px, 4 GiB |
| `texture()` | 3D Review (every binary) | Off | Off | `Rgba8OrFloat` | None | `First` | `First`; stored mips ignored | `Sdr` | 30000 px, 1 GiB |
| `thumbnail(size)` | bite | Off | Off | `Rgba8` | `size, Area` (shrink only) | `First` | `First`; stored mips ignored | `Sdr` | 30000 px, 1 GiB |

- **Every field stays public** for one-off adjustments.
  - **fire-shellext** starts from `viewer()` and sets `frames: First`, `stored_mips: false` and
    `hdr: Sdr`. Explorer shows a photo the way other viewers do, and the surrogate does no gain-map
    work. It keeps `surfaces: Sheet`, so a cubemap's thumbnail still shows every face, as fire-thumbs
    draws it today.
  - **3D Review** calls `texture()` in every binary.
    - The viewer keeps its upload-time mip skip. A texture over the GPU's limit is uploaded from the
      first level of its chain that fits, averaged in linear light for colour slots
      (`rhi/texture.rs`, `GOTCHAS.md`). So `texture()` has no fit and takes no GPU limit, and the
      Dimension stat keeps showing the source size.
    - The shells keep their own `fitted_within` halving afterwards, unchanged.
- **Frames and surfaces are separate switches.**
  - `frames: All | First` is animation: GIF frames and HEIF sequences (FD18).
  - `surfaces: Sheet | First` is a DDS file's array layers, cube faces or volume slices. `Sheet` lays
    them out as one image described by a `SheetLayout`, as today; `First` keeps only the first.
  - `stored_mips` keeps a DDS file's own mip chain. Only `viewer()` sets it.
  - fire-shellext and fire-cli need an animation's first frame but a DDS file's whole sheet.
    Explorer's thumbnail of a cubemap draws every face, and fire-cli reports a file's surfaces from
    its `layout`.
- **Limits are per setup** (`limits: Limits { max_dim, max_bytes }`).
  - bite runs many decodes at once, and 3D Review chose 30000 px / 1 GiB deliberately. fire opens one
    image at a time and needs the headroom. fire-cli keeps the viewer's limits, as today.
  - In 3D Review only the PSD path has the 1 GiB limit today. The `image` path stops at its default
    512 MiB allocation limit, and the JPEG path at zune's default 16384 px. `texture()` applies
    30000 px / 1 GiB to every format.
  - The same values feed libheif's security limits (FD16) and the PSD C++ (FD3).
- **ICC is a cargo feature (`icc`).** Without it, lcms2 is not built and the ICC field does not exist,
  so asking for ICC on a build without it fails to compile rather than being ignored. Extracting the
  profile bytes does not need the feature; only converting does. fire-cli builds with it for
  `icc_is_srgb`, though `inspect()` converts nothing.
- **EXIF orientation becomes a switch** (today it is unconditional). Engines' texture importers do not
  rotate by EXIF, and a rotated texture no longer matches its UVs.
  - `inspect()` turns it off for the same reason. Asset Check reports the size Unity imports, which
    is the stored one. Today fire-cli reports a rotated JPEG's upright size.
- **HEIF's own rotation and mirror (`irot` / `imir`, with `clap`) are always applied, in every setup.**
  - They are part of the HEIF image's definition, not metadata.
  - libheif applies them by default, and ImageMagick is expected to as well; bite's comparison checks
    this (section 6).
  - EXIF orientation is never applied to HEIF, so nothing rotates twice.
- **Off-ICC / off-EXIF is also what keeps bite's thumbnails matching ImageMagick:** `-thumbnail` keeps
  the colour profile without converting, and ImageMagick only rotates with `-auto-orient`.
- **Wide-gamut colour without an ICC profile follows the ICC switch.** This means an nclx box declaring
  Display P3 or BT.2020, which is common in Android AVIF. It is converted to sRGB in `viewer()` and left
  as stored in `inspect()`, `texture()` and `thumbnail()`.
  - **The crate does that conversion itself (step 3).** Measured in step 2b, libheif's default
    output leaves nclx primaries as stored: a Display P3 (200, 120, 60) comes back as (200, 120, 60),
    not sRGB's (213, 115, 42).

**As built (step 3):**
- The four setups are as the table says, with `Limits::VIEWER` (131072 px, 4 GiB) and
  `Limits::TEXTURE` (30000 px, 1 GiB). `inspect()` keeps the DDS sheet and drops stored mips.
- **fire-shellext's setup lives in fire-thumbs** (`fire_thumbs::decode_options()`), so the
  every-face cubemap check runs on every OS, not only in the Windows-only DLL.
- The limits reach every backend's header guard, zune's per-axis caps, the PSD bridge and
  libheif's security limits. A GIF's frames are capped by the lower of 1 GiB and `max_bytes`.
- The nclx conversion is lcms2's, from a profile built of the declared primaries (Display P3 and
  BT.2020) with sRGB's curve on both sides, so only the gamut moves. A file with an ICC profile
  takes the profile's.

### FD6 - Resizing: `fit` with a filter

`max_dim` becomes **`fit: Option<Fit { max, filter: Nearest | Area }>`** (values per setup in FD5).
- `Nearest` keeps fire's existing RAM-guard behaviour for >16K images.
- `Area` is an area average for real reductions: bite's thumbnails. 3D Review's oversize textures
  keep its upload-time mip skip instead (FD5).
- **`Area` averages stored values, not linear light.**
  - ImageMagick's `-thumbnail` also works on stored values by default, though not as an area
    average. It samples down to 5× the target, then resizes with Lanczos (Mitchell for images with
    alpha or a palette). The two should differ only slightly; step 5's gate checks that previews look
    unchanged.
  - It deliberately differs from the mip builders (FD11) and 3D Review's `fitted_within`, which average
    colour in linear light. The crate documents why.
- `downscaled_from` is reported as today; fire's viewer shows it.

**As built (step 3):** `Area` is separable over fractional spans, on the budget's threads.
Colour is weighted by alpha, so a transparent pixel lends the average no colour (ImageMagick's
resize does the same). PSD layers always shrink nearest-neighbour, so they land on the pixels of
the composite fire shrank the same way.

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
    loudly, as today. No `catch_unwind` has to be added to its 27 runtime thread-spawn sites.
  - Two preview-load workers sit outside the shells' guards: `review-preview`'s and the Windows
    preview pane's (`shell-windows/src/preview/pane.rs`). A decode panic in either is covered by
    `decode()`'s own catch.
  - **review3d-cli** shares the release profile, so the switch reaches it too. It decodes no images
    and installs no panic hook, so after the switch a panic exits with Rust's code 101 instead of
    aborting. Step 4 checks that Asset Check still reports that as the tool failing.
  - Binary size and startup are measured with its startup gate after the switch.

**As built (step 3):** `decode()`, `decode_with_progress()` and `probe()` run inside one
`catch_unwind`; the per-bridge ones it replaces mapped a panic to `Ffi`. `PsdLayers::decode_layer`
keeps its own. The `panic = "abort"` check is a `compile_error!` under
`cfg(all(panic = "abort", not(feature = "allow-abort")))`.

**As built in 3D Review (step 4):** a panic hook has to tell a decode's panic from any other on
the panicking thread, and a decode can panic on one of its passes' helper threads, which a flag the
app set around `decode()` wouldn't cover. So the crate says itself: **`fire_decode::in_decode()`**
is true inside `decode()`, `probe()` and `PsdLayers::decode_layer`, and on their helper threads.
3D Review's release viewer installs `review_log::install_aborting(.., in_decode)`; a debug viewer
keeps unwinding, as it always has.

### FD8 - A process-wide thread budget

Today threads multiply:
- **one decode** splits its big passes across up to 8 threads (`par_chunks`, with `thread_budget()` =
  cores capped at 8, then by `with_max_threads`);
- **fire's PSD C++** starts up to 8 threads of its own (3D Review's starts none);
- **libheif and dav1d** start as many as they choose: the wrapper sets no thread counts, so dav1d
  defaults to every core per decode;
- **the callers** decode several images at once: fire up to 4 workers; fire-cli up to 4, each held
  to its share of the cores with `with_max_threads`; 3D Review a thread per texture, plus a scoped
  pool for a model's own textures; bite half the cores, max 8.

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
  fire-cli drops its per-worker share, because the budget divides the cores itself.
- **3D Review replaces thread-per-texture with a worker pool of half the cores** (bite's
  `default_jobs` rule).
  - It keeps its path de-duplication and generation checks, and moves its model-texture pool onto the
    same workers.
  - The budget bounds CPU, not memory: twenty concurrent 4K decodes hold ~1.3 GB of RGBA8 at once.

**As built (step 3), with what measuring on the 14-core M3 Max changed:**
- **The budget is the core count less two, never below the old `min(cores, 8)`**, not the whole
  core count. Leasing all 14 made fire open a 12 MP HEIC 2% slower than step 2b's 8: the decode
  was faster, but fire's startup on its main thread was slower. Twelve opened it as fast as 8 did,
  and a 48 MP HEIC 7% faster.
- **A pass over memory takes at most 8 threads**: widening, interleaving, narrowing, the alpha
  scan. Fourteen gained nothing on 64-256 MiB buffers and cost 0.05 ms on 8 MiB ones, 3% of a
  2 ms TGA. What is bound by computation takes every core the budget has free: the colour
  transform (lcms2, ~9 ns a pixel), the area filter, and libheif and its codecs for the whole call.
- Below 1 MiB a pass stays on the calling thread. A pass is cut into four parts per thread, taken
  from a shared queue, so an efficiency core takes fewer of them instead of holding the pass up.
- The PSD bridge leases for its whole call too; `PsdLayers` leases per stage, up to what
  `LayerCtl::threads` asks.

**As built in 3D Review (step 4):** pool imports and reloads run on a fixed set of workers, half
the cores. A model's own textures keep their scoped pool in `render`'s `imported.rs`, sized by the
same rule, rather than moving onto the app's workers: that loader also serves the preview and both
shells, which have no app pool.

**Measured in bite (step 5): concurrent callers must cap each decode.** A lease takes the threads
that are free when the decode starts and keeps them to the end, without waiting. So of three iPhone
HEICs decoded at once, the first took nearly every core and the other two ran libheif on one thread
each: 637 ms, against 243 ms one after another. Capping each at a third of the cores
(`max_threads`) took it to 219 ms, and three AVIFs from 246 to 120 ms. PNG, PSD and EXR didn't
change. bite caps each concurrent decode at the cores divided by the decodes. A fairer lease in the
crate (a share of the cores per decode in flight) would make the cap unnecessary; it would change
fire's timings, so it is not done here.

### FD9 - Formats: accept what the crate decodes; route before decoding

- **3D Review accepts everything the crate decodes.**
  - An FBX referencing a `.dds` (common when it was exported from an engine's content folder) shows its
    texture instead of failing, and the stats name the real source format. That needs a UI change:
    today the Format row shows the file's extension (`ui/src/state/texture_view.rs`).
  - Its texture filter comes from `supported_extensions()`. Today it is a fixed list of nine (`png jpg
    jpeg tga tif tiff psd bmp gif`) that already omits the `hdr` / `pnm` its decoder handles. HEIC /
    HEIF / AVIF, DDS, EXR and the rest arrive with the change.
  - The same list decides drag-and-drop: a dropped file not on it opens as a model
    (`app/src/loading/mod.rs`).
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
    SVG, JPEG 2000, DPX, ...) to ImageMagick. Routing is by extension, so `.psb` (which the crate
    doesn't read) and Radiance `.hdr` stay on ImageMagick, though bite's format chips group them with
    PSD and EXR.
  - 3D Review's texture dialog filters on `supported_extensions()`.
- **HEIF is recognised by the `ftyp` major brand**, as today (`avif` / `avis` → AVIF; `heic`, `heix`,
  ... → HEIC; `mif1` / `msf1` / `mif2` → HEIF).

**As built (step 3):** `sniff` names the format by magic, then by extension for the formats
without magic (TGA) or sharing it (the TIFF-structured raws), and is `Format::Unknown` otherwise.
It answers whatever the build decodes, so a PSD is `Format::Psd` in a build without `psd`, and its
decode is `Unsupported(Format::Psd)`.

**As built in bite (step 5):** `in_process::is_routed` sends PNG, TGA, PSD, EXR, HEIC, HEIF and
AVIF to the crate by extension. EXR is routed after its comparison: ImageMagick shows EXR clamped
with no curve, as the crate does, so no sRGB option was added. bite enables `heif` for Windows and
macOS only, since heif-sys has libraries for no other target: its CLI still builds on Linux, where
a HEIC fails in the crate and goes to ImageMagick through FD10's retry.

### FD10 - Errors

- **New variants:**
  - **`Unsupported(Format)`** - a format compiled out of this build;
  - **`Cancelled`** - a decode cancelled through `DecodeCtl` (FD16);
  - **`Panicked`** (FD7);
  - **`NoMergedImage`** - a PSD saved without Maximize Compatibility.
- **`NoMergedImage` is already in fire's tree** (fire's PLAN.md A1, for Asset Check's
  `texture.psd_composite`). It was added on its own, ahead of the rest: `Unsupported(Format)` needs
  the public `Format` (FD9), which doesn't exist yet.
  - It covers two cases. One is psd_sdk finding no image data section. The other is the one
    Photoshop writes: a **white placeholder** merged image, marked only by `hasRealMergedData = 0`
    in the Version Info resource (1057). fire-decode reads that flag before psd_sdk runs.
  - The placeholder decodes cleanly, so without the flag it showed as a white canvas.
- **`TooLarge` keeps its meaning:** a refusal, never retried by a second decoder in the same process.
  It is also what a HEIF over the setup's limits returns, before libheif allocates the image.
- **bite retries a failed file through ImageMagick on any error, `TooLarge` included.**
  - The no-retry rule protects the process the crate runs in. ImageMagick runs in a process of its
    own, so a file it can't handle can't take bite down.
  - bite's ImageMagick has no size limits (its `policy.xml` sets none), so images over `thumbnail()`'s
    30000 px / 1 GiB get thumbnails today. They keep getting them, at today's speed.
- **3D Review maps errors to its existing message strings** (`app-notifications.ftl`). Diagnostics stay
  in English, per its invariant 12.

**As built in bite (step 5):** any error from the crate sends the file to ImageMagick, alone, after
the batch. A 30001×2 PNG, which `thumbnail()` refuses, gets its thumbnail that way (a test).

### FD11 - Mip builder, phase 2

fire's `crates/fire/src/render/mips.rs` moves into the crate as a **`mips` feature**, after the decode
migration is done. It is multi-format and threaded, averages in linear-light sRGB through lookup
tables, and adopts a file's own chain.
- It gains `srgb: bool`, for 3D Review's raw-average mode (data textures and the Tex viewport).
- It gains `max_levels`, so 3D Review can pass sokol's limit (`MAX_LEVELS` = `sg::MAX_MIPMAPS` = 16)
  without the crate depending on sokol.
- It uses the thread budget.
- **3D Review stops building chains with `rhi/mips.rs`** (257 lines) and builds them on its decode
  workers.
  - Today chains are built inside `Texture::rgba8_mipped_or_white` at upload. That runs on the render
    thread, and separately for each cache a texture is shown in (material, Tex viewport, UV).
  - The new chains are cached by `(path, srgb)`. The upload-time mip skip (FD5) then draws on them.
  - The file can't simply be deleted. `render/build.rs` compiles it in (`#[path]`) for the
    UV-checker bake, which also hashes it (`build/uv_checker.rs`). Step 7 either moves the bake onto
    the crate's builder or keeps the file for the bake alone.
- bite leaves the feature off.

### FD12 - bite's thumbnail cache

- **Thumbnails the crate makes are written as lossless WebP through `image-webp`.** It is already a
  dependency of `bite-gui`; `bite-imagemagick`, where `ThumbnailCache` lives, adds it.
  - **The source's ICC profile is embedded** (the crate returns `icc` even when `honor_icc` is off),
    as ImageMagick's `-thumbnail` keeps it today.
    - This is for parity, not colour: bite does no colour management. Neither of its decoders reads
      ICC, and no node uses `-profile`, so a thumbnail without the profile would look the same.
    - With it, the preview chain, which runs over the cached file, reads what ImageMagick writes
      today.
  - File names stay `thumb_{hash}_{size}.webp`, so lookup, invalidation, cleanup and ImageMagick's
    preview chain are unchanged.
    - The names carry no version, so an existing quality-85 file stays in use until its source
      changes or the cache is pruned. Comparisons start from an empty cache (section 6).
  - Files are larger than quality-85 lossy. If cache sizes at 1024+ px prove a problem, switch to lossy
    through libwebp (the `webp` crate).
- **Cached thumbnails are read through the crate too**, replacing bite's own `decode_png` /
  `decode_webp`. That includes the preview's output PNG, which `decode_png` reads today.
- **`probe()` replaces `header.rs`** (FD19), so a warm re-import of a crate format runs no ImageMagick
  process.
- **A HEIC / HEIF chip** (`.heic`, `.heif`) joins the Input node's format chips, **off by default** like
  AVIF. Today a folder import never picks up `.heic` / `.heif`; only the picker and drag-and-drop do.

**As built (step 5):**
- **A thumbnail is written without alpha when every pixel is opaque**, scanned on the thumbnail
  itself. ImageMagick's lossy WebP dropped an alpha plane that held nothing too, so the preview
  chain sees the channels it always saw. (`alpha_opaque` can't decide it: it is false for a source
  with no alpha channel at all.)
- **Written to a file of its own and renamed over the name**, so a cancelled import or the preview
  worker writing the same thumbnail never leaves a half file that reads as valid.
- **Crate and ImageMagick thumbnails are made at the same time**, the crate's on up to
  `default_jobs()` threads, each decode capped to its share of the cores (FD8).
- **Cached reads take `thumbnail()` without its fit and with `Limits::VIEWER`**, so a preview a chain
  enlarged past 30000 px still shows. A cache file the crate can't read still goes through
  ImageMagick, as before.
- **Lossy after all, at quality 85, through libwebp** (decided 10 November). Lossless files were
  three to seven times the quality-85 ones: 63 KiB for a HEIC at the default 256 px, 780 KiB at
  1024 (`docs/baselines.md`). libwebp is already built by the crate (libwebp-sys), and its simple
  API at 85 writes byte-for-byte what ImageMagick's `-quality 85` writes for the same pixels. It
  writes no ICC profile, so bite adds the `VP8X` header and `ICCP` chunk itself. The cache is the
  size it was.
- **The HEIC chip sits beside AVIF**, and the eleventh chip starts a third row.

### FD13 - Repository setup

- **Versions:** tags `vX.Y.Z`. Each app depends with `git = "...", tag = "..."` and moves up when it
  chooses. Local cross-repo work goes through `[patch."https://github.com/psmyles/fire-decode"]` in an
  untracked `.cargo/config.toml` pointing at a local checkout.
- **Toolchain:** edition 2024. The crate and all three apps move to the **latest stable Rust**. Today
  fire is on edition 2021 with no `rust-version`, and bite declares 1.87, which only its `xtask`
  inherits; `as_chunks` needs 1.88.
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

**As built (step 3):** features `psd`, `heif`, `icc`, `raw` and `allow-abort`; `mips` arrives in
step 7. CI runs core, psd, psd + heif (thumbnail and texture until `mips`) and psd + heif + icc +
raw (viewer). The local performance gate is `bench/decode-formats`, built against this checkout
or, with `--features old-api`, an older one.

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
  - `THIRD_PARTY_LICENSES` adds libheif, libde265 and dav1d as statically linked into `bite-gui` and
    `bite` (the CLI, through `bite-imagemagick`), with the relink section.
  - It also fixes its Windows section, which never says that `magick.exe` statically contains libheif
    and libde265.
  - `docs/spec.md`'s "no Sharp / no LGPL deps" and its "Avoided: ... Bun (statically links ...
    LGPL-2)" are reworded: bite now statically links LGPL-3 libraries, under the relink terms above.
  - There is no opt-out build: `bundle-magick-mac.sh --no-heic` is removed, or documented as dropping
    only ImageMagick's coder.
    - `THIRD_PARTY_LICENSES` already overstates it. It says `--no-heic` leaves no copyleft library
      in the app, but libltdl (LGPL-2.1+) stays; only libde265 and libx265 (GPL-2.0+) go with the
      coder.
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
    11.0 and bite's Info.plist says 12.0.
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
- **Wired in step 3:** `DecodeCtl` reaches libheif, and cancel is also polled between the decode's
  stages for every format. fire passes its loading bar's progress and, until 6d, a cancel that
  never fires.

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
- `Sdr` is today's behaviour for every file but PQ / HLG ones, which it converts to SDR (below), and
  the value in `texture()`, `thumbnail()` and fire-shellext.
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

Most HDR HEIC / AVIF photos (iPhone, recent Android) are an SDR image plus a gain map: Apple's HDR
gain-map auxiliary image, ISO 21496-1 (`tmap`), or both. Recent iPhones write both. With `Linear`, the
crate applies the gain map at the file's full headroom into the same linear output, and fire's
exposure / tonemap handles the range.
- **libheif 1.23 has no gain-map API** (no `tmap` or gain-map functions in the vendored headers).
  - The crate reads the pieces through what libheif does have: auxiliary images (Apple's URN), generic
    item data and references, `altr` entity groups, and EXIF / XMP metadata blocks.
  - It implements the ISO 21496-1 metadata parser, Apple's headroom reading (MakerNote / XMP) and the
    compositing itself.
  - The newest libheif is re-checked for a gain-map API at the re-vendor (FD16), and again before
    step 6c starts.
- **Transform order:**
  - Base and gain map are decoded with `ignore_transformations`.
  - The gain map is resampled to the base's coded size and applied. Its own transforms are not used.
  - Then the **base's** `clap` / `irot` / `imir` are applied, then the `tmap` item's own. Apple's
    files give the `tmap` item none.
- **Why that order:** Apple's writers store the gain map in the base's coded orientation, whatever
  its own `irot` says.
  - A portrait iPhone photo gives the base and the gain map the same `irot` 270. ImageIO's ISO
    encoder gives the gain map `irot` 0.
  - Both give the `tmap` item the rotated size and `irot` 0.
  - So applying only the displayed (`tmap`) item's transforms would leave every Apple portrait
    unrotated. Applying each input's own transforms before compositing would turn ImageIO's map
    against its base.
  - Checked on three iPhone photos and on ImageIO's own HDR decode (`fixtures/heif/README.md`). A
    portrait fixture is a gate.
- **An ISO `tmap` item can be the primary item itself.** libheif 1.23.6 can't decode it as primary
  (checked), so the crate falls back through the `altr` group to the SDR base.
- **When a file carries both maps, the ISO map wins** (standardised metadata).
  - This is the usual case: every iPhone photo checked carries both.
  - The iPhone's `tmap` item takes Apple's gain-map image as its gain-map input, so the two maps
    share pixels and differ only in their metadata.
  - ImageIO picks the ISO map too.

**How fire uses it**
- **PQ / HLG files open as HDR** (`Linear`); there is no SDR image to show instead.
- **Gain-map photos open as their SDR base** (`Sdr`), as today. An HDR toggle re-decodes them with
  `Linear`.
  - Reinhard maps 1.0 to 0.5, so a gain-map photo shown as HDR at 0 stops looks dimmer than its SDR
    base. That is why HDR is opt-in, not the default.
- **fire-shellext stays SDR** (FD5).
- **libheif's own SDR rendering of a PQ / HLG file reinterprets the signal** (measured in step 2b).
  libheif applies the YCbCr matrix but neither the PQ / HLG transfer nor the BT.2020 primaries: the
  RGB it returns is the encoded signal, exactly (a PQ code of 594, 203 cd/m², comes back as 594).
  Shown as sRGB, a PQ file looks flat and dark. An HLG one is closer, its curve being gamma-like, but
  dim (reference white lands at 75% grey), and both keep BT.2020's colours unconverted. fire shows it
  that way today.
- **So `Sdr` converts PQ / HLG itself** (settled on 10 November; `texture()`, `thumbnail()`,
  fire-shellext, and any `Sdr` decode):
  - the same decode to light as `Linear` (PQ EOTF, or HLG at 1000 cd/m² with gamma 1.2), ÷ 203, and
    the primaries to Rec.709;
  - then each channel clamped to 0..1, so 203 cd/m² is white and anything brighter clips, and
    sRGB-encoded into `Rgba16Unorm` (`Native`) or `Rgba8`. No tonemap curve: highlights above
    reference white are the part an SDR rendering gives up;
  - a shoulder above white is added only if the step 6b fixtures or bite's comparison show the clip
    reads badly on real files.
  - bite's ImageMagick almost certainly shows the raw signal as libheif does, so bite's previews of
    PQ / HLG files will differ from ImageMagick's (section 6). That is the intent.

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

**Done early (step 3):** `Animation::truncated`, set for GIF when a cap cuts the walk short.

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
  - `header.rs` has a second caller, `Magick::metadata`, used by every workflow run and a file's
    first preview. It takes the size from `probe()` too. Its `identify` reads of depth, resolution
    and EXIF stay.

**As built (step 3):** each format is read the cheapest exact way:
- **a few bytes of header:** PSD, QOI, farbfeld, PNM (zune's header reader), WebP (libwebp's),
  DDS (its header and the file length, for the layers the data backs);
- **walking with seeks:** JPEG's markers to the Start-Of-Frame, past EXIF thumbnails and ICC
  chunks. HEIF's top-level boxes, to read `meta` wherever it is: the primary item's `ispe`, then
  its `clap` and `irot` in `ipma` order, as libheif computes it (no libheif call). A sequence-only
  file takes its first visual track's sample size;
- **lazy header readers:** TIFF (the `tiff` crate), OpenEXR (the first layer with R, G and B), and
  the `image` crate's for PNG, GIF, BMP, TGA, ICO / CUR and Radiance HDR;
- **the whole file:** JPEG XL (jxl-oxide parses before it can say), and camera raw (the embedded
  preview's size).

It equals `decode()` with `inspect()` on every format built in its tests, every HEIF fixture,
and 188 images of fire's, bite's, 3D Review's and CrimeCrew's.

**As built in bite (step 5):** `header.rs` is gone. The cache and `Magick::metadata` both take the
size from `probe()`, and the format as `identify`'s `%m` spells it (`JPEG`, `HEIC`, `HEIF`, `FF`,
`PGM`, ...). It agrees with `identify` on all 115 files of the corpus and the fixtures that
`identify` can read, the portrait HEIC included. On the 14 it can't (malformed files, size
bombs), `probe()` still reports the header's size, and the decode then fails as it would have.

### FD20 - Static CRT on Windows

The HEIF stack and psd_sdk are C++. Built against the DLL runtime, they need `MSVCP140.dll`.
- `fire.exe`, `fire-cli.exe` (Asset Check's image checker, in fire's workspace) and
  `fire_shellext.dll` already import it, and no installer ships the redistributable: they rely on it
  being installed.
- `bite-gui.exe` imports only `VCRUNTIME140` today, so the crate would add the dependency. `bite.exe`
  links the crate too (FD14).
- **3D Review has already switched** (its D25, 4 November 2026). Every binary links both runtimes
  statically, and none imports `MSVCP140`, `VCRUNTIME140*` or `api-ms-win-crt-*`.

The decision:
- **All three apps build with `crt-static`**, set as `-C target-feature=+crt-static` for
  `x86_64-pc-windows-msvc` in each app's `.cargo/config.toml`. 3D Review's is in place.
  - Everything compiled through `cc` follows it automatically: psd_sdk, the HEIF wrapper, and the C
    dependencies built from source.
  - The HEIF libraries are re-vendored as `/MT` (FD16).
  - A `RUSTFLAGS` environment variable replaces the config's rustflags rather than adding to them.
    So a build that sets one must carry the flag itself.
- **No runtime DLL to install.** The shell DLLs also cannot pick up an older `msvcp140.dll` that
  another vendor's handler already loaded into the shared surrogate.
- **Each app checks that it links no other prebuilt `/MD` library** before switching; a mixed-CRT link
  fails or misbehaves.
  - The gate is an import table with no `MSVCP140`, `VCRUNTIME140*` or `api-ms-win-crt-*` entries,
    as 3D Review checks it.

Considered:
- **Bundling `VC_redist.x64.exe` in each installer** - about 25 MB, and it needs admin rights, while
  bite installs per-user by default.
- **Relying on it being installed** - fire's state today; it fails on a clean machine.

---

## 4. API, as built in step 3

The names settled in step 3 (10 November 2026). What is still to come is marked with its step.

```rust
// Formats (FD9, FD19)
pub fn sniff(bytes: &[u8], ext_hint: Option<&str>) -> Format;   // a few KiB of the start suffice
pub fn probe(reader: impl Read + Seek, ext_hint: Option<&str>)
    -> Result<Probe, DecodeError>;                              // catch_unwind inside
pub fn supported_extensions() -> &'static [&'static str];       // follows the features
pub fn all_extensions() -> &'static [&'static str];             // whatever the features, for tests
pub fn is_supported_extension(ext: &str) -> bool;               // supported_extensions(), any case
#[non_exhaustive] pub enum Format { Png, Jpeg, Gif, Bmp, Tiff, Tga, Ico, Cur, WebP, Qoi, Pnm,
    Farbfeld, JpegXl, RadianceHdr, Exr, Dds, Psd, Heic, Heif, Avif, CameraRaw, Unknown }
impl Format {                                                   // name(): "PNG", "OpenEXR", ...
    pub fn name(self) -> &'static str;
    pub fn is_supported(self) -> bool;                          // compiled into this build
    pub fn from_extension(ext: &str) -> Option<Format>;
}
pub struct Probe { pub width: u32, pub height: u32, pub format: Format }

// Decoding
pub fn decode(bytes: &[u8], ext_hint: Option<&str>, opts: &DecodeOptions)
    -> Result<DecodedImage, DecodeError>;                       // catch_unwind inside
pub fn decode_with_progress(bytes: &[u8], ext_hint: Option<&str>, opts: &DecodeOptions,
                            ctl: &DecodeCtl) -> Result<DecodedImage, DecodeError>;
pub fn decode_path(path: &Path, opts: &DecodeOptions) -> Result<DecodedImage, DecodeError>;
pub fn level_dims(w: u32, h: u32, level: u32) -> (u32, u32);
#[cfg(feature = "icc")] pub fn icc_is_srgb(profile: &[u8]) -> bool;

pub struct DecodeCtl<'a> {                     // DecodeCtl::NONE: no reports, never cancelled
    pub progress: &'a (dyn Fn(f32) + Sync),    // only rises; may be called from libheif's threads
    pub cancel: &'a (dyn Fn() -> bool + Sync), // polled between stages and by libheif
}

pub struct DecodeOptions {                     // no Default
    #[cfg(feature = "icc")] pub honor_icc: bool,  // ICC, and wide-gamut nclx with no profile
    pub honor_exif: bool,                      // EXIF (and a raw preview's) orientation only
    pub output: Output,                        // Native | Rgba8 | Rgba8OrFloat
    pub hdr: Hdr,                              // #[non_exhaustive] Sdr; Linear arrives in 6b
    pub fit: Option<Fit>,                      // Fit { max: u32, filter: Filter::{Nearest, Area} }
    pub frames: Frames,                        // All | First
    pub surfaces: Surfaces,                    // Sheet | First
    pub stored_mips: bool,
    pub limits: Limits,                        // Limits { max_dim: u32, max_bytes: u64 }
    pub max_threads: Option<usize>,            // caps this decode, the calling thread included
}
impl DecodeOptions {
    pub fn viewer() -> Self;                   // FD5's table, below
    pub fn inspect() -> Self;
    pub fn texture() -> Self;
    pub fn thumbnail(size: u32) -> Self;
}
impl Limits { pub const VIEWER: Limits; /* 131072 px, 4 GiB */ pub const TEXTURE: Limits; /* 30000 px, 1 GiB */ }

pub struct DecodedImage {
    pub pixels: Vec<u8>, pub width: u32, pub height: u32,
    pub format: PixelFormat,                   // the buffer
    pub source: SourceInfo,                    // the file
    pub alpha_opaque: bool,
    pub icc: Option<Vec<u8>>,                  // returned whether or not it was applied
    pub downscaled_from: Option<(u32, u32)>,
    pub source_mips: Option<Vec<Vec<u8>>>,
    pub layout: Option<SheetLayout>,
    pub animation: Option<Animation>,          // Animation { frames, truncated: bool };
}                                              // loop_count arrives with sequences in 6d

pub struct SourceInfo {                        // SourceInfo::new(format, name, channels, depth)
    pub format: Format,
    pub name: &'static str,                    // "Nikon NEF", "DDS BC7 sRGB": the status bar's
    pub channels: u8, pub bit_depth: u8, pub palette: bool,
    pub sample: Sample,                        // Unorm | Float
    pub transfer: Transfer,                    // Srgb | Linear | Pq | Hlg | Other
    pub primaries: Primaries,                  // Bt709 | DisplayP3 | Bt2020 | Other
    pub hdr: HdrKind,                          // None | Pq | Hlg | GainMap (GainMap from 6c)
}                                              // the four enums are #[non_exhaustive]

#[non_exhaustive]
pub enum DecodeError { Malformed(String), TooLarge(String), NoMergedImage, Unsupported(Format),
                       Cancelled, Panicked, Ffi(String), Other(String) }

pub use psd_layers::{PsdLayers, LayerInfo, LayerCtl, LayerImage, LayerError, BlendMode, Rect};

#[cfg(feature = "mips")]                       // step 7
pub fn mip_chain(img: &DecodedImage, srgb: bool, max_levels: u32) -> Vec<Vec<u8>>;
```

**Changed from the October sketch while building it:**
- **`with_max_threads` and `SUPPORTED_EXTENSIONS` are gone**, for `max_threads` and the two
  extension functions. `is_supported_extension` stays: fire's folder navigation and fire-cli's
  folder walk use it.
- **`SourceInfo` has a `name`**, the label fire's status bar shows, more specific than
  `Format::name` (a DDS file's pixel format, a raw's camera family).
- **`Hdr` has only `Sdr` until 6b.** A `Linear` that decoded as `Sdr` would be a promise the type
  doesn't keep; the enum is `#[non_exhaustive]` so 6b's variant isn't a breaking change.
- **`Animation::loop_count` waits for 6d**, with the sequences it is read from. GIF has no loop
  count in the `image` crate's decoder, and fire loops every animation today.
- **`.psb` stays in both lists**, as `Format::Psd`. The crate reads PSD only, so a PSB fails to
  decode, but fire-cli checks one from its header and fire's installer registers it.

---

## 5. Migration order and gates

Each step lands only when its gate passes.

| Step | Work | Gate |
| --- | --- | --- |
| 0 | **Baselines and fixtures; nothing moves.** The only code change is `links = "fire_psd"` in 3D Review's `crates/psd` (FD3), done. Record: fire TTFP per format, HEIC / AVIF included. 3D Review: size and import table of `3d-review.exe`, `review_shell.dll` and `libreview_quicklook.dylib`, plus its startup gate (the Windows half done on 4 November, in its `docs/PLAN.md`). bite: cold and warm import by format, counting `magick` processes, over a corpus with PSD, EXR and HEIF in it (`test_images` is untracked and holds only JPG, TGA and PNG), the warm import measured as the GUI does it (section 6). Build HEIF fixtures: 8 / 10 / 12-bit, iPhone grid, portrait (`irot`), PQ, HLG, Apple and ISO gain maps, an infinitely looping `avis`, a sequence-only file, Display P3 nclx without ICC, `meta` after `mdat`, malformed and oversized files | Numbers and fixtures recorded |
| 1 | **Extraction and the re-vendored stack.** `git filter-repo` of `fire-decode`, `psd-sdk-sys` and `heif-sys`: history kept, vendored HEIF binaries dropped (FD16). The re-vendored stack committed once (newest, one baseline, `/MT`, macOS 11). Committed bindings for both bridges, target guards, and `links = "fire_psd"`. Licence files, LGPL texts, relink section, and a release carrying the source tarballs (FD14). Repo made public. fire moves to `crt-static` (FD20) and to the tag | fire's tests pass. TTFP within 2% per format, HEIC / AVIF included. fire-shellext thumbnails a HEIC. An AVIF decodes on both targets (dav1d linked). `minos` is 11.0. `fire.exe`, `fire-cli.exe` and `fire_shellext.dll` import no `MSVCP140` / `VCRUNTIME140*` / `api-ms-win-crt-*` |
| 2a | **PSD merge (FD3).** Both patch sets, fire's wrapper speed work, 3D Review's borrowed bytes and fallible allocation, the Rust ICC scan, limits from Rust. 3D Review's PSD suites are ported into the crate | Both apps' PSD suites and malformed files pass. fire TTFP on PSDs |
| 2b | **HEIF wrapper v2 (FD16).** C++ with a versioned ABI: limits, thread counts (from today's `thread_budget()` until step 3), progress / cancel hooks, Rust-allocated buffer, init once, size guard, first frame of sequence-only files. Measure libheif's SDR output for PQ / HLG files | Malformed / oversized files return errors, never abort. Fixtures correct: bit depths, grid, portrait, sequence-only. Observed threads within the budget. fire TTFP on HEIC / AVIF |
| 3 | **API changes.** Named setups (`inspect()` included) with `limits`. `output`, `source`, `hdr` (only `Sdr` implemented), `fit`, `frames`, `surfaces`, `stored_mips`, `truncated`. `sniff` / `probe` / both extension lists. The process-wide budget (libheif included), panic guard and abort check, `DecodeCtl`, cargo features. fire moves to `viewer()`, fire-shellext to its SDR first-frame setup, and fire-cli to `inspect()` | fire TTFP. Per-format bench against the previous tag. Extension tests pass under every CI feature set. `probe()` dimensions equal `decode()` dimensions over the corpus. fire-shellext still draws every face of a cubemap. fire-cli's tests pass, plus one for a rotated JPEG reporting its stored size |
| 4 | **3D Review adopts `texture()`** with `heif` in every binary (`crt-static` is already in place). It deletes `crates/psd` and its decode code in the same commit, and narrows float textures per slot (FD4). Release switches to `unwind` with the viewer-only decode hook (FD7). Worker pool. The stats' Format row from `source`. Licences, README, D22, invariant 9, the non-goal, and `review-log`'s comments that a release build aborts on panic | Startup gate. Size and imports per binary against step 0. Before/after pixel comparison over its textures, with differences explained (e.g. 16-bit PSDs now round rather than truncate); 32-bit PSDs in colour slots unchanged. A texture over the GPU limit still uploads from its first mip that fits. HEIC / AVIF textures load in the viewer, the preview pane, and Quick Look via an FBX with an embedded HEIC texture (Quick Look's sandbox reaches only embedded textures). Shell panics still caught. Asset Check reports a review3d-cli panic as the tool failing. `minos` 11.0 |
| 5 | **bite.** Licences and spec first (FD14). Rust ≥ 1.88, inherited by every crate, and `crt-static`. Cold-import benchmark by format. Route PNG / TGA / PSD / HEIC / HEIF / AVIF through the crate, and EXR after its ImageMagick comparison. Failures, `TooLarge` included, retried through ImageMagick (FD10). `probe()` replaces `header.rs`. HEIC chip. Lossless WebP with ICC. Cached reads through the crate | Cold import faster per routed format. No `magick` process on a warm re-import of crate formats. `probe()` matches `identify`. HEIC orientation matches ImageMagick. Previews visually unchanged, ICC-tagged files included. An image over 30000 px still gets a thumbnail. Cache size acceptable |
| 6a | **fire: tonemap safe for negative values**, in the shader and in fire-thumbs (FD17) | Unit test with negative and below −1 inputs |
| 6b | **PQ / HLG → `Linear` half float, and to sRGB in `Sdr`.** `source` reports the HDR kind; fire opens PQ / HLG as HDR. `Sdr` converts PQ / HLG to SDR itself (FD17) | Synthetic PQ / HLG fixtures numerically correct in both modes (known cd/m² → known value; 203 cd/m² → white in `Sdr`). SDR HEIC TTFP unchanged. `texture()` / `thumbnail()` output bit-identical to the previous tag for every file but PQ / HLG ones |
| 6c | **Gain maps** (the newest libheif re-checked first), plus fire's HDR toggle | Portrait fixture aligned. Apple and ISO maps both applied, and ISO wins when both are present. SDR default unchanged |
| 6d | **Sequences (FD18), plus HEIF progress / cancel** in fire's loading bar | The infinite-loop file terminates. Truncation flagged. Progress only rises. Cancel stops a long sequence |
| 7 | **Phase 2: `mips` feature (FD11).** fire uses it; 3D Review builds chains on its workers, and settles `rhi/mips.rs`'s place in the UV-checker bake | fire TTFP. 3D Review frame times while textures load. The UV-checker bake still builds |

3D Review goes before bite: it was the original motivation, and its gains do not depend on a benchmark.

**Step 5 as built:** bite's floor is Rust 1.99, not 1.88: the crate's own `rust-version` became
1.99 in step 1, and bite inherits it in every crate. bite's `heif` is on for Windows and macOS only
(FD9).

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
  - Run over a corpus of PNG / TGA / PSD / EXR / HEIC / AVIF / JPG, built in step 0. bite's
    `test_images` is gitignored and holds only JPG, TGA and PNG (36 files).
- **Warm re-import, as the GUI does it:** a fresh `ThumbnailCache` over a disk cache that already
  holds every thumbnail. Count `magick` processes per format, before and after `probe()`.
  - Neither benchmark measures this today. `import_bench`'s second pass reuses its cache, so it gets
    memory hits and runs no `identify`. `bite bench-import`'s warm pass reuses its cache too, and it
    decodes no thumbnails.
- **Clear the thumbnail cache before each comparison.** The file names carry no version, so old
  quality-85 files would otherwise be reused.
- **An image over 30000 px** still gets a thumbnail, through the `TooLarge` retry (FD10).
- **EXR thumbnails:** compare `magick <exr>[0] -thumbnail ...` with the crate's clamp-no-curve output.
  Add an sRGB-encode option only if they differ visibly.
- **HEIC / AVIF thumbnails** against `magick <file>[0] -thumbnail ...`:
  - orientation: does ImageMagick apply `irot` / `imir`, as FD5 expects;
  - how ImageMagick renders PQ / HLG and gain-map files in SDR, against the crate's `Sdr` path. PQ /
    HLG files are expected to differ (FD17: the crate converts them, ImageMagick very likely shows
    the raw signal); record how, not whether;
  - decode time: dav1d (crate) against libaom (bite's ImageMagick).
- **Cache size** with lossless WebP, ICC embedded, at 256 and 1024 px on a typical folder.

**Measured in step 5** (macOS; `docs/baselines.md` has the tables):
- **Cold import:** PNG -70%, TGA -92%, PSD -87%, EXR -89%, HEIC -63%, AVIF -81%; JPG, still on
  ImageMagick, unchanged. bench/bite-import, baseline and branch built side by side.
- **Warm re-import:** no `magick` process for any crate format; PSD / EXR / HEIC / AVIF went from
  121-807 ms to 1-2 ms.
- **Over 30000 px:** thumbnailed through the retry (a test).
- **EXR:** ImageMagick shows it clamped with no curve too, so no sRGB option.
- **HEIC / AVIF:** ImageMagick applies `irot` / `imir` (identical on every orientation fixture).
  It shows the gain-map fixtures' SDR base, as the crate does (identical), and PQ / HLG as the raw
  signal, as the crate does until 6b (0.4% apart, the filters). dav1d decodes a 12 MP AVIF in 78 ms
  on one thread, where ImageMagick's libaom takes 172 ms with its start-up.
- **Cache size:** lossless files were three to seven times the quality-85 ones, so the thumbnails
  stay lossy, as ImageMagick wrote them (FD12).
- **Found:** ImageMagick un-blends a white matte from any PSD whose merged image has a 4th
  channel, which crushes a flat document's mask texture to black. fire-decode keeps the stored
  colour; see **PSD** below.

**PSD**
- **Transparent documents saved by Photoshop:** does their merged image carry a white matte the
  crate should divide out? Photoshop's merged data for a document with transparency is said to be
  blended over white, and ImageMagick un-blends it. But it does so for any 4th channel: on a flat
  document whose 4th channel is a mask (`T_Noise_Burshed_01_M.psd`), and on ImageMagick's own RGBA
  PSDs, it is wrong, and the crate's stored colour is right. If Photoshop's transparent documents
  do carry a matte, the crate should remove it only when the layer count is negative (the
  spec's mark for a merged transparency channel). No such file is on the Mac; it needs Photoshop.

**HEIF**
- **libheif's own SDR rendering of PQ / HLG** (no output nclx): does it re-encode to sRGB or
  reinterpret the signal? Measured in step 2b. It decides what 3D Review textures and bite thumbnails
  of such files look like.
  - **Measured: it reinterprets** (FD17), with heif-sys's `nclx_rendering` example on
    `fixtures/heif/pq_*` and `hlg_*`. bite's ImageMagick uses libheif too, so it very likely does the
    same; section 6's comparison confirms it.
  - **Settled: `Sdr` converts them itself** (FD17), so this no longer decides how textures and
    thumbnails look.

**3D Review**
- **Binary size and startup** after the switch to unwind, and per binary with `heif` and `crt-static`.
- **JPEG path:** what the crate still saves over its new hand-driven `JpegDecoder`.
- **Float textures in the Tex viewport:** which narrowing it shows them with (FD4), settled in
  step 4. Today a 32-bit PSD shows sRGB-encoded.

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
- **bite's bundled macOS ImageMagick is built for macOS 27.0** (`minos` of
  `resources/mac/magick/bin/magick`), while bite's Info.plist says 12.0, so it won't run on anything
  older. fire's vendored HEIF libraries have the same problem (FD16).

---

## Appendix A - Code facts this plan rests on

As of fire `710b3f6`, 3D Review `746f64b` and bite `b511984` (10 November 2026).

The October version cited fire `927ee03`, 3D Review `8f033f3` and bite `9407dc5`. 3D Review's and
bite's histories have since been rewritten:
- for bite, `78be8bb` is the rewritten `9407dc5`;
- 3D Review's `8f033f3` sat on a branch that was never merged (review comments, since removed);
  `a01d4c7` is the last tree the two histories share.

The first November revision cited fire `6bd14c5` and 3D Review `6c3ceaf`. Both are ancestors of the
heads above. fire's changes since are Asset Check's: A1's three fire-decode additions and
`fire-cli`. 3D Review's are Asset Check's, plus the static CRT and step 0's `links` key.

**fire**

*`crates/fire-decode`*
- **Files:** `lib.rs` (4,081 lines), plus `psd_layers.rs` (1,750), `dds.rs`, `tiff.rs`, `raw.rs`,
  `exif.rs` and `downscale.rs`.
- **Features:** `default = ["psd", "heif"]`. The workspace pulls the crate with
  `default-features = false`, and `fire`, `fire-cli` and `fire-shellext` re-enable both.
- **Public API:**
  - `decode`, `decode_path`, `with_max_threads`, `is_supported_extension`, `level_dims`,
    `icc_is_srgb`, `SUPPORTED_EXTENSIONS`;
  - `decode_with_progress(..., &dyn Fn(f32))` - only EXR and Radiance HDR report;
  - `PsdLayers` - pure Rust: RAW / RLE / ZIP channels, 8/16/32-bit, no PSB;
  - `DecodeOptions { max_dim, honor_icc }`, whose `Default` is 16384 / true;
  - `DecodedImage` describes the file through `bit_depth` (the file's own for PNG and DDS),
    `channels`, `palette` (PNG only) and `source_format`;
  - `DecodeError { Malformed, TooLarge, NoMergedImage, Ffi, Other }`.
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
  - It draws a GIF's first frame and a DDS cubemap's whole sheet of faces.
  - Its tests compare its table with `SUPPORTED_EXTENSIONS`.

*`fire-cli`* (Asset Check's image checker)
- **Decode:** `DecodeOptions { max_dim: 0, honor_icc: false }`, so full size with no ICC conversion.
  EXIF orientation is applied, since the crate always applies it.
- **Workers:** up to 4 files at once, each under `with_max_threads(cores / workers)`.
- **What it reads:** `source_format`, `bit_depth`, `channels`, `palette`, `alpha_opaque`, `icc`
  (through `icc_is_srgb`) and `layout`, for a DDS file's surfaces.
- **Without a decode:** a PSB, or a PSD with no merged image (`NoMergedImage`), is checked on its
  own Photoshop-header parser.

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
- **C++ runtime:** `fire.exe`, `fire-cli.exe` and `fire_shellext.dll` import `MSVCP140` /
  `VCRUNTIME140(_1)`, and no installer ships the redistributable.
- **Installer test:** `installer_associations_match_the_extension_table` `include_str!`s
  `installer/fire.iss`.
- **`THIRD-PARTY-NOTICES.md`:** libheif and libde265 are listed as LGPL-3.0-only (their headers say
  "or later") and statically linked, with relinking via `crates/heif-sys/vendor`.

**3D Review**

*Texture decode*
- **`crates/render/src/texture.rs`** (593 lines):
  - PSD, by `8BPS` magic → `review_psd`.
  - JPEG, by magic or extension → a hand-driven zune `JpegDecoder` (decode → `convert_color(RGBA)` →
    `flatten`, zune's default 16384 px limit), falling back to `image` on failure.
  - Everything else → `image::ImageReader` + `to_rgba8()`, with a 30000 px cap and `image`'s default
    512 MiB allocation limit.
  - Output is RGBA8 plus `source_channels` / `source_bit_depth`. `source_channels` is what the pixels
    carry: a CMYK JPEG is 3, a PSD's undrawn extra channels don't count, and `2 | 4` means alpha.
  - `image` features: png, hdr, tga, tiff, jpeg, pnm, bmp, gif.
- **`texture/imported.rs`** (661, new): a model's own textures, embedded or found beside it.
  - Decoded on a scoped pool of `min(cores, count)`.
  - `fitted_within` is 2×2 halving, in linear light for sRGB slots.
- **`TEXTURE_EXTENSIONS`** (`app/src/loading/mod.rs`): `png jpg jpeg tga tif tiff psd bmp gif`, with
  no hdr / pnm. It also decides drag-and-drop: a dropped file not on it opens as a model. There is
  no HEIF / DDS / EXR handling anywhere.
- **Stats** (`ui/src/stats.rs`): Format (from the file's extension), Dimension, Channels, BitDepth
  and FileSize. There is no `downscaled_from`.

*`crates/psd`*
- psd_sdk `f514495` + `review.patch` (P1, extended P2), with committed `bindings.rs` and asserts.
- `fire_psd_*` symbols and static lib `fire_psd`, with `links = "fire_psd"` since step 0.
- The image-resources parser is not called.
- Borrowed bytes and `try_reserve_exact`.
- Output is RGBA8: 16-bit truncated (`>> 8`), 32-bit colour clamped and sRGB-encoded, alpha linear.
  It starts no threads.
- Limits 30000 px / 1 GiB output / 2 GiB planar / 56 channels. Built `/MT` since the static CRT
  (D25), though `build.rs`'s comment still says `/MD`.

*Texture loading and mips*
- `texture_manager.rs` (762): one thread per decode, de-duplicated by path, with generation checks.
- `rhi/mips.rs` (257): RGBA8, sRGB or raw averaging, `MAX_LEVELS` 16 = `sg::MAX_MIPMAPS`. Built in
  `Texture::rgba8_mipped_or_white` at upload, on the main thread.
  - A texture over the GPU's limit is uploaded from the first level that fits (`levels_over_limit`).
  - `render/build.rs` also compiles the file (`#[path]`) for the UV-checker bake, which hashes it.

*Panics*
- **Profiles:** release `panic = "abort"` (viewer); `release-shell` `panic = "unwind"`
  (`review_shell.dll`, `libreview_quicklook.dylib`).
- **Hook:** `review_log::install`'s panic hook logs, then chains. The viewer and both shells call it.
- **Guards:** `catch_unwind` in each shell's `guard.rs`.
- **Threads:** 27 runtime thread-spawn sites.
- **review3d-cli** builds with the same release profile and installs no panic hook.

*Shells*
- **`review_shell.dll`:** Explorer thumbnails in the COM surrogate; the preview pane in
  `prevhost.exe`.
- **Quick Look dylib:** inside two sandboxed app extensions (app-sandbox only).
- **Reach and sizes:**
  - Quick Look reaches only embedded textures (its sandbox). The Windows thumbnail gets only a
    stream, so it finds embedded textures and those named by an absolute path that exists. The
    preview pane gets the path, so it also finds loose textures beside the model.
  - Thumbnail textures are fitted to `(size × 4).clamp(512, 2048)`; previews use 2048.
  - `review-preview`'s preview-load worker and the Windows preview pane's (`preview/pane.rs`) are
    not inside `catch_unwind`.

*Rules and build*
- **Where the rules live:** `docs/ARCHITECTURE.md` and `GOTCHAS.md` are the committed rules;
  `CLAUDE.md` is local and gitignored.
- **What they say:**
  - Invariant 9's FFI sites: `import`, `psd` and `optimize`, plus the scoped GPU site,
    `shell-macos`'s Objective-C hook, `shell-windows`' COM server and `shell-quicklook`'s C ABI.
  - D22: "a clean checkout links without a prebuilt library".
  - README: "you do not need any prebuilt libraries".
  - KTX2 / DDS import is a non-goal.
  - There is no LGPL component; the notices describe "a single statically-linked executable". They
    list psd_sdk as "Vendored unmodified", though `crates/psd/vendor/NOTICE.txt` says it is patched.
- **review3d-cli:** Asset Check's model checker. It decodes no images (no `review-render`
  dependency).
- **Workspace:** edition 2024, `rust-version` 1.88, `clippy::all` and `undocumented_unsafe_blocks`
  denied.
- **Checks:** no Rust CI; `scripts/check.ps1` / `check.sh` run by hand; startup gate `scripts/gate.ps1`
  / `gate.sh`.
- **Targets:** macOS arm64 only, `LSMinimumSystemVersion` 11.0; the Quick Look extensions are built
  for macOS 11.0.
- **C++ runtime:** static since D25 (4 November 2026). `.cargo/config.toml` sets `+crt-static`, and
  no binary imports `MSVCP140`, `VCRUNTIME140*` or `api-ms-win-crt-*`.
- **Test textures:** `assets/test_textures/T_Sides_D.psd` is the only tracked one.

**bite**

*Import and thumbnails*
- **`crates/bite-imagemagick/src/import.rs` (`ThumbnailCache`):**
  - A header probe (`header.rs`: PNG / BMP / WebP / JPEG / TGA) runs in parallel. TGA is recognised
    by extension only.
  - Misses are thumbnailed by `magick` in commands of 8 files (`batch_size`), up to `jobs` (cores / 2,
    max 8) at once. `MAGICK_THREAD_LIMIT` is 1 for a lone command, otherwise each command's share of
    the cores.
  - JPEGs get `-define jpeg:size=2N×2N`.
  - The command is `-thumbnail NxN> -quality 85` to `thumb_{fnv1a64(path)}_{size}.webp`, with no
    `-strip`, so profiles are kept.
  - Failed batches are retried per file. A miss's size comes from its own thumbnail command
    (`-print`); a disk hit that `header.rs` can't read gets one `magick identify`.
  - Each import builds a fresh `ThumbnailCache`, so every cache hit outside `header.rs`'s formats runs
    `identify`. The preview worker keeps one cache for the session, with 10-minute memory entries.
  - `-thumbnail` samples down to 5× the target, then resizes with Lanczos (Mitchell for alpha or
    palette images). `NxN>` fits the long side and only shrinks.
- **`crates/bite-gui/src/commands.rs` (`add_paths`):** calls `load_batch` 16 files at a time, then
  `work::decode_many` (png 0.18 with `STRIP_16`, image-webp 0.2). image-webp is a `bite-gui`
  dependency, not `bite-imagemagick`'s.
- **`render_preview`:** runs the ImageMagick chain over the cached thumbnail, then reads the output
  PNG with `decode_png`.
- **`Magick::metadata`** (every workflow run, and a file's first preview) takes the size from
  `header.rs` too, and reads depth, resolution and EXIF with `identify`.
- **Colour:** no colour management. Neither decoder reads ICC, and no node uses `-profile`.
- **Size limits:** the bundled `policy.xml` sets no resource limits, so ImageMagick thumbnails an
  image of any size.
- **`thumbnailSize`:** default 256, clamped to 64-2048.
- **Panics:** there is no `catch_unwind` anywhere. A decode panic in `decode_many`'s scope ends the
  import thread without `ImportFinished` / `ImportFailed`.
- **Inputs:** a 66-extension list, avif / heic / heif included. The Input node's format chips have AVIF
  (off) but no HEIC, so folder import skips `.heic` / `.heif`. Only PNG, JPEG, WebP and TIFF are on
  by default. The PSD chip covers `psd` and `psb`, the EXR chip `exr` and `hdr`.

*ImageMagick*
- **Windows:** a static `magick.exe`, 7.1.2-34 Q16-HDRI, with heic (libheif 1.23.6, libde265, AV1 via
  libaom), openexr and LibRaw.
- **macOS:** source-built 7.1.2-34 Q16-HDRI, with heic as a loadable module (`--no-heic` drops it),
  openexr, and no raw. It ships libaom and libx265 (GPL-2.0+), and its binaries are built for
  macOS 27.0.
- **Licence docs:** `THIRD_PARTY_LICENSES` documents LGPL only for the macOS dylibs. Its claim that
  `--no-heic` leaves no copyleft library is already untrue: libltdl (LGPL-2.1+) stays. `docs/spec.md`
  says "no LGPL deps" and lists Bun as avoided for statically linking LGPL code.

*Workspace and build*
- **Workspace:** edition 2021, `rust-version` 1.87 (inherited only by `xtask`), release `panic`
  unwind. No lints, no `rust-toolchain`, no CI.
- **The CLI:** `bite-cli` (`bite.exe`) depends on `bite-imagemagick`, so it links whatever that links.
- **Test images:** `test_images` is gitignored. The local copy holds 36 inputs: 23 JPG, 8 TGA and
  5 PNG.
- **C++ runtime:** `bite-gui.exe` imports only `VCRUNTIME140`.
- **Targets:** macOS arm64 only, minimum 12.0 (Info.plist); the Windows installer is per-user, x64.
- **Benchmarks:** `examples/import_bench.rs` (two passes over one cache, in the GUI cache directory)
  and `bite bench-import` (a fresh cache, process counts, no thumbnail decode). Neither measures the
  GUI's warm re-import.

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
