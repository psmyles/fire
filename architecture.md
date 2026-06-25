# Fire — Architecture

A Windows source-format image viewer optimized for *time-to-first-pixel* when
double-clicking a file in Explorer. Every design choice below traces back to one goal:
the image should be on screen as close to instantly as possible.

Fire is a **single, self-contained native Win32 application** that renders on the CPU.
There is no GPU device, no D3D runtime, no resident background daemon, and no separate
launcher stub.

---

## 1. Core insight and the pivot

The dominant cost of "double-click → pixels on screen" is **process cold-start plus
decode**, not draw. A 2K PNG decodes in single-digit milliseconds; the headline cost is
getting a process to `main()` and a window on screen.

The original design eliminated cold-start with a *resident GPU daemon* (a hidden
login-started process holding a warm `wgpu` device and a pooled window) fronted by a tiny
launcher stub. That choice cascaded a heavy stack — `winit` + `wgpu` + `egui` + a login
service — all of which existed to support a *cross-platform GPU* app.

A throwaway pure-CPU prototype (softbuffer) was then measured head-to-head against the
live GPU daemon on an 8192×4096 PNG:

| | GPU daemon | CPU path |
|---|---|---|
| Working set | 313 MB | **154 MB** |
| Committed (private) | 1,123 MB | **137 MB** |
| Threaded render | sub-ms | **1.4 ms magnify / 3.4 ms minify avg** (max 8.75 ms) |

Decode (≈392 ms, zune) is identical on both paths and dominates time-to-first-pixel.
Interaction is indistinguishable; memory is ~½ the working set and ~⅛ the commit charge;
and **removing the GPU deletes the D3D/driver runtime, the device-loss recovery path, and
the GPU warm-up that was the entire justification for a resident daemon.**

So the architecture pivoted: a *Windows-only CPU* viewer can shed winit, wgpu, egui, and
the daemon/stub split, and become a lean native Win32 app (the XnView-Classic model). A
cold launch of a small native exe is cheap enough that no residency is needed — the
process simply lives while it has a window open.

---

## 2. High-level architecture

```
Explorer double-click
        │  (file association → ProgID → fire.exe "C:\path\img.png")
        ▼
┌──────────────────────────────────────────────────────────────┐
│                          fire.exe                              │
│                                                                │
│  main(): Per-Monitor-V2 DPI awareness → read config →          │
│          NewWindow: open our own window                        │
│          SingleInstance: own the mutex+pipe, or forward & exit │
│                                                                │
│  ┌── frame window (Win32) ─────────────────────────────────┐  │
│  │  GDI toolbar (top) · GDI status bar (bottom)            │  │
│  │  owns message loop, title, size, lifecycle, theme       │  │
│  │  ┌── child "view" window ───────────────────────────┐   │  │
│  │  │  softbuffer surface — the image region           │   │  │
│  │  └──────────────────────────────────────────────────┘   │  │
│  └─────────────────────────────────────────────────────────┘  │
│                                                                │
│  decode worker pool (off-thread)  ──PostMessage──▶ UI thread   │
│  fire-decode core (zune / image / exr / psd_sdk / lcms2)       │
└──────────────────────────────────────────────────────────────┘
```

The thing Explorer launches is the whole app. There is no warm-up to amortize, so there
is nothing to keep resident.

---

## 3. Process model and lifecycle

Instance behavior is a **user setting** (`instance_mode` in the config), read *before* any
window is created:

- **NewWindow (default):** every launch opens its own window in its own process. No mutex,
  no pipe, nothing listening. The process exits when its window closes. This is the
  simplest, lowest-latency path and the right default for "double-click opens a viewer".
- **SingleInstance:** the first launch acquires a named mutex, opens its window, *and*
  serves a named pipe. Later launches detect the mutex, forward their path over the pipe to
  the running window (which reuses the one window, reset to fit per file), and exit. The
  pipe lives only inside the running window's process — it is **not** a resident daemon.

No autostart, no login residency in either mode. "Residency" is implicit: a process lives
exactly as long as it has a window open.

The single-instance mutex is `Local\`-scoped (per-login session), so fast-user-switching
gives each session its own instance; we explicitly do not want one machine-wide instance.

---

## 4. IPC and foreground activation

IPC exists only for **SingleInstance** mode.

- **Transport:** Windows named pipe (`\\.\pipe\fire`).
- **Framing:** length-prefixed messages (`u32` little-endian length + payload).
- **Payload:** protocol version + window-mode + activate flag + UTF-8 path. The wire
  format lives in the dependency-light `fire-ipc` crate so the forward path stays cheap.
- A forwarding launch writes one message and disconnects; the running instance routes it to
  its window (currently a single reused window; tabs/compare are future work).

### 4.1 Foreground activation (the one real trap)

A process that does not currently own the foreground **cannot** raise its own window:
Windows blocks `SetForegroundWindow` from it. When a later launch forwards a file, the
already-running instance would swap the image in but stay behind other windows — the
"instant open" would feel half-broken.

The fix uses the one process that *does* hold foreground rights at the moment of the
double-click: the forwarding launch, because Explorer started it.

1. The forwarder resolves the running instance's PID (via the connected pipe / its own
   spawn).
2. **As it sends the open request**, it calls `AllowSetForegroundWindow(owner_pid)`,
   handing over its one-shot foreground grant.
3. The running instance, on receipt (posted to its UI thread via `PostMessage`), calls
   `ShowWindow` + `SetForegroundWindow` on the target window promptly, before the grant
   lapses.

This path only runs in SingleInstance mode; NewWindow has nothing to forward.

---

## 5. Rendering pipeline (CPU)

- **Stack:** pure-CPU **softbuffer**. The decoded image is held in RAM and shaded into a
  packed `0x00RRGGBB` framebuffer that softbuffer blits to the window via GDI. No GPU
  device, no swapchain, no shader compilation, and therefore **no device-loss path** to
  handle.
- **Window split:** a top-level **frame** window owns the message loop and paints the GDI
  chrome (toolbar + status bar); a **child "view" window** in the middle is the softbuffer
  target. `WS_CLIPCHILDREN` lets the frame repaint chrome without touching the image and
  vice-versa, and makes the view's client rect *exactly* the image region (no chrome insets
  in the view math).
- **Threaded shading:** per output pixel, inverse-map into image space and fetch a *linear*
  RGBA sample. The framebuffer is split into horizontal row bands across
  `available_parallelism()` via `std::thread::scope`. Cost is O(surface pixels), nearly
  independent of source resolution.
- **Sampling:** nearest-neighbor when magnifying; **box-average** over the minify footprint
  (clamped 1–6 taps) in linear light. No mip chain — we accept mild shimmer below ~0.16
  zoom in exchange for zero preprocessing and zero extra memory.
- **Repaint** is driven by `InvalidateRect` → `WM_PAINT`; decode results and forwarded
  opens reach the UI thread by `PostMessage(frame, WM_APP_*)`.

### 5.1 Per-pixel color pipeline

Each sample is taken to **linear RGBA** per pixel format, then run through a common tail:

| `PixelFormat` | Buffer (native-endian) | → linear |
|---|---|---|
| `Rgba8Unorm` | u8 RGBA | sRGB LUT |
| `Rgba16Unorm` | u16 RGBA | `/65535`, sRGB→linear |
| `Rgba16Float` | f16 RGBA | half→f32 (already linear) |
| `Rgba32Float` | f32 RGBA | read directly (already linear) |

Common tail, in order: (minify) average footprint in linear → **HDR only** (float
formats): exposure `×2^stops`, then tonemap (Reinhard default / ACES toggle) → channel
isolation (solo R/G/B/A grayscale; alpha shown literally) → checkerboard composite over
transparency (linear 0.45/0.21) → **always** sRGB-encode via LUT (softbuffer presents raw
bytes, so the encode is done in software). The whole pipeline runs in linear light. An
8-bit opaque-RGB magnify fast path skips the linear round-trip for the common case.

LUTs (`lin[256]`, `srgb[4097]`) are built once per surface.

---

## 6. Decode pipeline

All decoders live behind a single **`fire-decode`** crate exposing a uniform
"bytes → (pixels, format, bit depth, optional ICC profile)" interface. Routing is by magic
bytes:

| Format(s) | Decoder |
|---|---|
| PNG, JPEG, `.hdr`, BMP, QOI, PPM, WebP, farbfeld, JXL | **zune** — hot path |
| TIFF, GIF, TGA | `image` crate (formats zune doesn't decode) |
| EXR | `exr` crate (pure Rust) → 32-bit float RGBA |
| PSD | **`psd_sdk`** (Molecular Matters, C++) over FFI → merged composite |
| ICC transforms | **Little CMS** (`lcms2`) over FFI |

Notes:
- **Decode speed is the project's primary metric.** The common formats run through zune
  with `DecoderOptions::new_fast` (platform intrinsics + unsafe fast paths). Output is
  normalized to interleaved RGBA in the source bit depth (8/16/float).
- **ICC fallback:** zune does not reliably surface embedded ICC for every format. When a
  profile must be honored, the file is routed through the `image`/format-specific decoder
  that exposes the profile, then transformed with `lcms2`.
- **FFI safety:** every C/C++ boundary (`psd_sdk`, `lcms2`) is wrapped in `catch_unwind`
  and runs on a decode worker, so a malformed file cannot take down the viewer process.
- **Oversized images:** `DecodeOptions::max_dim` is a **CPU/RAM guard**, not a GPU texture
  limit (an RGBA8 bitmap at 16384² is ~1 GiB; float HDR is 4×). It defaults to 16384, is
  configurable, and anything past it is CPU-downscaled to fit, recording the original size
  so the pixel inspector can note that a read came from the downscaled copy. Decode itself
  raises zune's internal guard well past this so large sources reach the downscale pass
  rather than being rejected outright. (Tiled/virtual texturing deferred to v2.)

---

## 7. Color management

- **Working space:** sRGB for 8/16-bit LDR; linear for float/half HDR.
- **ICC honored:** embedded profiles (PNG `iCCP`, JPEG APP2, TIFF tag, PSD resource) are
  parsed and transformed into the working space via `lcms2`. Files without a profile fall
  back to the sRGB assumption.
- **HDR display:** tonemap to SDR with an exposure-stops control (works on any monitor).
  True HDR swapchain output is not applicable to the CPU/GDI present path.

---

## 8. Native UI chrome (DPI + dark mode)

The toolbar and status bar are **custom GDI-painted**, not Win32 common controls. Common
controls were rejected because they have no documented dark-mode support; painting the
chrome ourselves gives full color control for light/dark with zero undocumented APIs.

- **Toolbar:** channel-isolation toggles (R/G/B/A/RGB), fit/1:1, HDR tonemap (ACES) and
  exposure (EV ±), with hover highlight, blue active-state for toggles, and disabled/dimmed
  state (e.g. ACES/EV are greyed out on non-HDR images). Buttons hit-test to the same view
  actions the keybinds drive — one state path.
- **Status bar:** file name, format, W×H, bit depth / channel layout, ICC presence, and on
  the right the zoom % (plus `EV ±` for HDR). The pixel-inspector eyedropper readout slots
  in here.
- **DPI awareness:** `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` is declared
  before any window exists, so the non-client area auto-scales and `WM_DPICHANGED` fires on
  monitor moves. All chrome metrics and the Segoe UI font scale from `GetDpiForWindow`; on
  `WM_DPICHANGED` we adopt the OS-suggested rect and rebuild metrics/font/layout.
- **Dark mode:** the system preference is read from the registry (`AppsUseLightTheme`); the
  title bar is darkened via `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`; the
  chrome and the letterbox backdrop use a self-painted dark/light palette; `WM_SETTINGCHANGE`
  re-skins live when the user flips the system theme.

---

## 9. Window / session model and configuration

- **Session model:** a window holds a current image, view state (zoom, pan, channel
  toggles, exposure, tonemap), and — planned — a folder cursor for ←/→ navigation across
  siblings.
- **Instance mode:** NewWindow (default) or SingleInstance, per §3.
- **Settings:** stored as **TOML** in `%APPDATA%`, editable directly; external edits
  hot-reload via the `notify` crate. An in-app settings dialog is native (no egui).
- **Future:** a third mode — compare two images side-by-side in one window, or tabs — is
  anticipated; it reuses the frame/child-view split (one view child per slot).

---

## 10. Viewer features

- Channel isolation (solo R/G/B/A, alpha-as-grayscale).
- Pan / zoom / fit / 1:1; mouse-wheel zoom-to-cursor; drag-pan.
- HDR exposure (stops) + tonemap operator (Reinhard / ACES).
- **Pixel inspector** (planned): eyedropper RGBA readout + a zoomed pixel grid at high
  magnification, custom-painted into the view child with GDI `TextOut`/`DrawText` (system
  font — no rasterizer dependency), reading `current_image` via `view.screen_to_image()`.
- Folder navigation (←/→ walks sibling files); clipboard (`arboard`); "open in configured
  editor"; configurable background and alpha checkerboard.

---

## 11. Explorer integration

- **Association only** (no thumbnail handler in v1): register an `HKCU` ProgID, declare
  supported extensions (`.jpg .jpeg .png .tga .tif .tiff .psd .exr .hdr` …), and point them
  at `fire.exe`. Appears in "Open with".
- Decoders are factored into the standalone `fire-decode` crate, so an `IThumbnailProvider`
  handler can be added later as a separate `cdylib` reusing that core with no rework.

---

## 12. Workspace layout

```
fast-image-viewer/
├─ crates/
│  ├─ fire/           # the viewer exe: Win32 shell, CPU render, decode pool, pipe
│  ├─ fire-decode/    # uniform decode core (zune/image/exr/psd_sdk/lcms2)
│  ├─ fire-ipc/       # pipe wire format for single-instance forwarding (shared)
│  └─ psd-sdk-sys/    # FFI bindings + cc build of psd_sdk
├─ assets/            # fire.ico + icon source
├─ installer/         # Inno Setup script
└─ Cargo.toml         # workspace
```

Key dependencies: `softbuffer` (CPU present), `raw-window-handle` (hands softbuffer the
HWND), `windows-sys` (Win32: window/message loop, GDI, DWM, DPI, pipe, mutex, registry),
`zune-image`/`image`/`exr`/`lcms2`/`psd_sdk` (decode), `serde`/`toml`/`notify` (config),
`arboard` (clipboard), `crossbeam-channel` (worker messaging). No winit, wgpu, egui, or
pollster.

---

## 13. Build and distribution

- `cargo build --release` produces a **single `fire.exe`** — no D3D runtime, no GPU driver
  coupling, smaller binary. The C++ `psd_sdk` builds via a `cc`/`bindgen` build script in
  `psd-sdk-sys`. The Fire `.ico` + version/product metadata are embedded via `winresource`.
- **Unsigned installer** (Inno Setup) for now: installs `fire.exe`, registers the `HKCU`
  file associations, and provides clean uninstall. No `Run`/autostart entry — there is no
  daemon to start. (No code signing yet — expect a SmartScreen prompt on first run.)

---

## 14. v1 scope vs. deferred

**In v1:** single self-contained native Win32 exe; configurable NewWindow / SingleInstance
lifecycle with **foreground activation on the forward path (§4.1)**; pure-CPU threaded
render with channel/alpha/gamma/exposure/tonemap; async worker decode; zune + image + exr +
psd_sdk decoders; ICC honoring via lcms2; tonemap-to-SDR HDR with exposure; downscale-to-fit
RAM guard; **DPI-aware, dark-mode-aware GDI toolbar + status bar**; open-in-editor +
clipboard; association-only Explorer integration; unsigned installer.

**In progress / deferred:** pixel inspector, native settings dialog + background-color
picker, exposure trackbar, toolbar tooltips, folder ←/→ navigation; compare/tabs mode;
Explorer `IThumbnailProvider`; code signing.

---

## 15. Key risks and notes

- **Cold start must stay cheap.** The whole bet of dropping the resident daemon is that a
  lean native exe reaches first-pixel fast. If a heavy dependency creeps back in, the
  cold-start cost the daemon was designed to avoid reappears — keep the launch path thin.
- **Foreground lock (§4.1).** Only relevant in SingleInstance mode, but the easiest thing
  to get wrong and the most visible when it is: without the `AllowSetForegroundWindow`
  handoff, a forwarded open silently fails to come to the front.
- **psd_sdk is C++.** Budget time for the `-sys` crate (bindgen + `cc`) and treat every FFI
  call as a panic boundary (`catch_unwind`, validated inputs) so a malformed file can't take
  down the viewer process.
- **ICC + zune tension.** Honoring profiles forces some formats off the zune hot path onto
  the `image` decoder that exposes ICC bytes; verify which formats this affects so you know
  where the fast path actually applies.
- **Large-image RAM.** The decoded image is retained in RAM (sampling source for shading and
  the inspector). The `max_dim` guard bounds the worst case; revisit if gigapixel sources
  become common (tiled/virtual texturing, v2).
- **First-run UX.** Unsigned installer → SmartScreen warning; document the "More info → Run
  anyway" step until signing is added.
