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
- `win.rs` — the Win32 shell and the central `App` state. **One window**: it owns the message loop,
  the swapchain (covering the whole client), and the ImGui layer. The image is drawn into a
  **sub-rect** of that swapchain (`App::image_rect`, recomputed every frame — no retained layout to
  invalidate), with the chrome drawn over the rest. The old frame/child-view split existed only
  because GDI cannot paint on a flip-model swapchain; with the chrome on the GPU it is gone, along
  with `WS_CLIPCHILDREN` and the second wndproc.
- `render/gpu.rs` — `GpuSurface`: the D3D11 device/swapchain/texture/shader.
- `render/imgui.rs` — the Dear ImGui context + the two **upstream** backends. Together with `gpu.rs`,
  the only places that use the typed `windows` crate (typed COM); everything else uses `windows-sys`.
- `render/view.rs` — pure pan/zoom/fit math + `Channel` (no Win32, unit-tested).
- `ui/` — the whole UI, rebuilt every frame in immediate mode (`mod.rs` = toolbar / status bar /
  transport band / hint chip / empty-state hint; `theme.rs` = style, colors, metrics; `settings/` =
  the settings window). **Pure UI: no Win32, no COM, no GDI.** It reads a `ViewSnapshot` and returns a
  `ui::Frame` of what the user asked for; the win shell applies it.
- `ui/settings/` — the settings window, an ImGui `BeginPopupModal` (`mod.rs` = the modal + its four
  tabs; `model.rs` = pure field accessors + open-with tree edits, unit-tested). It wears **ImGui's
  stock style, not fire's chrome theme** (`render::imgui::StockStyle`): the chrome is a toolbar, a
  settings window is a form. Two things it can't do itself are reported to the shell instead —
  "Browse…" (the file dialog pumps a modal loop) and keybind *capture* (chords are virtual keys, which
  only the wndproc sees).
- `chrome.rs` — despite the name, no longer paints anything, and is down to ~330 lines. What survives
  is the shared *model* and *theme*: `Action` + `ViewSnapshot` (the command vocabulary and the state
  the UI renders from), the light/dark `Palette` — whose highlight is the user's **system accent**
  (`GetSysColor(COLOR_HIGHLIGHT)` tracks it: documented, no registry poking) — and `apply_dark_titlebar`
  (documented DWM). **Every API it calls is documented**; the uxtheme ordinal hack went out with the
  Win32 menus.
- `decode_pool.rs` — off-thread worker pool (no async runtime).
- `folder.rs` — sibling-image cursor behind ←/→ navigation + the status-bar count; pure scan/
  sort/cursor logic (no Win32, unit-tested), scanned off-thread and posted back to the frame.
- `watcher.rs` — hot-reload: a per-window thread watches the open image's directory (`notify`) and
  posts `WM_APP_FILE_CHANGED` when the file's contents change, so the UI re-decodes it. Same
  off-thread/`PostMessage`/generation-tagged discipline as the decode pool and folder scan.
- `config.rs` — the whole persisted settings surface, as TOML in `%APPDATA%\fire\config.toml`;
  missing/invalid → defaults, always `sanitize()`d. Round-trips (`Serialize` + `save()`), because
  the settings window writes it back (`App::apply_settings` decides what applies live vs. next-image
  vs. next-launch).
- `keybinds.rs` — the key→`KeyAction` table (pure, unit-tested). *The* source of what a key does:
  `App::handle_key` looks up a `KeyChord`, and the toolbar's tooltips take their "(F)" suffixes from
  the same table, so a rebind relabels its button. Only non-default bindings are persisted.
- `forward.rs` / `ipc_server.rs` / `foreground.rs` — SingleInstance pipe client / server / foreground raise.
- `build.rs` — precompiles `render/shader.hlsl` to DXBC via `fxc` (embedded with `include_bytes!`) and
  embeds the `.ico` + product metadata via `winresource`.

## Cross-cutting invariants (the things that span files)

- **GPU = upload once, redraw is one draw.** The decoded image becomes a D3D11 texture with a
  hardware mip chain *once* on adopt. Pan/zoom/exposure/channel/tonemap (and the flipbook cell
  selection) are a **112-byte** constant buffer (`Params` in `render/gpu.rs` ↔ `cbuffer` in
  `render/shader.hlsl`, kept in lockstep by hand and guarded by a `size_of` assert); each frame is
  one fullscreen-triangle draw, scoped by `RSSetViewports` to the image's sub-rect of the window.
  Never reintroduce per-pixel CPU work or per-frame texture re-uploads. *One deliberate exception:*
  an animated GIF re-uploads the texture once **per animation frame**, paced by a Win32 timer at the
  GIF's own frame rate on the UI thread (`GpuSurface::advance_frame` / `App::tick_animation`) — never
  per render frame; a still image is still upload-once. **Flipbook mode** (sprite-sheet playback,
  `crate::flipbook`) is *not* an exception: the whole sheet stays one texture and playback only
  changes constant-buffer values (cell offsets + blend, `App::tick_flipbook` on `FLIPBOOK_TIMER_ID`),
  never re-uploading.
- **Rendering is event-driven; an idle window must cost ~0.** This is the invariant the ImGui
  migration was most at risk of breaking, because ImGui's natural mode is to redraw forever. A frame
  is drawn only when something happened. `App::request_frames(n)` asks for the *one or two* extra
  frames ImGui needs to settle a hover or a click, and the count **terminates** — at zero, `WM_PAINT`
  stops requesting itself. Never add an unconditional repaint or a free-running timer; the measured
  cost is 0.00% of a core idle and it must stay there. The one timer that repaints with no input
  behind it is the **caret blink** (`App::sync_caret_timer`), and it is armed *only* while
  `io.want_text_input` — i.e. while a settings text field is actually being edited — and killed the
  instant focus leaves.
- **Two render-target views of one backbuffer, and they are not interchangeable.** The image shader
  emits *linear* light and writes through the `*_SRGB` view; ImGui's colors are *already* sRGB and
  must write through the plain `UNORM` view. Drawing ImGui through the sRGB view encodes twice and
  visibly washes the whole UI out — it doesn't crash, it just looks wrong, so it is easy to introduce
  and hard to attribute. `GpuSurface::begin_frame` leaves the UNORM view bound for exactly this
  reason (architecture.md §5.2).
- **Worker/server threads never touch the window or renderer.** The decode pool and pipe server hand
  results to the UI thread *only* via `PostMessage(frame, WM_APP_*)` with a boxed payload in LPARAM;
  the wndproc reclaims the box. Keep this discipline for any new background work.
- **Nothing that pumps messages may be entered from `WM_PAINT`, or under a live `&mut App`.** The UI
  is built during the paint, so a click is *discovered* mid-paint — and a Win32 modal
  (`GetOpenFileNameW`, `TrackPopupMenu`, a message box) runs a nested loop that re-enters the wndproc,
  which would both recurse into an unvalidated paint and take a second `&mut App`. So it is recorded
  and posted, and runs once the paint has finished. **Exactly one thing still needs this**: the
  settings window's "Browse…" (`WM_APP_SETTINGS_BROWSE`). Any future Win32 modal obeys the same rule.
  **ImGui modals and popups do not**, and that is the point of them: the settings window and both
  menus are drawn inside the frame we were already painting, so they pump nothing and borrow nothing —
  their state just lives in `App`, and a chosen command can simply run.
- **The app calls no undocumented API.** It did — `TrackPopupMenu` menus are system-drawn, so the only
  way to dark-mode one was three `uxtheme.dll` ordinals (133/135/136) resolved by `GetProcAddress` and
  `transmute`d. The menus are ImGui popups now and that hack is gone. Keep it that way: if a Win32
  control can't be themed without an ordinal, the answer is to not use the Win32 control.
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
the settings window (General / Flipbook / Keybinds / Context menu).

**The UI is Dear ImGui, end to end.** The single-window collapse, toolbar, status bar, flipbook
transport, hint chip, tooltips, empty-state hint, settings window, and both popup menus — with the GDI
paint/hit-test/hover/focus layer, the hand-painted Win32 dialog, and the `TrackPopupMenu` menus all
deleted. No GDI painting and no undocumented APIs remain.

The settings window is deliberately **unstyled**: it runs on ImGui's stock look while the chrome runs
on fire's theme. That is a decision, not an oversight (see `ui/settings`), and the style pass is still
to come.

In progress: pixel inspector, clipboard.
