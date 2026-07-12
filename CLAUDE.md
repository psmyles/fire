# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**Fire** (*Fast Image REview*) is a Windows-only native Win32 image viewer optimized for
*time-to-first-pixel* when double-clicking a file in Explorer. It is a single self-contained
`fire.exe`: image decoded off-thread, presented on the GPU through a Direct3D 11 device + DXGI
flip-model swapchain created lazily when the window opens, with custom GDI-painted DPI/dark-mode
chrome. There is no resident process — the thing Explorer launches *is* the whole app.

[architecture.md](architecture.md) is the authoritative design doc (process model, GPU pipeline,
color management, IPC). Read it before any non-trivial change — the sections below are the
operational summary, not a replacement.

## Build, run, test

```sh
cargo build --workspace                  # whole workspace (debug)
cargo build -p fire --release            # the single fire.exe
cargo run -p fire -- C:\path\img.png     # open an image
cargo run -p fire                        # open empty (drag/drop or pipe-forward later)
cargo test -p fire                       # render/view + window unit tests
cargo test -p fire-decode                # decode core tests (incl. tests/heif.rs end-to-end)
cargo test -p fire-decode avif_solid     # run a single test by name substring
pwsh scripts/build-installer.ps1         # build dist/Fire-<version>-Setup.exe (Inno Setup)
```

`product.json` (repo root) is the single source of product metadata (name/version/publisher/…):
`fire`'s `build.rs` reads it into the exe's version resource + `FIRE_*` compile-time env vars (the
app reads `crate::product::*`), and the installer build script reads the same file. Bump the version
there and it flows into the app and the installer; nothing else hardcodes it.

Target is `x86_64-pc-windows-msvc` (pinned in `rust-toolchain.toml`); the code is Windows-only and
won't compile elsewhere. `cargo build` on a non-Windows host short-circuits in the build scripts.

### Toolchain prerequisites that gate the build

These are not optional — the build scripts `panic!` if they're missing:

- **MSVC C/C++ build tools (VS 2022) + Windows SDK** — for `cc` C/C++ compilation, Win32 linkage,
  and `fxc.exe` (offline HLSL compiler).
- **LLVM / libclang on `PATH`** (or `LIBCLANG_PATH` set) — `bindgen` in `psd-sdk-sys` and `heif-sys`.
- **Vendored native artifacts** (not in the Rust crate graph; fetch per each crate's `vendor/VENDOR.txt`):
  - `crates/psd-sdk-sys/vendor/Psd/` — the `psd_sdk` C++ source (MolecularMatters).
  - `crates/heif-sys/vendor/lib/` + `vendor/include/` — prebuilt static `heif.lib` / `libde265.lib` /
    `dav1d.lib` (vcpkg triplet `x64-windows-static-md`).

## Workspace architecture

Five crates (`crates/`). The dependency flow is `fire` → `{fire-decode, fire-ipc}`, and
`fire-decode` → `{psd-sdk-sys, heif-sys}`.

- **`fire`** — the viewer exe. Win32 shell, D3D11 render, decode worker pool, optional named pipe.
- **`fire-decode`** — uniform decode core. Single `decode`/`decode_path` entry point; routes by
  magic bytes (and, for camera raw, file extension) to zune (hot path) / `image` / `exr` / libheif /
  psd_sdk / `raw`, normalizes everything to interleaved RGBA in one of four `PixelFormat`s, extracts
  ICC, applies lcms2 transforms, and CPU-downscales oversized images. Camera raw (CR2/CR3/NEF/ARW/…)
  is handled by `raw.rs`, which extracts the largest embedded JPEG **preview** (TIFF-IFD walk / RAF
  header / JPEG-marker scan) and decodes it via zune — full sensor development is out of scope.
  **Decode speed is the project's primary metric.**
- **`fire-ipc`** — dependency-light named-pipe wire format (length-prefixed `OpenRequest`) shared by
  the forward path and the server. Kept lean so the SingleInstance forward launch stays cheap.
- **`psd-sdk-sys`** / **`heif-sys`** — `-sys` FFI crates (bindgen + `cc`) wrapping the vendored C/C++.

### `fire` crate module map (`crates/fire/src/`)

- `main.rs` — entry: sets Per-Monitor-V2 DPI awareness *before any window*, reads config, then either
  opens a window (NewWindow) or acquires the single-instance mutex / forwards-and-exits (SingleInstance).
- `win.rs` — the Win32 shell and the central `App` state. **Frame/child-view split:** a top-level
  *frame* window owns the message loop + GDI chrome; a *child "view"* window hosts the swapchain.
  `WS_CLIPCHILDREN` decouples chrome repaints from image repaints and makes the view client rect
  exactly the image region.
- `render/gpu.rs` — `GpuSurface`: the D3D11 device/swapchain/texture/shader. **This is the only place
  that uses the typed `windows` crate** (typed COM); everything else uses `windows-sys`.
- `render/view.rs` — pure pan/zoom/fit math + `Channel` (no Win32, unit-tested).
- `chrome.rs` — GDI-painted toolbar + status bar (no Win32 common controls — they lack dark-mode
  support). Buttons hit-test to the same `Action`s the keybinds drive (one state path).
- `decode_pool.rs` — off-thread worker pool (no async runtime).
- `folder.rs` — sibling-image cursor behind ←/→ navigation + the status-bar count; pure scan/
  sort/cursor logic (no Win32, unit-tested), scanned off-thread and posted back to the frame.
- `watcher.rs` — hot-reload: a per-window thread watches the open image's directory (`notify`) and
  posts `WM_APP_FILE_CHANGED` when the file's contents change, so the UI re-decodes it. Same
  off-thread/`PostMessage`/generation-tagged discipline as the decode pool and folder scan.
- `config.rs` — the whole persisted settings surface, as TOML in `%APPDATA%\fire\config.toml`;
  missing/invalid → defaults, always `sanitize()`d. Round-trips (`Serialize` + `save()`), because
  the settings dialog writes it back.
- `keybinds.rs` — the key→`KeyAction` table (pure, unit-tested). *The* source of what a key does:
  `App::handle_key` looks up a `KeyChord`, and the toolbar's tooltips take their "(F)" suffixes from
  the same table, so a rebind relabels its button. Only non-default bindings are persisted.
- `settings/` — the modal, tabbed, hand-painted settings dialog (`mod.rs` = window + widgets;
  `model.rs` = pure field accessors + open-with tree edits). **It never holds an `&mut App`**: its
  nested message pump re-enters the frame's wndproc, so it edits a cloned `Config` and posts it back
  via `WM_APP_SETTINGS_APPLY` (see `App::apply_settings` for what applies live vs. next-image vs.
  next-launch).
- `forward.rs` / `ipc_server.rs` / `foreground.rs` — SingleInstance pipe client / server / foreground raise.
- `build.rs` — precompiles `render/shader.hlsl` to DXBC via `fxc` (embedded with `include_bytes!`) and
  embeds the `.ico` + product metadata via `winresource`.

## Cross-cutting invariants (the things that span files)

- **GPU = upload once, redraw is one draw.** The decoded image becomes a D3D11 texture with a
  hardware mip chain *once* on adopt. Pan/zoom/exposure/channel/tonemap (and the flipbook cell
  selection) are a **112-byte** constant buffer (`Params` in `render/gpu.rs` ↔ `cbuffer` in
  `render/shader.hlsl`, kept in lockstep by hand and guarded by a `size_of` assert); each frame is
  one fullscreen-triangle draw. Never reintroduce per-pixel CPU work or per-frame texture
  re-uploads. Rendering is event-driven (`InvalidateRect` → `WM_PAINT` → one vsync-paced
  `Present`); an idle window must cost ~0. *One deliberate exception:* an animated GIF re-uploads
  the texture once **per animation frame**, paced by a Win32 timer at the GIF's own frame rate on
  the UI thread (`GpuSurface::advance_frame` / `App::tick_animation`) — never per render frame; a
  still image is still upload-once. **Flipbook mode** (sprite-sheet playback, `crate::flipbook`) is
  *not* an exception: the whole sheet stays one texture and playback only changes constant-buffer
  values (cell offsets + blend, `App::tick_flipbook` on `FLIPBOOK_TIMER_ID`), never re-uploading.
- **Worker/server threads never touch the window or renderer.** The decode pool and pipe server hand
  results to the UI thread *only* via `PostMessage(frame, WM_APP_*)` with a boxed payload in LPARAM;
  the wndproc reclaims the box. Keep this discipline for any new background work.
- **Stale-drop by generation.** Each decode job carries the window's monotonic `generation`; a result
  is uploaded only if it's still current, so a slow decode can't clobber a newer open.
- **`panic = "unwind"` must stay** (see the comment in root `Cargo.toml`). Every C/C++ FFI call
  (psd_sdk, lcms2, libheif) is wrapped in `catch_unwind` so a malformed file can't crash the viewer;
  `panic = "abort"` would silently defeat that. Treat every FFI boundary as a panic/validation boundary.
- **Shader is precompiled at build time** — there is no runtime `D3DCompile`/`d3dcompiler`. Edit
  `render/shader.hlsl` and a broken shader becomes a build error, not a launch panic. `PixelFormat` ↔
  DXGI format mapping and the per-pixel color order are documented in architecture.md §5.1.
- **Foreground activation (SingleInstance only, architecture.md §4.1).** A forwarded open must call
  `AllowSetForegroundWindow` on the *forwarding* process and `SetForegroundWindow` promptly on the
  running instance, or the window swaps the image but stays behind other windows. Easy to break, very
  visible when broken.
- **ICC vs. zune tension.** Honoring an embedded profile can force a format off the zune hot path onto
  the `image` decoder that exposes ICC bytes. Verify which formats this affects before assuming the
  fast path applies.

## Status / scope

Under active construction; see `TODO.md` and architecture.md §14 for the v1-vs-deferred split. The
core viewer (window, threaded decode, GPU viewport, pan/zoom/fit, channel isolation, HDR
exposure/tonemap, DPI/dark chrome, folder ←/→ navigation) is in place, as is the Inno Setup
installer with per-format Explorer associations (`installer/`, `scripts/build-installer.ps1`), and
the settings dialog (General / Flipbook / Keybinds / Context menu).
In progress: pixel inspector, clipboard.
