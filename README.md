# fast-image-viewer

**Fire** — a Windows source-format image viewer optimized for **time-to-first-pixel**
when double-clicking a file in Explorer. It is a single, self-contained native Win32
app: the image is decoded off-thread and presented on the CPU (no GPU, no D3D runtime),
with a custom DPI-aware, dark-mode-aware toolbar and status bar painted with GDI.

See [architecture.md](architecture.md) for the full design and `.claude/plans/` for the
phased implementation plan.

## Workspace

```
crates/
  fire/          the viewer exe — native Win32 window, CPU render, decode pool, optional
                 single-instance pipe (the whole app; Explorer launches it directly)
  fire-decode/   uniform decode core: bytes → (pixels, format, bit depth, ICC)
  fire-ipc/      named-pipe wire format for single-instance forwarding, dependency-light
  psd-sdk-sys/   FFI to vendored psd_sdk C++ (cc + bindgen)
installer/       Inno Setup script (packaging)
```

## Build & dev

```sh
cargo build --workspace
cargo run -p fire -- C:\path\img.png   # open an image
cargo run -p fire                      # open with no image (drag/drop or forward later)
cargo test -p fire                     # render/view + decode unit tests
cargo build -p fire                    # debug build
cargo build -p fire --release          # release build
```

Instance behavior is a config setting (`%APPDATA%\fire`): **NewWindow** (default — every
launch is its own independent process) or **SingleInstance** (later launches forward the
path to the running window over a named pipe and raise it to the foreground).

## Toolchain prerequisites (Windows, x86_64-pc-windows-msvc)

- Rust stable (1.96+)
- MSVC C/C++ build tools (VS 2022) + Windows SDK — for `cc` builds and Win32 linkage
- LLVM / libclang on `PATH` (or `LIBCLANG_PATH` set) — for `psd-sdk-sys` bindgen

The Rust crates are fetched automatically by `cargo`. The only external artifact to
vendor is the `psd_sdk` C++ source (into `crates/psd-sdk-sys/vendor/`), needed for the
PSD decoder.

## Status

Under active construction. The native Win32 viewer with a Direct3D 11 GPU viewport (window,
threaded decode, pan/zoom/fit, channel isolation, HDR exposure/tonemap, DPI-aware dark/light
chrome) is in place; packaging and the remaining toolbar extras (pixel inspector, settings
dialog, folder navigation, clipboard) are in progress.

Icon source: https://commons.wikimedia.org/wiki/File:Fire-dynamic-color.png
