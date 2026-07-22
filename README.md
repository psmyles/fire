# Fire

**Fire** - *Fast Image REview* - is an image viewer optimized for **time-to-first-pixel**
when double-clicking a file in Explorer. It has features to help in game development workflows that regular image viewers usually don't cover. Fire was built because of my frustrations with existing image viewers: slow, and no one covers everything I want from an image viewer. Fire is also strictly an image viewer and NOT an editor: good image editors already exist, please use them for editing, Fire can launch the image into them if you set it up for that. 

## Features
- View the contents of individual R, G, B, and A channels
- View images against different backdrops (black, white, grey, checkerboard)
- Flipbook player with automatic grid detection logic and playback controls
- Support for all source image formats needed for game development
- Tonemapping and exposure controls for HDR images
- Uses the fastest possible library for decoding each image format  
- Hot-reload: the displayed image re-decodes automatically when its file changes on disk
- The image is decoded off-thread and presented on the GPU
through a lean Direct3D 11 flip-model swapchain
- Perfectly smooth frame locked zoom and pan operations even on huge image files
- DPI-aware, dark-mode-aware toolbar, status bar, flipbook controls and settings window drawn
by **Dear ImGui** into the same backbuffer as the image
- An octagon overlay mode to visualize how image would get cropped by octagon polygon shape in VFX systems in game engines
- A customizable context menu that covers basic operations and allows for user defined behavior

See [architecture.md](architecture.md) for the full design.

## Workspace

```
crates/
  fire/          the viewer exe - native Win32 window, D3D11 render, decode pool, optional
                 single-instance pipe (the whole app; Explorer launches it directly)
  fire-decode/   uniform decode core: bytes → (pixels, format, bit depth, ICC)
  fire-ipc/      named-pipe wire format for single-instance forwarding, dependency-light
  psd-sdk-sys/   FFI to vendored psd_sdk C++ (cc + bindgen)
  heif-sys/      FFI to vendored libheif + libde265 + dav1d for AVIF / HEIF / HEIC (cc + bindgen)
```

## Build & dev

```sh
cargo build --workspace
cargo run -p fire -- C:\path\img.png   # open an image
cargo run -p fire                      # open with no image (drag/drop or forward later)
cargo test -p fire                     # render/view + window unit tests
cargo test -p fire-decode              # decode core tests (incl. tests/heif.rs end-to-end)
cargo build -p fire                    # debug build
cargo build -p fire --release          # release build (the single fire.exe)
pwsh scripts/build-installer.ps1       # build dist/Fire-<version>-Setup.exe (Inno Setup)
```

Product metadata (name/version/publisher/…) lives in `product.json` at the repo root - `build.rs`
reads it into the exe's version resource and `FIRE_*` env vars, and the installer script reads the
same file, so bumping the version there flows into the app and the installer alike.

Instance behavior is a config setting (`%APPDATA%\fire`): **NewWindow** (default - every
launch is its own independent process) or **SingleInstance** (later launches forward the
path to the running window over a named pipe and raise it to the foreground).

## Toolchain prerequisites (Windows, x86_64-pc-windows-msvc)

- Rust stable (target `x86_64-pc-windows-msvc`, pinned in `rust-toolchain.toml`)
- MSVC C/C++ build tools (VS 2022) + Windows SDK - for `cc` builds, Win32 linkage, and
  `fxc.exe` (offline HLSL → DXBC compile in `build.rs`)
- LLVM / libclang on `PATH` (or `LIBCLANG_PATH` set) - for `psd-sdk-sys` / `heif-sys` bindgen

The Rust crates are fetched automatically by `cargo`. The external artifacts to vendor are
the `psd_sdk` C++ source (into `crates/psd-sdk-sys/vendor/`, for the PSD decoder) and the
prebuilt static `libheif` + `libde265` + `dav1d` libs (into `crates/heif-sys/vendor/`, for
AVIF/HEIF/HEIC). See each crate's `vendor/VENDOR.txt` for the recipe.

