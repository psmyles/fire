# Code quality audit — July 2026

Whole-workspace review of `fire` v0.1.4 at commit `721015e` (clean tree, 2026-07-13).
File references are `path:line`, as of the original review.

> **Status: acted on.** The findings below have since been fixed in the working tree — see

> three additional bugs that surfaced while fixing them (including an uninitialized-memory
> read in the PSD C++ boundary). Line numbers in §3–§6 refer to the code *as found*, so they
> no longer all resolve; the findings are kept verbatim as the record of what was wrong.

## 1. Scope & method

- All five crates reviewed: `fire`, `fire-decode`, `fire-ipc`, `psd-sdk-sys`, `heif-sys`
  (first-party code; vendored `psd_sdk`/libheif sources treated as external).
- Three parallel exploration passes (shell/UI/render, decode + FFI, support modules +
  workspace hygiene), followed by hand verification of every reported finding at its cited
  lines, a targeted logic-bug pass over `flipbook.rs`, the `win.rs` playback/generation
  paths, and `raw.rs`, and a mechanical pass:
  - `cargo clippy --workspace --all-targets` — freshly recompiled (workspace crates cleaned
    first so cached results couldn't hide warnings): **0 warnings, 0 errors**.
  - `cargo test --workspace` — **112/112 pass** (61 fire, 32 fire-decode, 8 fire-ipc,
    2 heif-sys, 9 integration).
  - `cargo fmt --all -- --check` — **fails**: 112 hunks across 19 files (see §4.3).
  - `cargo tree --duplicates` — 21 transitively-duplicated crates, all normal ecosystem
    splits (bitflags 1/2, two `png`s via resvg-vs-image, …); nothing actionable.
  - `cargo audit` — not installed; not run.

Known-outstanding items were excluded up front: everything in `TODO.md` (vsync/fps limit,
tracy, custom background color, config hot-reload) and the deliberate non-goals of
architecture.md §15 (GPU device-loss, unsigned installer, RAM bounded by `max_dim`).

## 2. Overall assessment

This codebase is in unusually good shape. Zero clippy warnings on a fresh build, 112 green
tests, no TODO/FIXME debt in first-party code, `// SAFETY:` comments on nearly every unsafe
block, and — most tellingly — **no violations found of any of CLAUDE.md's cross-cutting
invariants** (upload-once GPU discipline, terminating frame counts, PostMessage-only thread
boundaries, no message pumping under paint, generation stale-drop, no undocumented APIs were
all spot-checked and hold).

The real findings cluster in four places: **the decoder's allocation guards are inconsistent
across backends** (the one class of issue that can still crash the viewer on a crafted file,
§3), **a knot of genuine playback/state bugs in the `win.rs` glue** around the (itself clean)
flipbook module (§6), **documentation has drifted** from the single-window/128-byte-cbuffer
reality the project relies on as its design record (§4.2), and **process hygiene** (CI, lint
config, formatting policy) hasn't caught up with the discipline of the code itself (§4.3).

## 3. High — robustness against crafted files

The project's own invariant is "a malformed file can't crash the viewer", enforced by
`catch_unwind` at every FFI boundary plus the worker-pool backstop (`decode_pool.rs:159`).
That machinery is verified present and correct — **but it only catches panics.** A failed
huge allocation calls `handle_alloc_error`, which **aborts the process**; `catch_unwind`
never sees it. So any backend that allocates from unvalidated header dimensions can still
kill the viewer, and the guards are currently inconsistent:

| Backend | Guard | Status |
|---|---|---|
| zune (JPEG/BMP/QOI/WebP/…) | `MAX_DECODE_DIM` via `DecoderOptions` (`lib.rs:566-568`) | guarded |
| PNG | explicit header check (`lib.rs:500`) | guarded |
| `image` fallback (TIFF/TGA/ICO) | `ImageReader` default limits (`lib.rs:712`) | guarded (512 MiB) |
| **EXR** | none | **unguarded** |
| **GIF** | none | **unguarded** |
| **HDR** | none | **unguarded** |

### 3.1 EXR: unguarded allocation from header dimensions

`crates/fire-decode/src/lib.rs:354` — the size closure allocates
`vec![[0.0f32; 4]; size.width() * size.height()]` (16 bytes/pixel) straight from the parsed
header, with no `MAX_DECODE_DIM` check and no reliance on any `image`-crate limit (the `exr`
crate is used directly). A small crafted file claiming absurd dimensions triggers a multi-GB
allocation attempt → allocation failure → **process abort**, before the downscale pass ever
runs. `lib.rs:368` (`Vec::with_capacity(buf.pixels.len() * 16)`) doubles the peak.
The EXR path is also the only decode backend with **no end-to-end test** (no `.exr` fixture
anywhere), so this is both the least-guarded and least-tested path.

**Remedy:** check `size` against `MAX_DECODE_DIM` inside the size closure (return an empty
buffer / error out), mirroring `decode_png`; add an `.exr` fixture test.

### 3.2 GIF: unguarded dimensions × unbounded frame count

`crates/fire-decode/src/lib.rs:654-659` — `GifDecoder::new` is constructed directly (no
`ImageReader`, so no default memory limits) and `collect_frames()` decodes **every frame to
a full RGBA canvas up front**. Memory is `frames × W × H × 4` with neither factor bounded:
a crafted GIF with large logical-screen dimensions and/or thousands of frames exhausts
memory before any guard applies. This is the widest exposure of the three because the frame
count multiplies the per-frame cost.

**Remedy:** check dimensions against `MAX_DECODE_DIM` after constructing the decoder, and
cap total animation frames/bytes (e.g. stop collecting past a budget and keep the first N
frames — the viewer already treats animation as best-effort).

### 3.3 HDR: no dimension guard

`crates/fire-decode/src/lib.rs:457-463` — `HdrDecoder::with_strictness` is constructed
directly (deliberately, to bypass `ImageReader`'s strictness — the doc comment at
`lib.rs:443-455` explains the routing well), but unlike `decode_png` no substitute
`MAX_DECODE_DIM` check was added. `into_rgba32f()` allocates 16 bytes/pixel from header
dimensions.

**Remedy:** same two-line header check as `decode_png`.

## 4. Medium

### 4.1 Supported-extension lists have drifted

`crates/fire/src/folder.rs:21-29` (`IMAGE_EXTS`, drives ←/→ folder navigation) and
`crates/fire/src/win.rs:1879-1884` (`SUPPORTED_EXTS`, drives the Open dialog filter) are
hand-maintained copies, and they already disagree: folder navigation accepts
`qoi ppm pgm pbm pnm ff jxl crw srf x3f fff rwl gpr` which the Open-dialog filter omits.
The installer's association list (`installer/fire.iss`) is a third copy. Each list's comment
claims it is "kept in sync" with the others — which is exactly the claim that has silently
failed. User-visible symptom: a `.qoi` file is reachable by arrow keys but invisible in the
Open dialog's "Image files" filter.

**Remedy:** one `pub` extension table (natural home: `fire-decode`, next to the routing it
mirrors) consumed by `folder.rs` and `win.rs`; a unit test asserting the two remaining copies
(installer) match would catch future drift.

### 4.2 Documentation drift against load-bearing invariants

CLAUDE.md presents module docs as the design record ("kept in lockstep by hand"), and several
now state things that are false, in exactly the places a future change would consult:

- `render/gpu.rs:1-5` — module doc says the image presents "on the child 'view' window"
  (deleted in the single-window collapse) and costs "~an 80-byte upload".
- `render/gpu.rs:67` — "112 bytes = 7 float4 registers", eight lines above the
  `assert!(size_of::<Params>() == 128)` at `gpu.rs:123` that contradicts it.
- `render/gpu.rs:1166` — "(`Params`, 80 bytes, …)".
- `render/view.rs:18-20` — `Viewport` doc still describes "the child view window's client
  area" and chrome "in separate windows".
- `chrome.rs:13-14` — claims "GDI text helpers at the bottom" exist for a settings dialog
  that is "still a hand-painted Win32 window"; the settings window is ImGui
  (`ui/settings/`), no GDI helpers remain in the file, and the doc-link target
  `crate::settings` no longer exists.
- CLAUDE.md ("Build, run, test") says non-Windows builds "short-circuit in the build
  scripts". True only for `crates/fire/build.rs:27`; `psd-sdk-sys/build.rs` and
  `heif-sys/build.rs` have no OS gate and instead fail on missing vendor artifacts / MSVC
  flags.

All cosmetic individually; collectively they erode the one mechanism the project uses to
keep the hand-synced cbuffer and architecture narrative trustworthy. Cheap to fix in one
sweep.

### 4.3 No CI, no lint/format policy

There is no `.github/` directory, no `clippy.toml`/`rustfmt.toml`/`deny.toml`, and no
`[workspace.lints]` in the root `Cargo.toml`. Consequences observed directly:

- The code *is* clippy-clean today, but nothing keeps it that way.
- `cargo fmt --check` fails with 112 hunks across 19 files — the codebase uses a deliberate
  wider style (e.g. single-line builder chains) that default rustfmt rewrites. Any
  contributor (or tool) that runs `cargo fmt` produces a 19-file noise diff. Either commit a
  `rustfmt.toml` encoding the house style, or run `cargo fmt` once and enforce it; the
  current in-between is the worst of both.
- 112 tests exist but only run when someone remembers.

**Remedy:** a minimal Windows CI job (`cargo clippy --workspace --all-targets -D warnings` +
`cargo test --workspace`; the vendored `-sys` artifacts likely require caching or a
`fire`+`fire-decode`-only subset), plus a formatting decision, plus
`[workspace.lints]` to pin the intent.

### 4.4 PSD is the only C++ FFI path — and has zero automated tests

`psd-sdk-sys` and the PSD route in `fire-decode` have no unit test, no fixture, and no
integration test; the only exercise is `examples/psd_roundtrip.rs`, which needs a real file
argument. Contrast HEIF (5 end-to-end tests with committed fixtures) and HDR (4, including a
regression test). The riskiest boundary (C++, hand-written `wrapper.cpp` channel sampling
across gray/RGB/8/16/float) is the only one CI can't see at all. A tiny committed `.psd`
fixture (as done for AVIF/HEIC) would cover the whole chain. Same gap, smaller: the 16-bit
HEIF scaling path (`heif-sys/wrapper.c:91-105`) has no 10/12-bit fixture — acknowledged in
`tests/heif.rs:6-9` as deferred.

### 4.5 `image` crate default features pull in an AV1 encoder

`fire-decode` depends on `image = "0.25"` with default features (root `Cargo.toml:32`),
which includes AVIF support via `ravif` → `rav1e` — a full AV1 **encoder**, one of the
heaviest crates in the ecosystem to compile. fire never encodes, and AVIF decoding is routed
to libheif/dav1d by magic-byte sniff, so the feature is dead weight: it costs every clean
build (rav1e + av-scenechange + friends) and some binary size (thin-LTO strips most, not
all). **Remedy:** `default-features = false` plus the features actually used by the fallback
paths (`png`, `gif`, `hdr`, `tiff`, `tga`, `ico`, `bmp`). Verified used elsewhere:
`jxl-oxide` comes from zune-image and *is* live (JXL is a supported extension).

## 5. Low

- **Dead `DecodeError::UnknownFormat` variant** — `fire-decode/src/lib.rs:147` defines it
  and `:159` prints it, but nothing constructs it (`sniff` always picks a backend). Remove
  or wire it to the sniff fallback.
- **heif wrapper hardening** — `heif-sys/wrapper.c:78-79` computes `w*bpp*h` in `size_t`
  with no `checked_mul` equivalent (the Rust psd side does this properly,
  `psd-sdk-sys/src/lib.rs:98-102`), and `wrapper.c:51` clamps `bits` low but not high, so a
  hypothetical `bits > 16` makes `shift` negative (UB). Both unreachable with real libheif
  outputs (its security limits cap dimensions; it only reports 8/10/12 bits) — cheap
  belt-and-braces fixes.
- **Single-instance pipe niceties** — `ipc_server.rs:46-55`: `CreateNamedPipeW` without
  `FILE_FLAG_FIRST_PIPE_INSTANCE` (a local process could squat `\\.\pipe\fire` before the
  viewer starts and impersonate the server), null security attributes, and a blocking
  `read_exact` with no timeout — one hung client stalls the (single) server thread, so later
  forwards fall back to opening new windows. Also `PIPE_NAME` is machine-global while
  `MUTEX_NAME` is deliberately `Local\` per-session (`fire-ipc/src/lib.rs:23,27`), so under
  fast-user-switching two sessions' viewers share one pipe namespace. All local-only,
  low-impact; the wire format itself is exemplary (length cap checked *before* allocation,
  with a regression test — `fire-ipc/src/lib.rs:189-192,243`).
- **Duplication worth folding on next touch** (none of it is wrong today):
  - `on_accent` duplicated verbatim — `ui/mod.rs:617-624` vs `ui/theme.rs:242-248` (one
    inlines the `luminance` the other calls). Same crate; keep the `theme.rs` copy.
  - Clipboard open/alloc/lock/set/free skeleton duplicated — `win.rs:1997-2020` vs
    `win.rs:2026-2060`.
  - Two near-identical `OPENFILENAMEW` setups — `win.rs:1911-1930` vs `:1938-1960`.
  - LPARAM→coords extraction inlined three times — `win.rs:1077`, `:1688`, `:1710`.
  - `anim_frames` clone block — `gpu.rs:464-469` vs `:506-511`.
  - Toolbar divider loop — `ui/mod.rs:459-470` vs `:487-498`.
- **`arboard` is a declared-but-unused workspace dependency** (root `Cargo.toml:45`; no
  crate consumes it — clipboard is raw Win32). Remove, or leave with a comment if it's
  staged for the clipboard TODO.
- **`dear-imgui-rs`/`-sys` pinned inline** (`fire/Cargo.toml:66-70`) rather than through
  `[workspace.dependencies]`, contradicting the root manifest's "every member inherits
  these" comment. Single-consumer, so harmless — but move the pin or amend the comment.
- **Release builds log to nowhere** — 21 `eprintln!` sites across 6 files, and the release
  exe is `windows_subsystem = "windows"` (`main.rs:13`), so stderr is discarded. Every
  logged failure (clipboard, pipe, GPU resize) is invisible in the field. Fine until the
  first user bug report; consider `OutputDebugStringW` or an opt-in log file.
- **Untested extractable logic** — the toolbar overflow-drop algorithm
  (`ui/mod.rs:411-447` + `strip_width` `:503-516`) is genuine priority/tie-break logic with
  no tests; it takes only widths and priorities, so it could be extracted pure and tested
  like `view.rs`/`folder.rs` are. Also `render/view.rs:184,196` (`screen_to_image` /
  `image_to_screen`) are `#[allow(dead_code)]` awaiting the pixel inspector — fine, just
  noting they're tested-but-unwired.

## 6. Targeted logic-bug pass

A dedicated correctness pass over `flipbook.rs`, the `win.rs` playback/transport/generation
paths, and `raw.rs` found seven real bugs (each mechanism re-verified by hand against the
code). Notably, `flipbook.rs` itself came out clean — its math is well-guarded and
well-tested; all the playback bugs live in the `win.rs` glue around it.

### 6.1 `SetCount` transport edit never reaches the GPU — playback clamps/blends wrong (certain)

`win.rs:699` + `win.rs:718-721`: `TransportEdit::SetCount` takes the "Count/scrub: just move
the position" branch, which calls only `set_flipbook_pos` — and `GpuSurface::set_flipbook_pos`
(`gpu.rs:433-438`) writes *only* `frame_pos`. Nothing re-pushes the full `FlipbookParams`, so
the shader keeps resolving cells against the **old** `frame_count`. Raising the count (32→64
on an 8×8 sheet) makes playback freeze on frame 31 for half of every loop; lowering it with
blend on crossfades the seam into the trimmed-off empty cell — the exact partial-last-row
case `frame_count` exists to handle. It self-corrects only when some other edit
(play/fps/blend/grid) happens to push full params.

Same branch, second defect: `SetCount` skips `sync_flipbook_timer` (`win.rs:710-717` only
resyncs for TogglePlay/SetFps/ToggleBlend). Typing Count = 1 while playing leaves the timer
firing every 16 ms doing nothing (a standing violation of the ~0-idle invariant); the
inverse sequence (pause → Count 1 → play → Count 8) leaves `playing = true` with **no**
timer, so playback only advances while the mouse moves.

**Remedy:** route `SetCount` through the full-params branch (`set_flipbook` +
`sync_flipbook_timer`).

### 6.2 Blend-off playback below ~4 fps runs at `0.25·fps²`, not `fps` (certain)

`sync_flipbook_timer` arms the blend-off timer at `1000/fps` ms (`win.rs:586`), but
`advance_flipbook` caps each tick's dt at `MAX_FLIPBOOK_STEP = 0.25 s` (`win.rs:146,633`)
and then resets `flipbook_last_tick` to now, discarding the excess. Whenever the interval
exceeds the cap (fps < 4), each tick advances only `0.25·fps` frames: at fps = 1 a frame
takes 4 s; at the supported minimum fps = 0.1, 40 s instead of 10. Mouse-motion paints mask
it (extra small-dt advances), which is probably why it survived the recent playback fix.

**Remedy:** either clamp the timer interval to ≤ `MAX_FLIPBOOK_STEP`, or don't cap dt below
the armed interval (cap at `max(0.25, interval)`).

### 6.3 Decode-failure reason is stored but never shown (certain)

`fail_load` (`win.rs:439-441`) sets `self.meta` to the failure message and then
`clear_image()`s the surface — but `snapshot`'s first branch (`win.rs:1016-1017`) is
`!has_image && !loading → "No image"`, which now always wins. The user opening a corrupt
file sees status "No image" and a title-bar "(failed)" with the actual `DecodeError` text
reaching only stderr — which release builds discard (§5). The error-reporting path exists
end-to-end and is dead at the last hop.

**Remedy:** branch on `!self.meta.is_empty()` before the "No image" arm (or clear `meta` in
the paths that should genuinely show "No image").

### 6.4 Hot-reload during the folder scan permanently kills ←/→ navigation (likely)

The folder scan is tagged with the *open*'s generation (`scan_folder`, `win.rs:336`), but a
watcher-triggered `reload` (`win.rs:312-320`) re-enters `begin_decode`, which bumps the
generation (`win.rs:289`) without rescanning. A scan that lands after that is dropped by
`folder_scanned`'s staleness check (`win.rs:361`), and since `open` cleared `self.folder`,
the cursor stays `None` — no ←/→, no "n / m" count — until the next fresh open. The window
is real in the canonical hot-reload scenario (opening a file an exporter is still writing,
folder on slow/network storage). The generation guard conflates "different image" with
"same image, re-decoded".

**Remedy:** give reloads a way to not invalidate the scan — e.g. track the scan's own
generation separately, or re-kick `scan_folder` on reload when `folder.is_none()`.

### 6.5 During a decode, the transport operates on the incoming image while the outgoing one is displayed (likely)

`begin_decode` sets `current_path` at *request* time (`win.rs:290`), but the surface shows
the previous image until adopt. Everything flipbook-related keys off `current_path`
(`flipbook_state`, `apply_transport_edit` `win.rs:671`, `advance_flipbook` `win.rs:615`), so
for the decode's duration (seconds for a big PSD/EXR) the transport band reflects and edits
the **incoming** image's state, and `advance_flipbook` pushes the incoming image's
`frame_pos` into the **displayed** image's surface params — visibly scrubbing the old image
if it was in flipbook mode. Self-heals at adopt.

**Remedy:** key the UI off an "adopted path" that flips in `decode_done`, or freeze
transport interaction while `loading`.

### 6.6 EXIF orientation sentinel conflates "unset" with an explicit Orientation = 1 (likely)

`raw.rs:345-351`: the walk keeps IFD0's Orientation by guarding `if orientation == 1` — but
1 is also a legitimate stored value, so when IFD0 explicitly says "upright", a later-walked
IFD (thumbnail IFD1 / SubIFD) with a different Orientation overrides it and the preview gets
rotated 90°/180° despite being correct. Requires a file whose non-primary IFD disagrees with
IFD0. (The eight `apply_orientation` transforms themselves were verified correct.)

**Remedy:** use `Option<u16>` (or a `seen` flag) for first-wins semantics.

### 6.7 JPEG SOF probe derails on legal `FF FF` fill bytes (likely)

`jpeg_dimensions` (`raw.rs:226-248`): JPEG permits any number of `0xFF` fill bytes before a
marker (`FF FF … FF C0`). The scanner takes `marker = b[i+1]` without handling
`marker == 0xFF`, reads a garbage length starting at the real marker byte, and skips ~49 KB —
usually past the real SOF, so the probe fails and `pick_largest` discards a valid (often the
largest) preview. The `i += 1` tolerance handles non-FF junk but not this case.

**Remedy:** `if marker == 0xFF { i += 1; continue; }`.

### Verified clean

The same pass explicitly cleared: all of `flipbook.rs`'s cell/seam/detection math (indexing,
zero-division guards, 1×N strips, negative `frame_pos` — the one theoretical `rem_euclid`
edge is neutralized downstream), the recent playback-starvation fix in commit `721015e`
(complete, and correctly non-self-sustaining), RAF's big-endian header reads,
`scan_jpeg_markers`' cap, and every `rd_*` bounds helper in `raw.rs`.

## 7. Strengths (verified, not just claimed)

- **Every CLAUDE.md cross-cutting invariant held** under spot-check: upload-once (GIF/
  flipbook exceptions exactly as documented), terminating `request_frames` with the caret
  blink as the only input-free timer, PostMessage-only worker boundaries with box reclaim on
  failed posts, no message pumping under paint, `SV_Position` origin subtraction present,
  dual RTV discipline present, generation stale-drop present, no undocumented APIs.
- **`catch_unwind` coverage is complete**: all three FFI entry points (psd_sdk, libheif,
  lcms2) wrapped at the call site *and* double-wrapped by the worker pool. (Bounding caveat:
  this catches panics, not native segfaults or allocation aborts — see §3.)
- **`raw.rs` is a model parser**: every read through `Option`-returning helpers, explicit
  DoS budgets (IFD count 64, entries 4096, SubIFDs 32, marker scan 256), cycle guard on the
  IFD walk.
- **`fire-ipc` validates like it's hostile input** (it is): length cap before allocation,
  version byte, discriminant check, UTF-8 check, all regression-tested.
- **`config.rs`**: best-effort load → defaults, single `sanitize()` chokepoint that also
  scrubs NaN/Inf, atomic save via temp+rename, race-safe `create_new` for the default file.
- **Test culture where it counts**: 112 tests, all pure-logic modules covered, including
  regression tests pinned to specific upstream bugs (zune-hdr exponent wrap) and
  design-agreement tests (settings sliders vs `sanitize` ranges).
- **Zero clippy warnings** on a fully-recompiled workspace, with almost no `#[allow]`
  escape hatches (the few present are justified: bindgen output, MAKEINTRESOURCE).
- **Unsafe discipline**: unsafety confined to the Win32/COM/FFI boundary (zero `unsafe` in
  every pure-logic module and in all of `fire-decode`), with `// SAFETY:` comments on the
  cross-thread ownership transfers.

## 9. Resolution

Everything in §3–§6 has been fixed, plus three bugs that only surfaced once the tests existed.
Verification: `cargo clippy --workspace --all-targets -- -D warnings` clean; **136 tests pass**
(was 112); the release exe was run against a real 5×5 flipbook sheet and against crafted decode
bombs, which it now refuses with a clean error instead of aborting.

### Found while fixing (not in the original review)

1. **PSD served short reads from uninitialized heap memory.** `MemoryFile::DoRead`
   (`psd-sdk-sys/wrapper.cpp`) copied only the bytes available and returned *success*, leaving the
   tail of psd_sdk's `malloc`'d buffer uninitialized — which psd_sdk then sampled straight into the
   composite. A truncated PSD would render whatever the heap happened to hold. It now zero-fills the
   remainder, records the short read, and `fire_psd_open` rejects the document. Caught by the first
   PSD fixture ever written; it is the reason §4.4 mattered.
2. **PSD had no size guard at all.** psd_sdk allocates its planar buffers from the file header
   *inside* `fire_psd_open`, before Rust sees a dimension — so a 40-byte PSD claiming 60000×60000
   asked for ~10 GB and aborted. Guarded in C++, at the allocation site (`psd_size_is_sane`).
3. **A per-axis dimension cap does not bound a GIF.** The review prescribed a `MAX_DECODE_DIM`
   check; that would have been useless here, because GIF dimensions are `u16` and 65535×65535 sits
   under any sane axis cap while asking for 17 GiB *per frame*. The guard checks the **product**
   (`check_dims` takes bytes-per-pixel and a 4 GiB budget), which is what actually gets allocated.

### Deliberately not done

- **No workspace reformat.** §4.3 offered "commit a `rustfmt.toml` encoding the house style, or run
  `cargo fmt` once". Neither is available: the code is *hand*-formatted, and no config reproduces it
  — `use_small_heuristics = "Max"` makes it worse (201 hunks vs 112), because rustfmt then wants to
  re-join lines the author deliberately split. Reformatting would mean overwriting 19 files of
  intentional formatting inside a bug-fix changeset. Left alone; `cargo fmt --check` is therefore
  **not** in CI. This is a standing decision for the owner, best made as its own commit.
- **`clippy::undocumented_unsafe_blocks` left off.** 74 `unsafe` blocks lack a `// SAFETY:` tag.
  They are not undocumented — the cross-thread hand-offs and FFI boundaries carry real reasoning —
  but most are one-line Win32 calls where a tag would be ritual. Enabling it today means writing 74
  comments to satisfy a lint, which is how SAFETY comments become noise reviewers skip. The reason
  is recorded in `Cargo.toml` next to the commented-out lint.

### The zune hot path — also closed

Initially left open, since zune exposes no total-size option and `Image::read` parses the header and
allocates the pixels in one shot, never handing back the dimensions in between. The way through is
`DecoderTrait::read_headers()`, which every format routed to zune implements: `check_zune_dims`
parses the header a *second* time on a throwaway decoder purely to have somewhere to refuse, then
applies the same `check_dims` byte budget as every other backend.

The second parse was measured rather than assumed, because decode speed is the project's primary
metric: **13.6 µs against a 129 ms decode** on a 4928×3264 JPEG — **0.011%**. It reads marker bytes
and no pixels, so it scales with header size, not image size (7.5 µs on a 4 MP file) and does not
grow with the images it protects.

Verified end-to-end: a 197 KB progressive JPEG patched to declare 65535×65535 (~17 GiB) is now
refused by the shipped exe with `JPEG 65535x65535 needs more than the 4294967296-byte decode guard`,
where it previously would have asked the allocator for 17 GiB and aborted. `zune_read_headers_is_not_vacuous`
pins the probe against a future zune upgrade silently dropping a `read_headers` impl and turning the
guard into a no-op.

**Every decode backend is now bounded by the product, not just the axes.**

## 8. Suggested order of attack

1. §3 decode guards (EXR, GIF, HDR) — small diffs, closes the only crash class found.
2. §6.1–6.3 (SetCount push, low-fps pacing, invisible decode error) — certain, user-visible,
   each a few lines.
3. §4.1 extension-table unification — user-visible today.
4. §6.4–6.7 (scan generation, transport-during-decode, orientation sentinel, FF fill bytes) —
   real but rarer; fold into the next touch of each area.
5. §4.2 doc sweep — one sitting, protects the design record.
6. §4.4 PSD + EXR fixtures — makes the riskiest paths visible to tests.
7. §4.3 CI + format decision — locks in everything above.
8. §4.5 and §5 opportunistically, on next touch of each file.
