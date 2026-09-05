# Fire

**Fire** - _Fast Image REview_ - is an image viewer optimized for **time-to-first-pixel**
when double-clicking a file in Explorer or Finder. It has features to help in game development workflows that regular image viewers usually don't cover. Fire was built because of my frustrations with existing image viewers: slow, and no one covers everything I want from an image viewer. Fire is also strictly an image viewer and NOT an editor: good image editors already exist, please use them for editing, Fire can launch the image into them if you set it up for that.

Fire runs on **Windows** (x86_64) and **macOS** (Apple Silicon) from one shared codebase: the
window, event loop and input are `winit`'s, the GPU work goes through `sokol_gfx` (Direct3D 11 on
Windows, Metal on macOS), and the only platform-specific code left is a device/swapchain module per
OS plus a handful of small leaves.

## Features

- View the contents of individual R, G, B, and A channels
- View images against different backdrops (black, white, grey, checkerboard)
- Flipbook player with automatic grid detection logic and playback controls
- Support for all source image formats needed for game development
- Tonemapping and exposure controls for HDR images
- Uses the fastest possible library for decoding each image format
- Hot-reload: the displayed image re-decodes automatically when its file changes on disk
- The image is decoded off-thread and presented on the GPU through a vsync-paced swapchain
  (a DXGI flip-model swapchain on Windows, a `CAMetalLayer` on macOS)
- Perfectly smooth frame locked zoom and pan operations even on huge image files, with zoom
  snapping to common steps and a pixel-crisp 1:1 (one texel per _physical_ pixel, so it stays
  crisp on a Retina display)
- Full-screen mode (F11) that hides all chrome and gives the image the whole monitor
- Fully customizable keyboard shortcuts (Settings \ Keybinds), layout-independent and shared
  between both operating systems - `Primary+` means Ctrl on Windows and ⌘ on macOS
- Honors EXIF orientation
- DPI-aware, dark-mode-aware toolbar, status bar, flipbook controls and settings window drawn
  by **Dear ImGui** into the same backbuffer as the image
- An octagon overlay mode to visualize how image would get cropped by octagon polygon shape in VFX systems in game engines
- A customizable context menu that covers basic operations and allows for user defined behavior
- One process, as many windows as you like: a second launch hands its path to the running Fire
  (`open-in = "new-window"` gives it its own window, `"reuse-window"` swaps it into the focused one)

See [architecture.md](architecture.md) for the full design, and
[mac-port-plan.md](mac-port-plan.md) for the record of how the shared shell was chosen and built.

## Workspace

```
crates/
  fire/          the viewer app - winit window + event loop, sokol_gfx render, decode pool,
                 instance socket (the whole app; the file manager launches it directly)
  fire-decode/   uniform decode core: bytes → (pixels, format, bit depth, ICC)
  fire-ipc/      wire format for forwarding an open to the running instance, dependency-light
  psd-sdk-sys/   FFI to vendored psd_sdk C++ (cc + bindgen)
  heif-sys/      FFI to vendored libheif + libde265 + dav1d for AVIF / HEIF / HEIC (cc + bindgen)
vendor/
  sokol-rust/    pinned upstream copy of floooh/sokol-rust (the crates.io `sokol` name is taken
                 by an unrelated crate); excluded from the workspace, built as a path dependency
```

## Build & dev

```sh
cargo build --workspace
cargo run -p fire -- path/to/img.png   # open an image
cargo run -p fire                      # open with no image (drag/drop or forward later)
cargo test -p fire                     # view/config/keybinds/flipbook/folder/theme unit tests
cargo test -p fire-decode              # decode core tests (incl. tests/heif.rs end-to-end)
cargo build -p fire --release          # release build (the single fire binary)
cargo clippy --workspace --all-targets -- -D warnings   # what CI runs
```

`--no-default-features` builds the viewer without the two vendored native decoders (PSD and
HEIF/AVIF), which is how CI checks the whole shell on a runner that has no vendor tree.

**Packaging.**

```pwsh
pwsh scripts/build-installer.ps1       # Windows: dist/Fire-<version>-Setup.exe (Inno Setup)
```

```sh
scripts/build-mac.sh                   # macOS: signed + notarized dist/Fire-<version>.dmg
scripts/dev-app.sh img.png             # macOS: quick unsigned .app for development, then open it
```

`scripts/build-mac.sh` runs by hand on the dev Mac and takes both credentials (the Developer ID
identity and the `notarytool` profile) from the keychain, so they never have to exist as CI
secrets. `--no-notarize` / `--no-sign` step down from that for iteration.

**Shaders.** The viewport shader is one annotated-GLSL source,
`crates/fire/src/render/shader.glsl`. `scripts/gen-shaders.sh` turns it into the per-backend HLSL
and MSL sources plus the sokol_gfx reflection under `crates/fire/src/render/generated/`, all of
which is **checked in** - so a plain `cargo build` never needs `sokol-shdc`, and `build.rs` only
compiles the host backend's pair to bytecode (`fxc` → DXBC, `xcrun metal` → `.metallib`). Edit the
`.glsl`, run the script on either OS, commit both.

**Measurement.** `scripts/ttfp.ps1` (Windows) and `scripts/ttfp.sh` (macOS) are the A/B
time-to-first-pixel harness: they launch two builds alternately on each test image and report
median / mean / sd of the milliseconds from _kernel process creation_ to the first image-bearing
present. The binary measures itself when `FIRE_TTFP_OUT` is set; `FIRE_TIMING=1` prints a
launch-path phase breakdown.

Product metadata (name/version/publisher/…) lives in `product.json` at the repo root - `build.rs`
reads it into the exe's version resource and `FIRE_*` env vars, and both packaging scripts read the
same file, so bumping the version there flows into the app, the installer and the `.app` bundle
alike.

Settings live in a per-user directory - `%APPDATA%\fire` on Windows,
`~/Library/Application Support/fire` on macOS - as a commented `config.toml` Fire writes on first
run, plus `window.toml` for the remembered window placement. Everything in it is also editable in
the app (Settings, at the bottom of the toolbar's menu button and of the viewport's right-click
menu).

## Toolchain prerequisites

Common to both: **Rust stable** via rustup (targets pinned in `rust-toolchain.toml`).

**Windows (`x86_64-pc-windows-msvc`)**

- MSVC C/C++ build tools (VS 2022) + Windows SDK - for `cc` builds and `fxc.exe` (offline
  HLSL → DXBC compile in `build.rs`)
- LLVM / libclang on `PATH` (or `LIBCLANG_PATH` set) - for `psd-sdk-sys` / `heif-sys` bindgen
- Inno Setup 6 and ImageMagick, for `scripts/build-installer.ps1` only

**macOS (`aarch64-apple-darwin`)**

- **Full Xcode**, not just the Command Line Tools, plus the separately-downloaded Metal toolchain
  on Xcode 16+ (`xcodebuild -downloadComponent MetalToolchain`). `build.rs` compiles the Metal
  shader offline, so this is needed for _every_ build. Verify by **running**
  `xcrun -sdk macosx metal --version` - `xcrun --find metal` succeeds even when the toolchain is
  absent, because what it finds is a stub that only fails when used.
- Nothing else: libclang for bindgen ships inside Xcode and `clang-sys` finds it unaided.
- Homebrew `cmake ninja meson nasm pkg-config` plus a bootstrapped vcpkg are needed **only** to
  re-vendor the HEIF stack (`crates/heif-sys/vendor/VENDOR.txt`), never for a normal build.

The Rust crates are fetched automatically by `cargo`. The external artifacts to vendor are the
`psd_sdk` C++ source (into `crates/psd-sdk-sys/vendor/`, for the PSD decoder) and the prebuilt
static `libheif` + `libde265` + `dav1d` libs (into `crates/heif-sys/vendor/<target>/`, one
directory per target, for AVIF/HEIF/HEIC). See each crate's `vendor/VENDOR.txt` for the recipe.

## License

Fire is MIT licensed - see [LICENSE](LICENSE).

The Fire binary is statically linked, so it contains code from ~160 other projects.
[CREDITS.md](CREDITS.md) says who wrote them; [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)
carries the formal per-package copyright notices, with full license texts in [licenses/](licenses/).
The installer and the `.dmg` ship all of it alongside the binary.

One dependency is copyleft: **libheif** and **libde265** are LGPL-3.0-only. Fire links them
statically, so you are entitled to relink Fire against your own modified builds of those
libraries - the exact static libraries are in `crates/heif-sys/vendor/<target>/lib/` and the recipe
that produced them is in `crates/heif-sys/vendor/VENDOR.txt`. See the LGPL section of
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for details.
