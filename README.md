# fast-image-viewer

A Windows source-format texture viewer optimized for **time-to-first-pixel** when
double-clicking a file in Explorer. The dominant cost of "double-click → pixels on
screen" is process cold-start, so the architecture eliminates it with a **resident
daemon** (GPU device + a pre-warmed window kept hot at login) fronted by a **tiny
launcher stub** that Explorer actually invokes.

See [texture-viewer-architecture.md](texture-viewer-architecture.md) for the full
design and `.claude/plans/` for the phased implementation plan.

## Workspace

```
crates/
  texview-ipc/      shared named-pipe protocol (stub <-> daemon), dependency-light
  texview-stub/     tiny launcher exe Explorer invokes (connect → send → exit)
  texview-daemon/   resident process: ipc server, session, wgpu render, egui UI
  texview-decode/   uniform decode core: bytes → (pixels, format, bit depth, ICC)
  psd-sdk-sys/      FFI to vendored psd_sdk C++ (cc + bindgen)
installer/          Inno Setup script (Phase 6)
```

## Build & dev

```sh
cargo build --workspace
cargo run -p texview-daemon                 # start the resident daemon
cargo run -p texview-stub -- C:\path\img.png  # forward a file to the daemon
cargo run -p texview-daemon --example adapter_probe  # GPU adapter probe (Phase 0)
```

## Toolchain prerequisites (Windows, x86_64-pc-windows-msvc)

- Rust stable (1.96+)
- MSVC C/C++ build tools (VS 2022) + Windows SDK — for `cc` builds and Win32 linkage
- LLVM / libclang on `PATH` (or `LIBCLANG_PATH` set) — for `psd-sdk-sys` bindgen
- A GPU with the DX12 (or Vulkan) backend

The Rust crates are fetched automatically by `cargo`. The only external artifact to
vendor is the `psd_sdk` C++ source (into `crates/psd-sdk-sys/vendor/`), needed when
the PSD decoder lands in Phase 2.

## Status

Greenfield, under active construction. Phase 0 (workspace + toolchain proof) first;
the walking-skeleton end-to-end open (stub → pipe → daemon → one PNG on screen with
foreground activation) is Phase 1.
