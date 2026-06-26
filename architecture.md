# Fire — Architecture

A Windows source-format image viewer optimized for *time-to-first-pixel* when
double-clicking a file in Explorer. Every design choice below traces back to one goal:
the image should be on screen as close to instantly as possible.

Fire is a **single, self-contained native Win32 application** that renders on the GPU via a
lean **Direct3D 11** device created when the window opens (no warm-up). There is no resident
background daemon and no separate launcher stub — the GPU device is cheap enough to create on
launch, so nothing needs to be kept warm.

---

## 1. Core insight and the pivots

The dominant cost of "double-click → pixels on screen" is **process cold-start plus
decode**, not draw. A 2K PNG decodes in single-digit milliseconds; the headline cost is
getting a process to `main()` and a window on screen.

**Original design — a resident GPU daemon.** Cold-start was eliminated with a hidden,
login-started process holding a warm `wgpu` device and a pooled window, fronted by a tiny
launcher stub. That choice cascaded a heavy stack — `winit` + `wgpu` + `egui` + a login
service — all to support a *cross-platform GPU* app, at a resident ~1.1 GB commit charge.

**First pivot — drop the daemon (and, at first, the GPU).** A throwaway pure-CPU prototype
(softbuffer) was measured head-to-head against the live GPU daemon on an 8192×4096 PNG:

| | GPU daemon | CPU path |
|---|---|---|
| Working set | 313 MB | **154 MB** |
| Committed (private) | 1,123 MB | **137 MB** |
| Threaded render | sub-ms | **1.4 ms magnify / 3.4 ms minify avg** (max 8.75 ms) |

Decode (≈392 ms, zune) is identical on both paths and dominates time-to-first-pixel. The
real lesson was **not "CPU beats GPU"** — it was that a *cold launch of a small native exe is
cheap enough that no residency is needed*. That deleted the daemon, the launcher stub, and
the entire winit/wgpu/egui/login-service stack, leaving a lean native Win32 app (the
XnView-Classic model). The first cut of that app shaded every pixel on the CPU.

**Second pivot — bring back a *non-resident* GPU device for presentation.** Pure-CPU shading
re-runs the whole per-pixel pipeline for *every* surface pixel on *every* pan/zoom event. On a
large window at a high refresh rate (a 240 Hz monitor), that pegged ~40% of a CPU during fast
interaction and never felt as smooth as the old GPU build — even after an interactive-resolution
preview and a prefiltered mip chain. The fix restores GPU-class interaction without bringing
back anything the first pivot killed:

- The image is uploaded **once** as a D3D11 texture with a hardware-generated mip chain.
- Pan, zoom, exposure, channel, and tonemap become an **80-byte constant buffer**; each frame
  is one fullscreen-triangle draw that re-samples the texture — **~0 CPU per frame**.
- A **DXGI flip-model swapchain** paces presentation to vsync, so interaction is tear-free and
  smooth at the monitor's true refresh.

Crucially, the D3D11 device is created **when the window opens**, not warmed by a daemon. The
daemon existed only to amortize GPU warm-up against cold-start; once cold-start proved cheap,
its justification was gone — and a per-window device created on launch inherits none of its
residency cost. So the architecture keeps every win of the first pivot (no daemon, no stub,
lean native Win32, cheap cold start) **and** the smoothness of GPU presentation. winit, wgpu,
and egui stay gone; the GPU path is a few hundred lines of typed COM against the `windows` crate.

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
│  │  │  D3D11 swapchain — the image region              │   │  │
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

## 5. Rendering pipeline (GPU)

- **Stack:** **Direct3D 11** with a **DXGI flip-model swapchain** (`DXGI_SWAP_EFFECT_FLIP_DISCARD`)
  on the child view window's HWND. The decoded image is uploaded **once** as a GPU texture; a
  short HLSL vertex+pixel shader (compiled at startup via `D3DCompile`) does the sampling and the
  whole color pipeline. The device is created lazily at window open — hardware preferred, with
  the **WARP** software rasterizer as a fallback for RDP/headless — so there is no warm-up daemon.
- **Window split:** a top-level **frame** window owns the message loop and paints the GDI chrome
  (toolbar + status bar); a **child "view" window** in the middle owns the swapchain.
  `WS_CLIPCHILDREN` lets the frame repaint chrome without touching the image and vice-versa, and
  makes the view's client rect *exactly* the image region (no chrome insets in the view math).
- **The image is a texture, not a per-frame computation.** On adopt, the decoded pixels are
  uploaded to a `USAGE_DEFAULT` texture created with a full mip chain
  (`D3D11_RESOURCE_MISC_GENERATE_MIPS`); `GenerateMips` builds the pyramid on the GPU. Each of the
  four `PixelFormat`s maps to a native DXGI format (see §5.1). After that, pan / zoom / exposure /
  channel / tonemap are just values in an **80-byte constant buffer**; the source texture never
  changes until a new image is opened.
- **Per-frame work is one draw.** A frame maps the constant buffer (`MAP_WRITE_DISCARD`), writes
  the view transform + display state, and issues a single **fullscreen-triangle** draw; the pixel
  shader inverse-maps each output pixel into image space and samples the texture. There is no
  vertex buffer and no CPU per-pixel work — pan/zoom change a transform, not pixels, so
  interaction cost is independent of image resolution and of zoom-out factor.
- **Sampling:** a **point** sampler when magnifying (crisp 1:1 texels) and an **anisotropic +
  mipmapped** sampler when minifying. Hardware anisotropy + the GPU mip chain replace the
  CPU-built prefiltered pyramid and the on-the-fly box average entirely, and give better
  anti-aliasing (anisotropic > trilinear) at no per-frame CPU cost.
- **Presentation is vsync-paced.** `Present(1, …)` on the flip swapchain blocks to the monitor's
  refresh, so fast pan/zoom is tear-free and smooth at high refresh rates (e.g. 240 Hz) while the
  CPU sits near idle. Rendering is **event-driven** — a frame is drawn only on `WM_PAINT` (driven
  by `InvalidateRect` after an input or a decode), so an idle window costs nothing on either the
  CPU or the GPU.
- **Repaint / wakeups:** view changes call `InvalidateRect` → `WM_PAINT` → one present; decode
  results and forwarded opens reach the UI thread by `PostMessage(frame, WM_APP_*)`.

### 5.1 Per-pixel color pipeline (HLSL)

The pixel shader mirrors what the old CPU shader did, per output pixel, in linear light. The
source format determines how the texture is uploaded and decoded to linear:

| `PixelFormat` | DXGI texture format | → linear |
|---|---|---|
| `Rgba8Unorm` | `R8G8B8A8_UNORM_SRGB` | hardware sRGB-decode on sample |
| `Rgba16Unorm` | `R16G16B16A16_UNORM` | shader sRGB→linear (sample already normalized) |
| `Rgba16Float` | `R16G16B16A16_FLOAT` | already linear |
| `Rgba32Float` | `R32G32B32A32_FLOAT` | already linear |

Common tail, in shader order: sample (point/aniso per §5) → **HDR only** (float formats):
exposure `×2^stops`, then tonemap (Reinhard default / ACES toggle) → channel isolation (solo
R/G/B/A grayscale; alpha shown literally) → checkerboard composite over transparency (linear
0.45/0.21).

The shader outputs **linear**; the swapchain's render-target view is created as a `*_SRGB` format
so the hardware does the final sRGB encode on write. (The flip model disallows an `*_SRGB`
*swapchain* format, so the backbuffer is `R8G8B8A8_UNORM` and only the **RTV** is `_UNORM_SRGB` —
the standard trick.) The backdrop/letterbox clear color is the theme-aware chrome color, unpacked
from its `0x00RRGGBB` value and sRGB-decoded to linear on the CPU so it matches.

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
- **HDR display:** tonemap to SDR in the shader with an exposure-stops control (works on any
  monitor). The float source is sampled and tonemapped live each frame, so exposure/operator
  changes are free. A true HDR (scRGB / 10-bit) swapchain is now *possible* with the D3D11 flip
  swapchain — deferred; current output is tonemap-to-SDR.

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
- Pan / zoom / fit / 1:1; LMB drag-pan (the image can be pushed fully off any edge — Fit/1:1
  recenters it); mouse-wheel and RMB-vertical-drag zoom, both about the cursor.
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
│  ├─ fire/           # the viewer exe: Win32 shell, D3D11 render, decode pool, pipe
│  ├─ fire-decode/    # uniform decode core (zune/image/exr/psd_sdk/lcms2)
│  ├─ fire-ipc/       # pipe wire format for single-instance forwarding (shared)
│  └─ psd-sdk-sys/    # FFI bindings + cc build of psd_sdk
├─ assets/            # fire.ico + icon source
├─ installer/         # Inno Setup script
└─ Cargo.toml         # workspace
```

Key dependencies: `windows` (typed COM for the D3D11 device + DXGI flip swapchain + HLSL
compile), `windows-sys` (Win32: window/message loop, GDI, DWM, DPI, pipe, mutex, registry),
`zune-image`/`image`/`exr`/`lcms2`/`psd_sdk` (decode), `serde`/`toml`/`notify` (config),
`arboard` (clipboard), `crossbeam-channel` (worker messaging). No winit, wgpu, egui,
pollster, or softbuffer.

---

## 13. Build and distribution

- `cargo build --release` produces a **single `fire.exe`**. It links the D3D11/DXGI system DLLs
  (present on every supported Windows; no redistributable, no bundled runtime) and compiles its
  HLSL at startup. The C++ `psd_sdk` builds via a `cc`/`bindgen` build script in `psd-sdk-sys`.
  The Fire `.ico` + version/product metadata are embedded via `winresource`.
- **Unsigned installer** (Inno Setup) for now: installs `fire.exe`, registers the `HKCU`
  file associations, and provides clean uninstall. No `Run`/autostart entry — there is no
  daemon to start. (No code signing yet — expect a SmartScreen prompt on first run.)

---

## 14. v1 scope vs. deferred

**In v1:** single self-contained native Win32 exe; configurable NewWindow / SingleInstance
lifecycle with **foreground activation on the forward path (§4.1)**; GPU (D3D11) shader
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
  lean native exe reaches first-pixel fast. Creating the D3D11 device + flip swapchain and
  compiling the HLSL now happen on the launch path; they are cheap (low-ms) but real, so keep
  them lean and off the critical path to the first decode where possible. If a heavy dependency
  creeps back in, the cold-start cost the daemon was designed to avoid reappears.
- **GPU device loss — deliberately unhandled.** A D3D11 device can be lost (TDR, driver update,
  GPU reset). The renderer does **not** recreate the device/swapchain on `DXGI_ERROR_DEVICE_REMOVED`,
  by design: this is a stateless viewer (no unsaved data), so the recovery story is "relaunch."
  Re-opening the file is one keystroke and costs nothing a user would notice. WARP remains a
  fallback only at *creation* time (no hardware / RDP), not a mid-session failover.
- **Foreground lock (§4.1).** Only relevant in SingleInstance mode, but the easiest thing
  to get wrong and the most visible when it is: without the `AllowSetForegroundWindow`
  handoff, a forwarded open silently fails to come to the front.
- **psd_sdk is C++.** Budget time for the `-sys` crate (bindgen + `cc`) and treat every FFI
  call as a panic boundary (`catch_unwind`, validated inputs) so a malformed file can't take
  down the viewer process.
- **ICC + zune tension.** Honoring profiles forces some formats off the zune hot path onto
  the `image` decoder that exposes ICC bytes; verify which formats this affects so you know
  where the fast path actually applies.
- **Large-image RAM (+ VRAM).** The decoded image is retained in RAM (the texture-upload source
  and the pixel-inspector backing) *and* lives as a GPU texture with a mip chain (~4/3× its size
  in VRAM). The `max_dim` guard bounds the worst case; revisit if gigapixel sources become common
  (tiled/virtual texturing, v2).
- **First-run UX.** Unsigned installer → SmartScreen warning; document the "More info → Run
  anyway" step until signing is added.
