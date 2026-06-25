# Fast Texture Viewer — Architecture

A Windows source-format texture viewer optimized for *time-to-first-pixel* when
double-clicking a file in Explorer. Every design choice below traces back to one
goal: the image should be on screen as close to instantly as possible.

---

## 1. Core insight

The dominant cost of "double-click → pixels on screen" is **process cold-start**,
not decode. A 2K PNG decodes in single-digit milliseconds; a cold runtime can
burn hundreds of ms to seconds just reaching `main()`. The architecture therefore
eliminates cold-start entirely with a **resident daemon**, and treats decode as a
background concern that never blocks the window from appearing.

---

## 2. High-level architecture

```
Explorer double-click
        │  (file association → ProgID → stub.exe "C:\path\tex.tga")
        ▼
┌─────────────────┐   length-prefixed message    ┌──────────────────────────────┐
│  Launcher stub  │ ───────  named pipe  ──────▶  │        Resident daemon        │
│  (tiny Rust exe)│   { path, flags }             │  (auto-started at login,      │
│  connect+send+  │                               │   hidden, always warm)        │
│  exit (~ms)     │                               │                               │
└─────────────────┘                               │  • session/window manager     │
        │ if pipe absent                          │  • wgpu device + queue (warm) │
        └── spawn daemon, retry ──────────────────│  • pooled window               │
                                                   │  • decode worker pool          │
                                                   │  • texview-decode core         │
                                                   └──────────────────────────────┘
```

The thing Explorer actually launches is a **tiny Rust stub**. Rust is the right
choice for it precisely because there is no runtime to warm — the stub starts in
sub-millisecond territory, opens the pipe, forwards the path plus flags, and
exits. If no daemon is listening, it spawns one, waits for the pipe, and forwards.

---

## 3. Process model & lifecycle

- **Daemon auto-starts at login** via an `HKCU\...\Run` entry; it launches hidden
  and immediately warms the wgpu device, queue, a pooled window, and the decoders.
  Result: even the first open of the session is instant.
- **Stays resident** (no idle exit by default). Steady RAM cost is the deliberate
  trade for a guaranteed-warm experience.
- **Single instance** enforced by a named mutex; a second daemon launch detects
  the mutex and exits.
- **Crash resilience:** the launcher stub always re-checks the pipe and respawns
  the daemon if it died, so a *process* crash self-heals on the next open. Note
  this covers process death only — a surviving-process **device loss** is handled
  separately in §5.1, since the stub has no crash to detect there.

---

## 4. IPC protocol

- **Transport:** Windows named pipe (e.g. `\\.\pipe\texview`).
- **Framing:** length-prefixed messages (`u32` little-endian length + payload).
- **Payload:** UTF-8 path + flags (window-mode override such as "new window",
  "new tab"; activate/focus request).
- Stub writes one message and disconnects; daemon handles routing to the right
  window/session.

### 4.1 Foreground activation (mandatory — the #1 resident-daemon trap)

A background process **cannot** raise itself to the foreground: Windows blocks
`SetForegroundWindow` from any process that doesn't currently own the foreground,
so the daemon swapping a texture into its warm window will *succeed silently* —
the window stays behind others or merely flashes in the taskbar. That would make
the headline "instant" feature feel half-broken.

The fix exploits the one process that *does* hold foreground rights at the moment
of the double-click: the stub, because Explorer launched it.

1. The stub resolves the daemon's PID — either it just spawned it (knows the PID
   directly) or it queries the connected pipe via `GetNamedPipeServerProcessId`.
2. **Before/as it sends the pipe message**, the stub calls
   `AllowSetForegroundWindow(daemon_pid)`, handing its foreground right to the
   daemon.
3. The daemon, on receiving the message, immediately calls `SetForegroundWindow`
   (plus `ShowWindow`/restore-if-minimized) on the target window.

Timing matters: the granted right is one-shot and lapses on the next foreground
change, so the daemon must raise the window promptly, and the stub should not race
ahead and trigger an unrelated foreground change before the daemon acts. Keeping
the stub's "send → (optionally) brief wait → exit" tight is enough in practice.
On cold first-run the stub also spawns the daemon, so it has the PID in hand for
step 1 with no pipe query needed.

---

## 5. Rendering pipeline

- **Stack:** `winit` (windowing) + `wgpu` (GPU). wgpu's only real cost — device
  and instance creation — is paid once at daemon startup, so it never touches
  per-open latency. In exchange you get clean WGSL shader authoring.
- **Draw model:** decoded pixels upload to a GPU texture; a single textured quad
  is drawn. View transform (zoom/pan/fit/1:1) is a uniform.
- **WGSL fragment shader responsibilities:**
  - Channel isolation — solo R / G / B / A, alpha-as-grayscale.
  - Alpha visualization — checkerboard composite.
  - Gamma / sRGB encode for display.
  - Exposure (stops) + tonemap operator (ACES or Reinhard) for HDR sources.
- **Async decode → instant window:** on a pipe message the daemon shows the
  window immediately with a placeholder; decode runs on a worker thread; the
  texture is uploaded via `Queue::write_texture` (wgpu `Queue` is `Send + Sync`)
  and presented when ready. A 200 MB layered PSD still *feels* instant to open.

### 5.1 Device-loss recovery (mandatory for a resident daemon)

A login-resident daemon lives for days, and a long-lived GPU device *will*
eventually be lost while the process itself keeps running — none of these crash
the stub, so the respawn path in §3 never fires and opens silently render black:

- GPU driver update or reset,
- TDR (Timeout Detection & Recovery),
- sleep/resume,
- hybrid-GPU switch on laptops (iGPU ↔ dGPU / switchable graphics).

A fresh-per-launch app never has to handle this; a resident one must. So this is
**v1 scope, not a someday item.** The daemon installs a device-lost path that:

1. Detects loss — wgpu surfaces it via the device-lost callback and via
   `SurfaceError::Lost` / `Outdated` from `get_current_texture`.
2. Tears down and rebuilds the device + queue, all pipelines, bind groups, and
   the pooled window's surface/swapchain.
3. Re-decodes (or re-uploads from a retained CPU copy) the currently displayed
   image so the visible window recovers, not just future opens.

Reconfiguring the surface alone handles the lightweight `Lost`/`Outdated` cases;
full adapter loss requires the complete rebuild in step 2.

---

## 6. Decode pipeline

All decoders live behind a single **`texview-decode`** crate exposing a uniform
"bytes → (pixels, format, bit depth, optional ICC profile)" interface.

| Format(s)              | Decoder                                  |
|------------------------|------------------------------------------|
| JPEG, PNG, `.hdr`      | **zune** (`zune-jpeg`, `zune-png`, `zune-hdr`) — hot path |
| TGA, TIFF, GIF, BMP    | `image` crate (or dedicated `tiff` / `tga`) |
| EXR                    | `exr` crate (pure Rust)                  |
| PSD                    | **`psd_sdk`** (Molecular Matters, C++) over FFI |
| ICC transforms         | **Little CMS** (`lcms2`) over FFI        |

Notes:
- **Hot path stays on zune** for the common, profile-less JPEG/PNG case.
- **ICC fallback:** zune does not reliably surface embedded ICC bytes for every
  format. When a profile must be honored, route that file through the
  `image`/format-specific decoder that *does* expose the profile, then build a
  transform with `lcms2`.
- **FFI safety:** all C/C++ boundaries (`psd_sdk`, `lcms2`) are wrapped in
  `catch_unwind` and run on the decode worker, so a malformed file cannot take
  down the resident daemon.
- **Oversized images:** anything past the GPU max texture dimension (~16384) is
  CPU-downscaled to fit before upload; the pixel inspector notes when a value is
  read from the downscaled copy. (Tiled/virtual texturing deferred to v2.)

---

## 7. Color management

- **Working space:** sRGB for 8/16-bit LDR; linear for float/half HDR.
- **ICC honored:** embedded profiles (PNG `iCCP`, JPEG APP2, TIFF tag, PSD
  resource) are parsed and transformed into the working space via `lcms2`.
  Files without a profile fall back to the sRGB assumption.
- **HDR display:** tonemap to SDR with an exposure-stops control (works on any
  monitor). True HDR swapchain output is a deferred v2 option behind a setting.

---

## 8. Window / session model & configuration

- **Session model:** the daemon manages N windows; each window holds a current
  image, a folder cursor (for ←/→ navigation across siblings), and view state
  (zoom, pan, channel toggles, exposure). Tab mode = one window with multiple
  image slots.
- **Default behavior:** **reuse a single window** — a second open swaps the
  texture into the warm window (the fastest possible open). Overridable.
- **Configurable** to *new window per file* or *tabs*.
- **Settings:** stored as **TOML** in `%APPDATA%`, editable directly, *and* via an
  in-app **egui** panel (`egui-winit` + `egui-wgpu`). External edits hot-reload
  via the `notify` crate.

---

## 9. Viewer features (v1)

- Channel isolation (solo R/G/B/A, alpha view) — shader-driven.
- Pan / zoom / fit / 1:1, mouse-wheel zoom-to-cursor.
- **Pixel inspector:** eyedropper showing RGBA under the cursor (8-bit, float,
  hex) + a zoomed pixel grid at high magnification.
- Folder navigation (←/→ walks sibling files).
- **External handoff:** "open in configured editor" (e.g. Photoshop),
  copy-image-to-clipboard, copy-path.
- Configurable background, alpha checkerboard.
- **Top toolbar:** channel-isolation toggles (R/G/B/A/RGB), alpha-view toggle,
  background-color picker, fit/1:1, exposure control (HDR), open-in-editor. Drawn
  as an egui overlay (the same `egui-winit`/`egui-wgpu` integration already in the
  stack), so it's wired to the same uniforms the keybinds drive — no separate
  state path.
- **Bottom status bar:** file name, format, dimensions (W×H), bit depth /
  channel layout, color space / ICC profile name, file size on disk, current zoom
  %, and the eyedropper readout under the cursor. Also egui-drawn.

> Both bars are chrome over the same render surface, so account for them in the
> viewport math: the image's fit/center rectangle excludes the toolbar and
> status-bar heights. Make both toggleable (e.g. a borderless/zen mode) since some
> users want pixels edge-to-edge.

---

## 10. Explorer integration

- **Association only** (no thumbnail handler in v1): register an `HKCU` ProgID,
  declare supported extensions (`.jpg .jpeg .png .tga .tif .tiff .psd .exr .hdr`
  …), and point them at the stub. Appears in "Open with".
- Decoders are already factored into the standalone `texview-decode` crate, so an
  `IThumbnailProvider` handler can be added later as a separate `cdylib` reusing
  that core with no rework.

---

## 11. Suggested crate manifest

```toml
# daemon / shared
winit            # windowing
wgpu             # GPU
bytemuck         # POD casts for vertex/uniform data
pollster         # block on wgpu async init at startup
raw-window-handle
serde + toml     # config
notify           # config hot-reload
egui + egui-winit + egui-wgpu   # settings panel + overlay
crossbeam-channel or std mpsc   # worker <-> render messaging
windows          # named pipe, mutex, Run-key registration, shell assoc

# decode (texview-decode crate)
zune-image / zune-jpeg / zune-png / zune-hdr
image            # TGA, TIFF, GIF, BMP fallback (+ ICC-bearing paths)
exr              # OpenEXR
lcms2            # ICC transforms (FFI)
# psd_sdk via a -sys crate + bindgen/cc build script (C++)
```

> Confirm exact zune format coverage against current versions when you pin the
> manifest — that codec list moves, and it determines how much falls to `image`.

---

## 12. Workspace layout

```
texview/
├─ crates/
│  ├─ texview-stub/      # tiny launcher exe (Explorer target)
│  ├─ texview-daemon/    # resident process: ipc, session, render, ui
│  ├─ texview-decode/    # uniform decode core (zune/image/exr/psd_sdk/lcms2)
│  ├─ texview-ipc/       # pipe protocol + message types (shared)
│  └─ psd-sdk-sys/       # FFI bindings + cc build of psd_sdk
├─ installer/            # Inno Setup / cargo-wix script
└─ Cargo.toml            # workspace
```

---

## 13. Build & distribution

- `cargo build --release`; the C++ `psd_sdk` builds via a `cc`/`bindgen` build
  script in `psd-sdk-sys`.
- **Unsigned installer** (Inno Setup or cargo-wix) for now: installs the stub +
  daemon, registers the `HKCU` file associations, adds the `Run` auto-start
  entry, and provides clean uninstall. (No code signing yet — expect a SmartScreen
  prompt on first run; revisit signing if/when it becomes a goal.)

---

## 14. v1 scope vs. deferred

**In v1:** resident daemon + login auto-start, stub launcher, **foreground
activation via `AllowSetForegroundWindow` handoff (§4.1)**, **device-loss
detection and rebuild (§5.1)**, winit+wgpu render with channel/alpha/gamma/
exposure shaders, async worker decode, zune+image+exr+psd_sdk decoders, ICC
honoring via lcms2, tonemap-to-SDR HDR with exposure, downscale-to-fit,
reuse-window default with new-window/tabs config, TOML + egui settings, pixel
inspector, **egui toolbar (channel/background/exposure/fit) and status bar (file
name, format, resolution, bit depth, color space, size, zoom, eyedropper
readout)**, open-in-editor + clipboard, association-only Explorer integration,
unsigned installer.

**Deferred to v2:** Explorer `IThumbnailProvider` thumbnail handler, true HDR
monitor output, tiled/virtual texturing for gigapixel images, faster JPEG via
profiling-driven tuning, code signing.

---

## 15. Key risks & notes

- **Stub must stay trivial.** If the launcher ever grows a heavy dependency, its
  own startup reintroduces the cold-start you designed the daemon to avoid. Keep
  it: connect → send → exit (with spawn-and-retry fallback).
- **Foreground lock (§4.1).** A background daemon can't raise its own window; the
  stub must hand it foreground rights via `AllowSetForegroundWindow` or the
  "instant" open silently fails to come to front. Easiest thing to forget; most
  visible when wrong.
- **Device loss over long residency (§5.1).** Driver updates, TDR, sleep/resume,
  and laptop GPU switches will eventually lose the device without crashing the
  process; without explicit rebuild, opens go black silently.
- **psd_sdk is C++.** Budget time for the `-sys` crate (bindgen + `cc`) and treat
  every FFI call as a panic/abort boundary (`catch_unwind`, validated inputs).
- **ICC + zune tension.** Honoring profiles forces some formats off the zune hot
  path onto the `image` decoder that exposes ICC bytes; verify which formats this
  affects so you know where the fast path actually applies.
- **Resident RAM.** Always-warm means steady memory use; if it ever matters, the
  lifecycle can be made configurable (idle-exit) without architectural change.
- **First-run UX.** Unsigned installer → SmartScreen warning; document the
  "More info → Run anyway" step for users until signing is added.
