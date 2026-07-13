# Fire — Architecture

A Windows source-format image viewer optimized for *time-to-first-pixel* when
double-clicking a file in Explorer. Every design choice below traces back to one goal:
the image should be on screen as close to instantly as possible.

Fire is a **single, self-contained native Win32 application** that renders on the GPU via a
lean **Direct3D 11** device created when the window opens (no warm-up). There is no resident
background process and no separate launcher stub — the GPU device is cheap enough to create
on launch, so nothing needs to be kept warm.

---

## 1. Core insight

The dominant cost of "double-click → pixels on screen" is **process cold-start plus
decode**, not draw. A 2K PNG decodes in single-digit milliseconds; the headline cost is
getting a process to `main()` and a window on screen. A cold launch of a small native exe is
cheap enough that **no resident process is needed** to feel instant — so Fire keeps nothing
warm and creates everything it needs on the launch path. Decode (≈392 ms for an 8192×4096
PNG via zune) dominates time-to-first-pixel and is the project's primary metric; everything
else is kept off the critical path to the first pixel.

Two consequences shape the whole design:

- **No residency.** There is no background process and no launcher stub. The thing Explorer
  launches is the whole app; it lives exactly as long as it has a window open.
- **Non-resident GPU presentation.** Shading every surface pixel on the CPU would re-run the
  whole per-pixel pipeline on *every* pan/zoom event; on a large window at a high refresh rate
  (a 240 Hz monitor) that pegs a CPU core during fast interaction. Instead the image is
  uploaded **once** as a D3D11 texture with a hardware-generated mip chain, and pan / zoom /
  exposure / channel / tonemap (and flipbook cell selection) become a **128-byte constant
  buffer** — each frame is one
  fullscreen-triangle draw that re-samples the texture (**~0 CPU per frame**). A **DXGI
  flip-model swapchain** paces presentation to vsync, so interaction is tear-free and smooth at
  the monitor's true refresh. The device is created **when the window opens**, not warmed ahead
  of time, so it adds no residency cost; the GPU path is a few hundred lines of typed COM
  against the `windows` crate.

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
  pipe lives only inside the running window's process — nothing stays resident.

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
  on the window's HWND. The decoded image is uploaded **once** as a GPU texture; a
  short HLSL vertex+pixel shader (precompiled to DXBC at build time by `fxc` and embedded in the
  exe — no runtime `D3DCompile`) does the sampling and the whole color pipeline. The device is
  created lazily at window open — hardware preferred, with
  the **WARP** software rasterizer as a fallback for RDP/headless — so there is no warm-up step.
- **One window.** The swapchain covers the whole client and the image is drawn into a **sub-rect**
  of it, with the chrome (Dear ImGui — §5.2) drawn over the remainder *into the same backbuffer*.
  There used to be a frame/child-view split, because GDI and a flip-model swapchain cannot paint the
  same surface; with the chrome on the GPU that reason is gone, and with it `WS_CLIPCHILDREN`, the
  second window class, and the second wndproc. `App::image_rect` is the single definition of the
  image region; it is recomputed every frame and pushed into `GpuSurface::set_image_rect`, so there
  is no retained layout to invalidate. `RSSetViewports` maps the fullscreen triangle onto that
  sub-rect and clips to it, so the shader still fills the image region — background, checkerboard,
  letterbox and all — in **one draw**.
- **The image is a texture, not a per-frame computation.** On adopt, the decoded pixels are
  uploaded to a `USAGE_DEFAULT` texture created with a full mip chain
  (`D3D11_RESOURCE_MISC_GENERATE_MIPS`); `GenerateMips` builds the pyramid on the GPU. Each of the
  four `PixelFormat`s maps to a native DXGI format (see §5.1). After that, pan / zoom / exposure /
  channel / tonemap (and the flipbook cell offsets + blend) are just values in a **128-byte
  constant buffer**; the source texture never changes until a new image is opened (flipbook
  playback only moves the cell offsets — never re-uploads).
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

### 5.2 The UI pass (Dear ImGui)

The chrome is drawn by **Dear ImGui 1.92** into the *same backbuffer*, between the image draw and
the `Present`:

```
WM_PAINT →  clear the whole backbuffer to the chrome fill   (UNORM view)
            image pass  : viewport = image sub-rect, one fullscreen triangle   (SRGB view)
            UI pass     : ImGui_ImplDX11_RenderDrawData, whole client          (UNORM view)
            Present(1)  — vsync-paced
```

**Two render-target views of the same pixels, deliberately.** The image shader emits linear light and
writes through the `*_SRGB` view (above). ImGui's colors are *already* sRGB, so it must write through
a plain `UNORM` view — pushing it through the sRGB view would encode twice and visibly wash the
entire UI out. Both views are legal on the one backbuffer precisely because that backbuffer is plain
`R8G8B8A8_UNORM`. `GpuSurface::begin_frame` leaves the UNORM view bound for exactly this reason;
getting it wrong does not crash, it just looks bad.

**We own no backend code.** `dear-imgui-sys`'s `backend-shim-win32` / `backend-shim-dx11` features
compile ocornut's own `imgui_impl_win32.cpp` / `imgui_impl_dx11.cpp` and expose them over a
~10-function C ABI (`render/imgui.rs` declares them and nothing more). That is the whole reason this
dependency was acceptable: the platform/renderer glue — historically the part that rots — is
upstream's problem. There is no maintained Rust D3D11 backend, and writing one would have recreated
the very "constantly patching small issues" problem the migration existed to end.

**Rendering stays event-driven** — the invariant most at risk here, since ImGui's natural mode is to
redraw forever. A frame is drawn only when something happened; `App::request_frames(2)` asks for the
one or two extra frames ImGui needs to settle a hover or a click, and the count *terminates* — at
zero, no further `WM_PAINT` is requested. No input, no timer, no message → no frame. Measured, not
assumed: **0.16% of one core idle with the chrome up**, which is the file watcher, not ImGui.

**Cost, measured** (release, median of 12 launches, from the kernel's process-creation time so the
loader is included): time-to-first-pixel **+2.8 ms** on a 38 KB image — the unfair case, where decode
is instant so ImGui init has nothing to hide behind — and **+0.3 ms (noise)** on a real 8.9 MB one,
where the ~4.5 ms of ImGui init runs on the UI thread while the decode is still going and vanishes
into its shadow. The exe grows ~1.34 MB. Of that 4.5 ms, ~2.7 ms is D3D11 device-object creation and
under 1 ms is the first frame *including* rasterizing every glyph it draws — the fonts are not the
cost.

---

## 6. Decode pipeline

All decoders live behind a single **`fire-decode`** crate exposing a uniform
"bytes → (pixels, format, bit depth, optional ICC profile)" interface. Routing is by magic
bytes, with one exception: **camera raw is routed by file extension** (`decode`'s
`ext_hint`), because the many TIFF-structured raws (NEF/ARW/DNG/ORF/…) share TIFF's magic
and can't be told apart from a plain `.tif` by header alone. The few raws with a unique
signature (CR2's `CR\x02` marker, CR3's `crx ` ISOBMFF brand, RAF's ASCII magic, X3F) are
also detected by magic so a no-extension open still routes correctly.

| Format(s) | Decoder |
|---|---|
| JPEG, BMP, QOI, PPM, WebP, farbfeld, JXL | **zune** — hot path |
| GIF | `image` crate — **all frames** (animated GIF plays; still GIF is a single frame) |
| PNG | `image` crate → RGBA8/RGBA16 (+ICC). Deliberately **not** zune: the `png`+`fdeflate` stack measured ~1.8× faster than zune-png on large textures (the gap is in the core decode) |
| Radiance HDR (`.hdr`/`.pic`) | `image` crate → 32-bit float RGBA. Deliberately **not** zune: zune-hdr ≤ 0.5.2 wraps RGBE exponents ≥ 32 stops from unity (dark pixels decode 2³² too bright), and the `image` decoder is ~2× faster besides |
| TIFF, TGA, ICO | `image` crate (formats zune doesn't decode) |
| AVIF, HEIF, HEIC | **libheif** (+ libde265 / dav1d) over FFI → 8/16-bit RGBA (+ICC) |
| EXR | `exr` crate (pure Rust) → 32-bit float RGBA |
| PSD | **`psd_sdk`** (Molecular Matters, C++) over FFI → merged composite |
| Camera raw (CR2/CR3, NEF, ARW, RAF, ORF, RW2, DNG, …) | **`raw`** (pure Rust) → extract the embedded JPEG **preview**, decode via zune |
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
- **Camera raw = embedded preview, not develop.** A raw file is a per-vendor container
  around the sensor mosaic plus a full-size, camera-rendered **JPEG preview**. Developing
  the mosaic (demosaic + white balance + color matrices) is slow and at odds with the
  time-to-first-pixel goal, so `raw` instead extracts the largest embedded JPEG and decodes
  *that* through the zune path (ICC/downscale/etc. come for free). It locates the preview by
  walking the TIFF/EXIF IFD tree (or the RAF header / a whole-file JPEG-marker scan for
  non-TIFF containers like CR3), validates each candidate by probing its JPEG Start-Of-Frame
  for the largest dimensions, and applies the file's EXIF orientation so portrait shots are
  upright. All parsing is pure-Rust and bounds-checked (malformed → "no preview", never a
  panic). The displayed pixels are therefore 8-bit (the camera's rendering); full raw
  development is explicitly out of scope (a separate opt-in mode if ever wanted, §14).
- **Animated GIF:** GIF is routed to the `image` crate (by its `GIF8` magic), which decodes
  **every** frame — each already composited to a full RGBA8 canvas with GIF disposal handled — plus
  each frame's display delay. A multi-frame GIF comes back with a `DecodedImage::animation`
  (`Some(Animation)`); a single-frame GIF is an ordinary still (`None`), so the still path is
  untouched. Frame 0 is duplicated into `DecodedImage::pixels` so first-paint / downscale / alpha
  scanning work unchanged. Per-frame delays below 20 ms (including the common 0 = "as fast as
  possible") are clamped to 100 ms, matching browsers. The viewer plays it back with a UI-thread
  timer (§10). Animated WebP is *not* animated (WebP stays on the still zune hot path).
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

## 8. UI chrome (DPI + dark mode)

The UI is **Dear ImGui**, drawn on the GPU into the same backbuffer as the image (§5.2). It used to
be hand-painted GDI — chosen because the Win32 common controls have no documented dark mode, which
is true but led somewhere worse: we ended up owning *layout, scrolling, tab bars, text input, hover,
focus and hit-testing*, and every one of those produced bugs (a scrollbar that didn't drag, a focus
ring wiped by `EN_KILLFOCUS`). ImGui is not "more native" — it is themeable, not native — but those
are solved, tested primitives now, so that class of defect cannot occur. `ui/` is pure immediate-mode
code with no Win32 in it; it reads a `ViewSnapshot` and returns a `ui::Frame` of what the user asked
for, which the win shell applies.

- **Toolbar:** channel isolation (R/G/B/A/RGB), fit/1:1, zoom, flipbook, HDR tonemap + exposure
  (float sources only), and a right-docked group (outline, backdrop, full-screen, menu). Buttons
  dispatch the same `Action`s the keybinds drive — one state path. When the window is too narrow the
  left group sheds its lowest-priority slots into a "»" popup. There is **no gear**: Settings is the
  last entry of the menu button's popup, which is the same menu the viewport's right-click opens — one
  place to look, not two. That menu therefore stays enabled with no image loaded (its file entries
  hide themselves), or Settings would be unreachable from an empty window.
- **Status bar:** file name, format, W×H, bit depth / channel layout, ICC presence, and on the right
  the folder position and zoom % (plus `EV ±` for HDR).
- **Popup menus** (`ui::MenuState`): the *actions* menu (right-click on the image, or the "Open in…"
  toolbar button) and the *overflow* menu behind "»". Both are ImGui popups.

  They were `TrackPopupMenu` — and that one choice dragged in everything else: a `CreatePopupMenu` /
  `AppendMenuW` / `DestroyMenu` rebuild on every show, a command-id numbering scheme (`OPEN_WITH_ID_BASE
  + pre-order index`) to map a returned id back to the app to launch, a `PostMessage` deferral because
  the menu pumps its own modal loop and must never open from inside `WM_PAINT`, and — because a Win32
  menu is *system-drawn*, frame and gutter and all — **three undocumented `uxtheme.dll` ordinals**
  (`AllowDarkModeForWindow` / `SetPreferredAppMode` / `FlushMenuThemes`, 133/135/136) resolved by
  `GetProcAddress` and `transmute`d, purely to make it dark. All of that is gone. The menu is drawn in
  the frame we were already painting; a clicked "Open in…" entry names itself by its **index path**
  into the configured tree (`config::entry_at`), so there is no second walk to keep in step and no way
  for the menu and the launcher to disagree; and the app now calls **no undocumented API at all**.
- **Input routing:** ImGui sees every message first, then two booleans decide who owns it —
  `want_capture_mouse` (the pointer is over a widget) and `want_capture_keyboard` (a text field has
  focus, so keys are typing, not commands). That *replaces* the entire hand-rolled
  hover/capture/hit-test/focus layer. Note this is **not** the wnd-proc handler's return value:
  upstream returns true only for the few messages it fully consumes, never for "that click was mine"
  — gating on it would let a click on a toolbar button also pan the image underneath. One exception:
  a pan/zoom drag already in flight keeps the mouse to the end of the gesture even if the cursor
  strays over the chrome (`GpuSurface::is_mouse_captured`), or the drag would stick on crossing it.

  Keys need three cases ImGui's booleans don't cover, and each is a bug if you skip it. A **keybind
  capture** takes every key *before* ImGui sees it, Esc included (ImGui would read Esc as "close the
  modal" instead of "cancel the capture"). The **settings window** is modal, so keys are its, not the
  viewer's — but `want_capture_keyboard` can't express that, because ImGui sets it `true` for the whole
  time *any* modal is open (`ActiveId != 0 || modal_window != NULL`); `want_text_input` is the one that
  means "a text box has focus". And an open **popup menu** is *not* modal, so ImGui leaves the flag
  false and every key falls straight through — Esc would close the window out from under the menu.
- **DPI awareness:** `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` is declared before any
  window exists, so the non-client area auto-scales and `WM_DPICHANGED` fires on monitor moves. On a
  DPI change we adopt the OS-suggested rect, rescale the style, and re-raster the icon atlas — and
  that is *all*: ImGui 1.92's dynamic font system rasterizes glyphs on first use, so **there is no
  font atlas to rebuild**. (Do not build one, either: caching it would mean serializing ImGui's
  internal glyph structures, and it would save ~1 ms — the fonts are not the cost, the D3D11 device
  objects are.) Note `font_scale_dpi` scales *glyphs only*: every other metric is in logical px and
  is scaled in `ui::theme::apply`, or the chrome stays 96-dpi-sized on a HiDPI monitor.
- **Dark mode:** the system preference is read from the registry (`AppsUseLightTheme`); the title bar
  is darkened via `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`; the ImGui style and the
  letterbox backdrop come from the light/dark token sets in the **stylesheet** (`ui/theme.toml`,
  below), whose `accent` is the user's **system accent** (`GetSysColor(COLOR_HIGHLIGHT)` —
  documented, no registry poking; `chrome::system_highlight`). `WM_SETTINGCHANGE` /
  `WM_DWMCOLORIZATIONCOLORCHANGED` re-skin live; the restyle is unconditional, because the accent can
  move without the light/dark mode changing.
- **The stylesheet (`crates/fire/src/ui/theme.toml`):** every color, metric and spacing value the UI
  draws with, in one commented file — the two styles (chrome and settings form), both palettes, the
  bar heights, the paddings and roundings. Colors are a small grammar (`#hex`, `none`, `accent`, a
  token name, `lift(X, a)`, `alpha(X, a)`, `contrast(X)`), so *derived* colors — a hover state, a
  readable tick on an accent of unknown brightness — stay in the data rather than the code.
  `ui::theme` parses it, resolves it against the mode's tokens and the live accent, and applies it;
  the token → `StyleColor` mapping is the only styling decision left in Rust. **Release builds embed
  it** (`include_str!`) and never touch the disk; **debug builds** load it from the source tree and
  `hotstyle.rs` watches it — save the file and the running window restyles (`WM_APP_THEME_RELOADED` →
  `App::restyle`: metrics, both styles, the icon atlas, the clear color, repaint). A stylesheet is
  installed only once it parses *and* every color in it resolves, so a typo prints and changes
  nothing rather than putting a broken window on screen.
- **Icons:** `build.rs` still rasterizes the SVGs to A8 coverage masks; they are now packed into one
  RGBA8 **atlas strip** (white RGB, coverage in alpha) uploaded as a single D3D11 texture. ImGui's
  shader multiplies texel by vertex color, so `(1,1,1,a) * tint` gives any tint from one texture —
  no per-tint CPU work, which is what the old GDI path did on every repaint.

---

## 9. Window / session model and configuration

- **Session model:** a window holds a current image, view state (zoom, pan, channel
  toggles, exposure, tonemap), and a folder cursor for ←/→ navigation across siblings. The
  cursor (`folder.rs`) is built off-thread: opening a file scans its directory for sibling
  images on a background thread that posts the sorted list back (`WM_APP_FOLDER_SCANNED`), so
  the image shows first and the count fills in after (lazy). It is a snapshot taken at open
  time, generation-tagged for stale-drop like a decode, and re-scanned only on a fresh open.
- **Instance mode:** NewWindow (default) or SingleInstance, per §3.
- **Window placement:** the frame opens at the size/position it had when last closed — the
  restored (non-maximized) rect plus a maximized flag are captured on `WM_DESTROY` with
  `GetWindowPlacement` and persisted to `%APPDATA%\fire\window.toml` (see `window_state.rs`),
  then re-applied next launch with `SetWindowPlacement` (workspace coordinates round-trip
  exactly). The window is **never** resized to the image — every open lands in fit-to-window
  mode (`set_image` fits to the current viewport). The launcher's "Run" setting (the shortcut's
  Normal/Minimized/Maximized, read from `STARTUPINFO.wShowWindow`) overrides the show state:
  an explicit Maximized/Minimized wins, otherwise the remembered maximized state is restored.
- **Settings:** stored as **TOML** in `%APPDATA%\fire\config.toml`, editable directly *and* from the
  in-app settings window (`crate::ui::settings`) — a tabbed ImGui `BeginPopupModal` (General /
  Flipbook / Keybinds / Context menu) with OK/Cancel/Apply.

  It is drawn **inside the frame we were already painting**, which is the whole difference from the
  2,150-line hand-painted Win32 dialog it replaced: no second HWND, no nested `GetMessageW` pump, and
  therefore none of the `&mut App` aliasing that pump forced (the old dialog had to edit a *cloned*
  `Config` and post it back). The state simply lives in `App`, is edited during the paint, and the
  shell applies what the frame returns. Scrolling, the tab bar, text input, focus and hit-testing are
  ImGui's, not ours.

  **It has its own style** (`ui::theme::form`), and that is a decision rather than an omission. The
  chrome's style (`ui::theme::apply`) is a *toolbar*: buttons transparent until touched, no field
  frames, tight spacing — because it sits over an image and must not compete with it. A dialog that
  inherited it has invisible buttons and inputs whose edges you cannot see. So the settings window
  starts from ImGui's *factory geometry* (`render::imgui::FormStyle` snapshots the style at context
  creation, before `ui::theme` overwrites it — the only moment it exists) and `theme::form` paints
  fire's own palette and the user's accent onto it. Same colours as the chrome, form shape. Two
  ImGui-default behaviours are corrected on the way: `WindowBg`/`PopupBg` are ~94 % opaque (right for a
  debug overlay on a 3D scene, wrong here — the viewport's empty-state hint ghosted through), and the
  tab bar fills the *unselected* tabs while leaving the selected one to blend into the page, which
  reads as "this tab is disabled and those are buttons".

  **Its layout has no pixel constants.** It opens at a fraction of the viewport and is resizable from
  there; the footer is pinned to the bottom by giving the tab content a *negative-height* `BeginChild`
  (`[0, -footer]`), so the settings scroll above OK/Cancel/Apply instead of the scrollbar running past
  them; and each control's width is `content_region_avail − (the tab's longest label, measured in the
  live font)`, which both stretches the controls to the window and aligns every label into one column,
  from the same number. Labels are drawn on the **left**, with the widget given a hidden `##id` —
  ImGui's native order puts a widget's label *after* it, which reads as "New window ▼ Opening an
  image" and strands the labels in a ragged right-hand column. Nothing here to re-tune for a font, a
  DPI, or a resize.

  Two things it cannot do itself, and reports to the shell instead: **"Browse…"** (`GetOpenFileNameW`
  pumps its own modal loop, so it is posted as `WM_APP_SETTINGS_BROWSE` and runs after the paint — §5.2)
  and **keybind capture** (a chord is a virtual-key code, which only the wndproc sees; while a row is
  armed the shell routes every key to it, Esc included, or ImGui would read Esc as "close the modal").
  **Esc/Enter are the shell's too** — ImGui does not close a modal on Escape, and a dialog you cannot
  escape is a trap.

  Changes apply live where that isn't hostile (watcher, backdrop, zoom/exposure steps, keybinds, menu
  contents), on the next image where re-fitting under the user would be (open-fit, tonemap, flipbook
  playback defaults), and on the next launch for `instance-mode`. *Not yet:* hot-reloading
  `config.toml` when it changes on disk (only the displayed image is watched — §10).
- **Accent color:** the highlight throughout the **chrome** (pressed toolbar buttons, the latched
  channel/backdrop keys) is the user's **system accent**, read via `GetSysColor(COLOR_HIGHLIGHT)` —
  which Windows 10/11 set from the accent, so no undocumented API or registry read is needed. The
  text drawn *on* it flips to black for a pale accent. Re-read on theme/accent change. The settings
  window is outside this: it is stock ImGui, blue and all.
- **Future:** a third mode — compare two images side-by-side in one window, or tabs — is
  anticipated; it reuses the frame/child-view split (one view child per slot).

---

## 10. Viewer features

- Channel isolation (solo R/G/B/A, alpha-as-grayscale).
- Pan / zoom / fit / 1:1; LMB drag-pan (the image can be pushed fully off any edge — Fit/1:1
  recenters it); mouse-wheel and RMB-vertical-drag zoom, both about the cursor.
- HDR exposure (stops) + tonemap operator (Reinhard / ACES).
- **Animated GIF playback:** an animated GIF plays automatically at its authored per-frame delays.
  The decode delivers all frames (§6); the frame window runs a Win32 timer (`ANIM_TIMER_ID`),
  rescheduled each tick to the next frame's delay, whose handler advances `GpuSurface`'s frame index,
  uploads that frame as the texture, and invalidates the view. The timer is (re)armed on every adopt
  and stopped when a still image or a failed load takes over (`App::sync_animation`), so it follows
  ←/→ navigation and hot-reload and never outlives the animated image. Playback is pan/zoom/channel/
  exposure-agnostic (those still just change the constant buffer). Only GIF is animated for now.
- Drag-and-drop open: both the frame and the view child register with `DragAcceptFiles`, and
  each wndproc routes `WM_DROPFILES` through the same `App::open` path as a launch/forward
  (registering both is required because `WS_CLIPCHILDREN` gives the view its own client rect, so
  a drop over the image would otherwise miss the frame). The first dropped file is opened.
- **Pixel inspector** (planned): eyedropper RGBA readout + a zoomed pixel grid at high
  magnification, custom-painted into the view child with GDI `TextOut`/`DrawText` (system
  font — no rasterizer dependency), reading `current_image` via `view.screen_to_image()`.
- Folder navigation: ←/→ walk the sibling images in the current file's directory (wrapping at
  both ends), in file-manager natural order (case-insensitive, digit-runs by value so `img2`
  precedes `img10`); the status bar shows the position/count (`3 / 27`).
- Hot-reload: the displayed image re-decodes automatically when its file changes on disk
  (`watcher.rs`, on by default; `hot-reload = false` disables it). A per-window thread watches the
  current image's *directory* non-recursively via the `notify` crate (`ReadDirectoryChangesW`),
  which survives editors' atomic save-and-rename; it debounces write bursts and gates on a
  modified-time/size change (so a pure metadata touch — or the viewer's own decode read — can't
  trigger a reload loop), then posts `WM_APP_FILE_CHANGED` to the frame. The reload keeps the
  current pixels on screen until the new decode lands (no blank flash) and preserves the view
  (zoom/pan/channel/exposure) when the new image has the same dimensions, only re-fitting if the
  dimensions changed. The watch follows ←/→ navigation (it re-targets on every open/load) and is
  generation-tagged for stale-drop, exactly like decodes and folder scans.
- Planned: clipboard (`arboard`); "open in configured editor"; configurable background and
  alpha checkerboard.

---

## 11. Explorer integration

- **Association only** (no thumbnail handler in v1): register an `HKCU` ProgID, declare
  supported extensions (`.jpg .jpeg .png .tga .tif .tiff .psd .exr .hdr` …, plus camera-raw
  `.cr2 .cr3 .nef .arw .raf .orf .rw2 .dng` … under an opt-in `assoc\raw` task), and point
  them at `fire.exe`. Appears in "Open with".
- Decoders are factored into the standalone `fire-decode` crate, so an `IThumbnailProvider`
  handler can be added later as a separate `cdylib` reusing that core with no rework.

---

## 12. Workspace layout

```
fire/
├─ crates/
│  ├─ fire/           # the viewer exe: Win32 shell, D3D11 render, decode pool, pipe
│  ├─ fire-decode/    # uniform decode core (zune/image/exr/psd_sdk/libheif/lcms2)
│  ├─ fire-ipc/       # pipe wire format for single-instance forwarding (shared)
│  ├─ psd-sdk-sys/    # FFI bindings + cc build of psd_sdk
│  └─ heif-sys/       # FFI bindings + cc/link of libheif (AVIF/HEIF/HEIC)
├─ assets/            # fire.ico + icon source
└─ Cargo.toml         # workspace
```

Key dependencies: `windows` (typed COM for the D3D11 device + DXGI flip swapchain),
`windows-sys` (Win32: window/message loop, GDI, DWM, DPI, pipe, mutex, registry),
`zune-image`/`image`/`exr`/`lcms2`/`psd_sdk`/`libheif` (decode), `serde`/`toml`/`notify`
(config), `arboard` (clipboard), `crossbeam-channel` (worker messaging).

---

## 13. Build and distribution

- `cargo build --release` produces a **single `fire.exe`**. It links only the D3D11/DXGI system
  DLLs (present on every supported Windows; no redistributable, no bundled runtime — and with the
  shader precompiled at build time, not even `d3dcompiler`). The viewport HLSL is compiled to DXBC
  by `fxc` (Windows SDK) in `build.rs` and embedded via `include_bytes!`. The C++ `psd_sdk` and the
  `libheif`/`libde265`/`dav1d` decoder stack are built/linked via `cc`/`bindgen` build scripts in
  `psd-sdk-sys` and `heif-sys`. The Fire `.ico` + version/product metadata are embedded via
  `winresource`.
- **`product.json` (repo root) is the single source of product metadata** — name, version,
  publisher, copyright, homepage, description. `fire`'s `build.rs` reads it to fill the exe's
  version resource and to re-export the values as `FIRE_*` compile-time env vars the app reads
  (window title, etc.); the installer build script reads the same file. Bump the version there and
  it flows into the application and the installer alike — nothing else hardcodes it.
- **Unsigned installer** (Inno Setup, `installer/fire.iss`): per-user install (no admin, to match
  the `HKCU` association model), with a wizard page offering Fire as the default viewer per format
  plus an "All supported image formats" master toggle (default off — never steals associations the
  user didn't pick). Registers the shared `Fire.Image` ProgID + `OpenWithProgids` + a
  Default-Programs `Capabilities` block, with clean uninstall. No `Run`/autostart entry — nothing
  stays resident. (No code signing yet — expect a SmartScreen prompt on first run. Note: Windows
  protects the per-extension default via a hashed `UserChoice`, so the installer can claim *unset*
  types outright but cannot silently override a type the user has already assigned.)
- **Build the installer** with `scripts/build-installer.ps1`: it syncs the Cargo workspace version
  to `product.json`, regenerates `assets/fire.ico` from `assets/icon.png` (ImageMagick), builds the
  release exe, writes `installer/product.generated.iss` (the `#define`s from `product.json`), and
  compiles `installer/fire.iss` with ISCC into `dist/Fire-<version>-Setup.exe`.

---

## 14. v1 scope vs. deferred

**In v1:** single self-contained native Win32 exe; configurable NewWindow / SingleInstance
lifecycle with **foreground activation on the forward path (§4.1)**; GPU (D3D11) shader
render with channel/alpha/gamma/exposure/tonemap; async worker decode; zune + image + exr +
psd_sdk + libheif decoders; camera-raw embedded-preview decode; ICC honoring via lcms2;
tonemap-to-SDR HDR with exposure; downscale-to-fit RAM guard; **DPI-aware, dark-mode-aware
ImGui toolbar + status bar + settings window**; open-in-editor + clipboard; association-only
Explorer integration; unsigned installer.

**In progress / deferred:** pixel inspector, background-color *picker* (the settings window ships
the four preset backdrops; a custom color needs a `Params`/shader change), exposure trackbar;
compare/tabs mode; Explorer `IThumbnailProvider`; **full raw development** (demosaic the sensor
mosaic instead of showing the embedded preview — a separate opt-in mode, kept off the fast path);
code signing.

---

## 15. Key risks and notes

- **Cold start must stay cheap.** The whole bet is that a lean native exe reaches first-pixel
  fast. Creating the D3D11 device + flip swapchain happens on the launch path; it is cheap
  (low-ms) but real, so keep it lean and off the critical path to the first decode where
  possible. If a heavy dependency creeps back in, that cold-start cost reappears.
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
