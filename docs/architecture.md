# Fire - Architecture

A source-format image viewer for **Windows and macOS**, optimized for *time-to-first-pixel* when
double-clicking a file in Explorer or Finder. Every design choice below traces back to one goal:
the image should be on screen as close to instantly as possible.

Fire is a **single, self-contained native application** with one shared shell on both operating
systems: `winit` owns the window, the event loop and input; `sokol_gfx` owns the GPU behind one
drawing API, on a device and swapchain the shell creates itself (Direct3D 11 on Windows, Metal on
macOS). There is no resident background process and no separate launcher stub - the GPU device is
brought up on its own thread while the window is being created, so nothing needs to be kept warm.

This shape is the result of a port from a Windows-only Win32 + Direct3D 11 shell. The decisions
that produced it are recorded in [Appendix A](#appendix-a---the-decision-record) as **D1-D25**, and
cited by number from the sections below wherever a choice needs its reasoning; the two shells that
were built, measured and rejected on the way are in
[Appendix B](#appendix-b---how-the-shell-was-chosen).

---

## 1. Core insight

The dominant cost of "double-click → pixels on screen" is **process cold-start plus
decode**, not draw. A 2K PNG decodes in single-digit milliseconds; the headline cost is
getting a process to `main()` and a window on screen. A cold launch of a small native binary is
cheap enough that **no resident process is needed** to feel instant - so Fire keeps nothing
warm and creates everything it needs on the launch path, in parallel. Decode dominates
time-to-first-pixel and is the project's primary metric; everything else is kept off the critical
path to the first pixel. (PNG itself is decoded via the `image` crate, not zune's own PNG path -
see §6.)

Three consequences shape the whole design:

- **No residency.** There is no background process and no launcher stub. The thing the file
  manager launches is the whole app; it lives exactly as long as it has a window open.
- **Nothing on the launch path waits for anything it does not need.** `main` starts the GPU
  bring-up on its own thread on its first line (D18) - device creation is the longest single item
  and needs no window - then submits the launch path's decode *before* the event loop, the window
  or the GPU exist. The window is created alongside the bring-up thread and joins it only when it
  has something to draw into. On macOS this hides the entire 34 ms Metal device behind window
  creation: the measured join wait is **0.01 ms**.
- **Non-resident GPU presentation.** Shading every surface pixel on the CPU would re-run the
  whole per-pixel pipeline on *every* pan/zoom event; on a large window at a high refresh rate
  (a 240 Hz monitor) that pegs a CPU core during fast interaction. Instead the image is
  uploaded **once** as a GPU texture with a full mip chain, and pan / zoom / exposure / channel /
  tonemap (and flipbook cell selection) become a **128-byte uniform block** - each frame is one
  fullscreen-triangle draw that re-samples the texture (**~0 CPU per frame**). The swapchain paces
  presentation to vsync, so interaction is tear-free and smooth at the monitor's true refresh.

**Measured.** The Windows figures below are the migration gate (D2): `scripts/ttfp.ps1`, kernel
process creation → first image-bearing present, 12 interleaved launches per cell, release, idle
machine.

| Image | Win32 + D3D11 (the old shell) | winit + sokol_gfx | Δ | Budget |
| --- | --- | --- | --- | --- |
| 38 KB PNG | 133.5 / 132.4 ms | 135.4 / 131.6 ms | +1.9 / -0.8 ms | ≤ 5 ms |
| 8.9 MB PNG | 142.6 / 144.0 ms | 142.2 / 143.3 ms | -0.4 / -0.7 ms | ≤ 10 ms |

macOS has no cross-OS budget - the number to beat is the next mac build's (`scripts/ttfp.sh`,
8 interleaved launches per cell): **170.8 ms median** on a 130 KB JPEG, **224.5 ms** on a 27.5 MB
4096² PNG. The warm launch-path breakdown there is ~11 ms process start → `main` (the loader,
before a line of ours runs), ~75 ms of AppKit `finishLaunching` + activation inside `run_app`,
~31 ms window creation, 0.3 / 2.2 ms swapchain / ImGui, and the remainder in the first frame and
its vsync. The two largest items are AppKit's and the window's, not ours.

---

## 2. High-level architecture

```
Explorer double-click                    Finder / Dock / open(1)
  fire.exe "C:\path\img.png"             Apple Event → application:openURLs:
        │                                          │
        ▼                                          ▼
┌───────────────────────────────────────────────────────────────────┐
│  fire — one process                                               │
│                                                                   │
│  main(): GPU bring-up thread starts here                          │
│          read config → submit the launch decode                   │
│          try to bind the instance socket                          │
│          owner: run the event loop     │ client: forward & exit   │
│                                                                   │
│  winit event loop (ControlFlow::Wait / WaitUntil)                 │
│   ├─ Window 1 ─ swapchain + Viewer (view, chrome, timers)         │
│   ├─ Window 2 ─ …                                                 │
│   └─ deadline heap → the next WaitUntil                           │
│                                                                   │
│  render:  one device + one sokol_gfx for the process              │
│           one swapchain per window (render/d3d11.rs | metal.rs)   │
│           image pass = one fullscreen triangle, precompiled       │
│           UI pass    = sokol_imgui into the same pass             │
│                                                                   │
│  decode worker pool  ──EventLoopProxy──▶ the event loop           │
│  fire-decode core (zune / image / exr / heif / psd_sdk / lcms2)   │
└───────────────────────────────────────────────────────────────────┘
```

The thing the file manager launches is the whole app. There is no warm-up to amortize, so there
is nothing to keep resident.

---

## 3. Process model and lifecycle

**One process, one event loop, N windows** (D5). Fire is always a single process: every launch
tries to bind the local instance socket, the first one wins and runs the loop, and every later one
forwards its path to the owner and exits. There is no mutex - **the bind *is* the lock**.

Where a forwarded open lands is a user setting (`open-in` in the config), applied by the owner:

- **`new-window` (default):** the open gets its own window in the same process.
- **`reuse-window`:** the open is swapped into the focused window (or, with none focused, the most
  recently created one).

This replaced the old `instance_mode` (NewWindow = a whole second process / SingleInstance = a
named mutex + pipe). The reason is macOS: Finder never launches a second process - it sends an
open-file event to the running app - so a per-launch process has no mac equivalent, and winit runs
N windows in one loop cleanly. Windows users get the same UX from one process. The trade accepted
with it (D7) is that crash isolation is now per-process rather than per-window: every FFI call
already runs under `catch_unwind` on a worker with validated inputs, and a viewer has no unsaved
state, but a true segfault in libheif/psd_sdk would close every window rather than one.

No autostart, no login residency. "Residency" is implicit: the process lives exactly as long as it
has a window open - closing the last window exits the loop.

**A stale socket is a real state, and it is repaired rather than tolerated.** Where the name lives
in an OS namespace (Windows named pipes, Linux abstract sockets) the kernel frees it when the owner
dies. macOS has no such namespace - `interprocess` would emulate one with a file in the shared
temp directory - so Fire takes the explicit-path branch there and puts the socket in the user's own
runtime directory (two people on one Mac were otherwise sharing `/tmp/fire.sock`). A socket *file*
outlives its owner, and ⌘Q is the common way that happens: AppKit's `terminate:` ends in `exit()`,
so `main` never returns and the listener is never dropped. Measured, that cost **168 ms → 2196 ms
on every launch after a normal quit**. Three fixes, all live:

- The owner - and only the owner, since a forwarding launch would be deleting someone else's -
  registers an `atexit` `unlink` of its socket (`ipc_server::unlink_on_exit`). That covers ⌘Q, a
  plain `main` return, and the TTFP stamp's own `exit(0)`.
- The connect retry budget splits by meaning. "The name is not there yet" (`NotFound` /
  `ERROR_PIPE_BUSY`) still gets the full 2 s, because the owner may be anywhere in its own startup.
  "The name is there and refuses" (`ConnectionRefused`) - a socket file whose owner is gone, which
  nothing will ever start answering - gets 150 ms, enough to cover the window between a live
  owner's `bind` and its `listen` and no more. `SIGKILL` recovery went **2196 ms → 320 ms**.
- A launch with *no path to forward* still connects, so it can fail. `forward(None)` used to
  return `Ok` without connecting, and `Ok` means "forwarded, now exit" - a double-clicked Fire
  flashed in the Dock and vanished, permanently, until the socket file was deleted by hand. On a
  proven-dead name `ipc_server::rebind_after_stale` unlinks it and `main` re-binds and serves, so
  the first launch after a crash repairs the state instead of running un-coordinated forever.

---

## 4. Opening a file from the OS

Two paths in, both ending at the same `Viewer::open`.

**Windows: the argument, then the socket.** A double-click passes the path as `argv[1]`. If this
launch is not the owner it forwards that path over the instance socket and exits.

- **Transport:** the `interprocess` crate maps one name onto a named pipe on Windows and a Unix
  socket elsewhere.
- **Framing:** length-prefixed messages (`u32` little-endian length + payload).
- **Payload:** protocol version + window-mode + activate flag + UTF-8 path. The wire format lives
  in the dependency-light `fire-ipc` crate (no serde) so the forward path stays cheap.
- A forwarding launch writes one message and disconnects; the serving thread turns it into an
  `AppEvent::Open` for the event loop and never touches a window or the renderer itself.

**macOS: an Apple Event, always.** Finder, `open(1)` and a drop on the Dock icon all go through
Launch Services, which delivers the file as an Apple Event: a fresh launch gets *no arguments at
all*, and an app that is already running gets *no new process*, so the instance socket never sees
it either. Without a hook, Fire would open blank from Finder and ignore every later open.

`openfiles.rs` adds `application:openURLs:` to winit's own delegate class at runtime
(`class_addMethod`) and re-sets the delegate on `NSApplication` so AppKit re-caches which methods
exist. That is the only one of the three available routes that leaves winit's dispatch intact:
replacing the delegate crashes winit's `ApplicationDelegate::get`, and registering our own
`NSAppleEventManager` handler would be replaced by the one `NSApplication` installs during
`finishLaunching`. A launch-by-open fires *before* winit reports `resumed`, so those opens are
held and handed to the first window as it is created - it comes up showing the file rather than
coming up blank and loading it a frame later. Everything after that goes to the loop as
`AppEvent::Open`, exactly like a forwarded launch.

### 4.1 Foreground activation (the one real trap - Windows)

A process that does not currently own the foreground **cannot** raise its own window: Windows
blocks `SetForegroundWindow` from it. When a later launch forwards a file, the already-running
instance would swap the image in but stay behind other windows - the "instant open" would feel
half-broken.

The fix uses the one process that *does* hold foreground rights at the moment of the double-click:
the forwarding launch, because Explorer started it. As it sends the open request it calls
`AllowSetForegroundWindow`, handing over its one-shot grant; the owner raises and focuses the
target window on receipt, before the grant lapses. It is a `cfg(windows)` leaf in `platform.rs`,
called from the forward path. macOS needs no equivalent - Launch Services activates the app.

---

## 5. Rendering pipeline (GPU)

- **Stack: `sokol_gfx`, on a device and swapchain the shell owns** (D19). sokol_gfx does not own a
  window: it is handed a device once at `sg_setup` (through `sg_environment`) and a render target
  per frame (through `sg_swapchain`). That is what keeps winit's window model *and* gives one
  drawing API above two graphics APIs. The per-OS glue is ~250 lines each and is the only
  GPU-API-specific code in the tree:
  - `render/d3d11.rs` - a D3D11 device (hardware preferred, **WARP** as a fallback for
    RDP/headless) and a **DXGI flip-model swapchain** (`DXGI_SWAP_EFFECT_FLIP_DISCARD`).
  - `render/metal.rs` - an `MTLDevice` and a `CAMetalLayer` hosted on winit's `NSView`.

  `render/mod.rs` aliases whichever module this build has as `backend`, so `gpu.rs` carries no
  `cfg`. They are twins, not a trait - the target set is closed, so an alias costs nothing at
  runtime - which makes the contract (`Device::{create, fill_environment}`,
  `Swapchain::{new, size, resize, set_scale_factor, acquire, present}`, `SWAPCHAIN_FORMAT`)
  something that must be kept in step **by hand**. Anything added to one must be added to the other.
- **One process, one device; one swapchain per window.** sokol_gfx is a process-wide singleton, so
  `sg_setup` runs once. The device is created on the bring-up thread and used from the main thread
  after the join (D3D11 devices are free-threaded, and only the main thread touches the immediate
  context). The `CAMetalLayer` is the exception: Core Animation and `NSView` are main-thread-only,
  so the *layer* is created on the main thread by `GpuSurface::new`.
- **One window, one pass.** The swapchain covers the whole client area; the image is drawn into a
  **sub-rect** of it and the chrome (Dear ImGui - §5.2) over the remainder, *into the same pass*.
  `Viewer::image_rect` is the single definition of the image region; it is recomputed every frame
  and pushed into the surface, so there is no retained layout to invalidate. `apply_viewportf` +
  `apply_scissor_rectf` map the fullscreen triangle onto that sub-rect and clip to it, so the
  shader still fills the image region - background, checkerboard, letterbox and all - in **one
  draw**.
- **The image is a texture, not a per-frame computation.** On adopt, the decoded pixels are
  uploaded as an immutable image carrying **every mip level in one `sg_make_image`**. sokol_gfx has
  no `GenerateMips` and its rules forbid rendering into an image created with data, so the chain is
  built on the CPU on the decode worker (`render/mips.rs`, D21) - ~5 ms on an 8.9 MB image, off the
  UI thread. Each level is a 2×2 box filter of the level above, with 8-bit sRGB sources averaged in
  *linear* light through two lookup tables (what hardware does for an `*_SRGB` format); rows are
  split across threads for the big levels. After the upload, pan / zoom / exposure / channel /
  tonemap (and the flipbook cell offsets + blend) are just values in a **128-byte uniform block**;
  the source texture never changes until a new image is opened (flipbook playback only moves the
  cell offsets - never re-uploads).
- **Per-frame work is one draw.** A frame applies the pipeline and bindings, uploads `Params` with
  `sg_apply_uniforms`, and issues a single **fullscreen-triangle** draw; the fragment shader
  inverse-maps each output pixel into image space and samples the texture. There is no vertex
  buffer and no CPU per-pixel work - pan/zoom change a transform, not pixels, so interaction cost
  is independent of image resolution and of zoom-out factor.
- **Sampling:** a **point** sampler when magnifying (crisp 1:1 texels) and an **anisotropic +
  mipmapped** sampler when minifying, selected per frame. Hardware anisotropy + the mip chain
  replace the old CPU-built prefiltered pyramid and on-the-fly box average entirely, and give
  better anti-aliasing at no per-frame CPU cost.
- **Presentation is vsync-paced, and the wait is reported.** `render_frame` returns
  `Presented::{Skipped, Yes { waited }}`. Where that wait happens differs by backend and each
  measures its own: D3D11 blocks inside `Present(1, 0)`, Metal blocks earlier, in `nextDrawable`
  at acquire. On Metal, `sg_end_pass` calls `presentDrawable:` and `sg_commit` commits, so
  `Swapchain::present` only releases the drawable - presenting again there would be a double
  present. The `waited` verdict matters because playback is paced on it: DXGI answers
  `DXGI_STATUS_OCCLUDED` *immediately* when the window is hidden or fully covered, and pacing on a
  present that no longer blocks would spin.
- **Rendering is event-driven** - `request_redraw` only after input, a decode landing, or a timer.
  An idle window with an image open measured **0.0 ms of CPU over 5 s**. See §5.2 and §10 for why
  that invariant survives an immediate-mode UI and an animation timer.
- **Resize and device loss.** A resize drops the render-target view and calls `ResizeBuffers`; a
  zero dimension (a minimized window) is remembered but not applied - DXGI refuses it - and the
  frame is skipped. A device-removed reset shows up as a failed `GetBuffer` /
  `CreateRenderTargetView`, which skips the frame rather than drawing into nothing. Relaunch
  remains the recovery story (§15).

### 5.1 Per-pixel color pipeline

The source format determines how the texture is uploaded and how it is decoded to linear. Where a
device lacks the format a source needs, the pixels are converted on the way in rather than the
pipeline being changed:

| `PixelFormat` | sokol_gfx texture format | → linear |
|---|---|---|
| `Rgba8Unorm` | `Srgb8a8` | hardware sRGB-decode on sample |
| `Rgba16Unorm` | `Rgba16` (or `Rgba16f` where 16-bit norm sampling is unsupported) | shader sRGB→linear |
| `Rgba16Float` | `Rgba16f` | already linear |
| `Rgba32Float` | `Rgba32f` (or `Rgba16f` where float32 filtering is unsupported) | already linear |

Common tail, in shader order: outline / letterbox test (which return early on the gutter) → sample
(point/aniso per §5) → **HDR only** (float formats): exposure `×2^stops`, then tonemap (Reinhard
default / ACES toggle) → channel selection: solo R/G/B/A as grayscale, `RGB` as the color values on
their own, and `RGBA` composited over the backdrop (the transparency checkerboard is linear
0.45/0.21) → the octagon overlay's "hide outside" fade toward the backdrop → sRGB encode on the way
out.

**The swapchain is a plain UNORM target and the shader sRGB-encodes its own output** (D20). Flip-
model swapchains disallow `*_SRGB` formats, and Dear ImGui's colors are *already* sRGB, so one
UNORM target is correct for both passes and the old two-render-target-view trick is gone. The
shader owns the encode and must not be "fixed" into a linear write. The *format* is per-OS -
`R8G8B8A8_UNORM` on D3D11, `BGRA8Unorm` on Metal, because a `CAMetalLayer` does not accept RGBA8 -
so `SWAPCHAIN_FORMAT` lives in the backend module. That is a storage channel *order* difference
only: the shader still writes `float4` RGBA and Metal swizzles on the way out.

The backdrop/letterbox clear color is the theme-aware chrome color, unpacked from its `0x00RRGGBB`
value and sRGB-decoded to linear on the CPU so it matches.

### 5.2 The UI pass (Dear ImGui)

The chrome is drawn by **Dear ImGui 1.92** into the *same pass*, between the image draw and
`sg_end_pass`:

```
RedrawRequested → swapchain.acquire()          (None → skip the frame)
                  sg_begin_pass                clear to the chrome fill
                  image pass : viewport = the image sub-rect, one triangle,
                               128-byte uniform block via sg_apply_uniforms
                  UI pass    : simgui_render() into the same pass
                  sg_end_pass, sg_commit
                  present                      vsync-paced
```

**We own no backend code** (D3). Input comes through `dear-imgui-winit`, the maintained winit
backend of the same crate family as `dear-imgui-rs`, released in step with it. Drawing is
`sokol_imgui.h` in its `SOKOL_IMGUI_NO_SOKOL_APP` mode - a renderer only, compiled by `build.rs`
from `crates/fire/simgui/` - which uploads ImGui's textures (fonts included, through the 1.92
texture protocol) as sokol_gfx images and draws the draw lists into the current pass. It is the
same header on every OS, maintained next to sokol_gfx by its author. That was the whole reason the
dependency was acceptable: the platform/renderer glue - historically the part that rots - is
upstream's problem.

**One ImGui context per window, and only one is ever current.** Dear ImGui has a single current
context; `dear-imgui-rs` models that as an active `Context` or a `SuspendedContext`. Every window's
context lives suspended and each operation activates it for exactly the duration of a closure
(`Imgui::with`), so N windows in one process never race over the global and there is no ordering
rule for the caller to remember. `simgui_setup` runs once per process and every call of its goes
through `igGetIO()`, i.e. whatever is current, so it draws whichever window's context is active.

**Rendering stays event-driven** - the invariant most at risk here, since ImGui's natural mode is
to redraw forever. A frame is drawn only when something happened; the viewer asks for the one or
two extra frames ImGui needs to settle a hover or a click, and that count *terminates*. No input,
no timer, no event → no frame.

**The frame closes even if the UI panics.** A panic mid-frame would leave the context between
`NewFrame` and `Render` and the next frame would assert; it would also leave sokol_gfx mid-pass.
Both the ImGui build closure and the UI callback inside the pass run under `catch_unwind` -
whatever was built is rendered, the panic is logged, and the app carries on.

**Cost, measured** on the Win32+D3D11 shell when ImGui replaced hand-painted GDI (release, median
of 12 launches, from the kernel's process-creation time so the loader is included):
time-to-first-pixel **+2.8 ms** on a 38 KB image - the unfair case, where decode is instant so
ImGui init has nothing to hide behind - and **+0.3 ms (noise)** on a real 8.9 MB one. The current
shell's ImGui init is 1.7 ms on Windows and 2.2 ms on macOS.

### 5.3 The shader, and where its bytecode comes from

`render/shader.glsl` is **the one source**: sokol-shdc's annotated GLSL (Vulkan syntax, separate
texture and sampler objects). `scripts/gen-shaders.sh` turns it into everything else, all of it
checked in under `render/generated/`:

- `shader_viewport_hlsl5_{vertex,fragment}.hlsl` and `..._metal_macos_{vertex,fragment}.metal` -
  the per-backend sources, which **`build.rs` compiles to bytecode**: `fxc` → `.dxbc` on Windows,
  `xcrun metal` + `metallib` → one `.metallib` *per stage* on macOS (per stage because
  SPIRV-Cross names every entry point `main0`, and two functions cannot share a library).
- `shader.rs` - the sokol_gfx reflection: the 128-byte uniform block, the texture, the two
  samplers, which sampler pairs with the texture, and the per-backend entry-point names. All that
  is left in `render::gpu::make_shader` is swapping the generated desc's `source` for `build.rs`'s
  `bytecode`.

So a plain `cargo build` never needs `sokol-shdc`, there is **no runtime shader compile on either
OS** (D4/D24 - the wgpu branch lost ~32 ms to exactly this, and TTFP is the primary metric), and a
broken shader is a build error. `gpu.rs` asserts `size_of::<Params>()` equals the *generated*
uniform block's size, so an edit that changes the block fails the build rather than producing a
wrong-looking image. The generated HLSL's `packoffset`s and its `b0`/`t0`/`s0`/`s1` registers came
out byte-identical to the hand-written cbuffer that preceded them, and neither backend inserts a
Y-flip: `gl_FragCoord` maps to `SV_Position` and `[[position]]`, both top-left origin, which is
what the pixel math assumes.

**One rule the shader must keep:** never sample inside a per-pixel branch without explicit
derivatives. The letterbox and outline tests above the sampling *are* branches, and an
implicitly-derived LOD inside a branch is undefined where the quad diverges - which produced a
flickering 1 px line on all four image edges. Every tap is `textureLod` or `textureGrad`.

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
| JPEG, BMP, QOI, PPM, WebP, farbfeld, JXL | **zune** - hot path |
| GIF | `image` crate - **all frames** (animated GIF plays; still GIF is a single frame) |
| PNG | `image` crate → RGBA8/RGBA16 (+ICC). Deliberately **not** zune: the `png`+`fdeflate` stack measured ~1.8× faster than zune-png on large textures (the gap is in the core decode) |
| Radiance HDR (`.hdr`/`.pic`) | `image` crate → 32-bit float RGBA. Deliberately **not** zune: zune-hdr ≤ 0.5.2 wraps RGBE exponents ≥ 32 stops from unity (dark pixels decode 2³² too bright), and the `image` decoder is ~2× faster besides |
| TIFF | **`tiff` crate directly** → RGBA at the source depth (8/16/32f, +ICC). Going through `image` lost samples: it can only represent what `tiff`'s conservative `colortype()` names, so an unlabelled 4th sample (Photoshop's `ExtraSamples = 0`) was dropped, grey+alpha was refused outright, and 16-bit was narrowed to 8. Associated (premultiplied) alpha is straightened here. Palette/CMYK/YCbCr/Lab still fall back to `image` |
| TGA, ICO, CUR | `image` crate (formats zune doesn't decode). A `.cur` is an ICO with `2` in its type word and two directory fields reused for the hotspot, neither of which the ICO decoder reads - so it decodes unchanged, and is only sniffed and labelled separately |
| DDS | **`ddsfile`** (header, incl. the DX10 extension) + **`bcdec_rs`** (blocks), both pure Rust → BC1-BC7 to RGBA8, **BC6H to RGBA16F** (the HDR path), and every uncompressed layout via the header's channel bit masks. Decompressed on the CPU rather than uploaded as blocks: `DecodedImage` is an uncompressed canvas by contract, and the mip builder, downscale guard, alpha scan and flipbook detector all read it as one |
| AVIF, HEIF, HEIC | **libheif** (+ libde265 / dav1d) over FFI → 8/16-bit RGBA (+ICC) |
| EXR | `exr` crate (pure Rust) → 32-bit float RGBA |
| PSD | **`psd_sdk`** (Molecular Matters, C++) over FFI → merged composite, at the document's own depth (8/16/32f). `wrapper.cpp` owns the colour-mode conversion: RGB/Grey/Duotone direct, Indexed through the palette, CMYK composited **through K** (PSD stores CMYK inverted), Lab via D50 XYZ. 16-bit samples are Photoshop's 15-bit+1 range (**0…32768**, not 0…65535); 1-bit Bitmap mode is refused, since psd_sdk sizes its planes `bits/8` = 0 |
| Camera raw (CR2/CR3, NEF, ARW, RAF, ORF, RW2, DNG, …) | **`raw`** (pure Rust) → extract the embedded JPEG **preview**, decode via zune |
| ICC transforms | **Little CMS** (`lcms2`) over FFI |

`SUPPORTED_EXTENSIONS` in `fire-decode` is *the* table of what Fire opens - 61 extensions. The
Windows installer keeps a second copy because an Inno Setup script can import nothing, and a test
(`installer_associations_match_the_extension_table`) polices the two against each other; the macOS `Info.plist`
needs no such test because `scripts/build-mac.sh` parses the const itself.

Notes:
- **Decode speed is the project's primary metric.** The common formats run through zune
  with `DecoderOptions::new_fast` (platform intrinsics + unsafe fast paths). Output is
  normalized to interleaved RGBA in the source bit depth (8/16/float).
- **ICC fallback:** zune does not reliably surface embedded ICC for every format. When a
  profile must be honored, the file is routed through the `image`/format-specific decoder
  that exposes the profile, then transformed with `lcms2`.
- **FFI safety:** every C/C++ boundary (`psd_sdk`, `lcms2`) is wrapped in `catch_unwind`
  and runs on a decode worker, so a malformed file cannot take down the viewer process.
  (This is also why `panic = "abort"` is *not* set in the release profile - `catch_unwind` only
  works with unwinding panics.)
- **The mip chain is built here too** (§5): on the decode worker, right after the decode and
  before the image is posted, so the UI thread never pays for it.
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
  **every** frame - each already composited to a full RGBA8 canvas with GIF disposal handled - plus
  each frame's display delay. A multi-frame GIF comes back with a `DecodedImage::animation`
  (`Some(Animation)`); a single-frame GIF is an ordinary still (`None`), so the still path is
  untouched. Frame 0 is duplicated into `DecodedImage::pixels` so first-paint / downscale / alpha
  scanning work unchanged. Per-frame delays below 20 ms (including the common 0 = "as fast as
  possible") are clamped to 100 ms, matching browsers. The viewer plays it back on a loop timer
  (§10). Animated WebP is *not* animated (WebP stays on the still zune hot path).
- **DDS = the first surface only, for now.** A DDS is a container: it can hold a mip chain, six
  cubemap faces, an array of layers, or a volume's depth slices, and `dds.rs` locates all of them
  (`Surfaces`) but currently decodes mip 0 of layer 0. Signed block formats are re-centred for
  display (`bcdec` returns the raw -127..127 range as bytes, which would show a signed normal map
  as noise), and `DXT2`/`DXT4` - and any DX10 header that says so - have their premultiplied alpha
  straightened, the same fixup the TIFF path applies.
- **Oversized images:** `DecodeOptions::max_dim` is a **CPU/RAM guard**, not a GPU texture
  limit (an RGBA8 bitmap at 16384² is ~1 GiB; float HDR is 4×). It defaults to 16384, is
  configurable, and anything past it is CPU-downscaled to fit, recording the original size
  so the pixel inspector can note that a read came from the downscaled copy. Decode itself
  raises zune's internal guard well past this so large sources reach the downscale pass
  rather than being rejected outright. (Tiled/virtual texturing deferred to v2.)

The pool itself (`decode_pool.rs`) is shared by every window in the process. Each job carries a
process-wide monotonic `generation` plus the window it was issued for; a result is adopted only if
it is still that window's latest generation, so a slow decode can never clobber a newer one. A
superseded job is still decoded - its result is just dropped on arrival - which wastes a little
work and keeps the pool dead simple. Workers never touch a window or the renderer; they send an
`AppEvent` through the event loop's proxy, the same discipline the folder scan, the file watcher
and the instance-socket server thread follow.

---

## 7. Color management

- **Working space:** sRGB for 8/16-bit LDR; linear for float/half HDR.
- **ICC honored:** embedded profiles (PNG `iCCP`, JPEG APP2, TIFF tag, PSD resource) are
  parsed and transformed into the working space via `lcms2`. Files without a profile fall
  back to the sRGB assumption.
- **HDR display:** tonemap to SDR in the shader with an exposure-stops control (works on any
  monitor). The float source is sampled and tonemapped live each frame, so exposure/operator
  changes are free. A true HDR (scRGB / 10-bit, or EDR on macOS) swapchain is *possible* on both
  backends - deferred; current output is tonemap-to-SDR.

---

## 8. UI chrome (DPI + dark mode)

The UI is **Dear ImGui**, drawn on the GPU into the same pass as the image (§5.2). It used to be
hand-painted GDI - chosen because the Win32 common controls have no documented dark mode, which is
true but led somewhere worse: we ended up owning *layout, scrolling, tab bars, text input, hover,
focus and hit-testing*, and every one of those produced bugs (a scrollbar that didn't drag, a focus
ring wiped by `EN_KILLFOCUS`). ImGui is not "more native" - it is themeable, not native - but those
are solved, tested primitives now, so that class of defect cannot occur. `ui/` is pure
immediate-mode code with no window system and no GPU API in it; it reads a `ViewSnapshot` and
returns a `ui::Frame` of what the user asked for, which the viewer applies.

- **Toolbar:** channel isolation (R/G/B/A/RGB), fit/1:1, zoom, flipbook, HDR tonemap + exposure
  (float sources only), and a right-docked group (outline, octagon, backdrop, full-screen, menu).
  Buttons dispatch the same `Action`s the keybinds drive - one state path. When the window is too
  narrow the left group sheds its lowest-priority slots into a "»" popup. There is **no gear**:
  Settings is the last entry of the menu button's popup, which is the same menu the viewport's
  right-click opens - one place to look, not two. That menu therefore stays enabled with no image
  loaded (its file entries hide themselves), or Settings would be unreachable from an empty window.
- **Status bar:** file name, format, W×H, bit depth / channel layout, ICC presence, and on the right
  the folder position and zoom % (plus `EV ±` for HDR).
- **Empty window:** a centred card with the logo, the product identity (long name + version, from
  `product.json` via `build.rs`) and the drop/open hint. It degrades gracefully - the logo and then
  the identity block drop out - when there is not enough room.
- **Popup menus** (`ui::MenuState`): the *actions* menu (right-click on the image, or the "Open
  in…" toolbar button) and the *overflow* menu behind "»". Both are ImGui popups.

  They were `TrackPopupMenu` - and that one choice dragged in everything else: a `CreatePopupMenu`
  / `AppendMenuW` / `DestroyMenu` rebuild on every show, a command-id numbering scheme to map a
  returned id back to the app to launch, a `PostMessage` deferral because the menu pumps its own
  modal loop, and - because a Win32 menu is *system-drawn* - **three undocumented `uxtheme.dll`
  ordinals** resolved by `GetProcAddress` and `transmute`d, purely to make it dark. All of that is
  gone. The menu is drawn in the frame we were already painting; a clicked "Open in…" entry names
  itself by its **index path** into the configured tree (`config::entry_at`), so the menu and the
  launcher cannot disagree; and the app now calls **no undocumented API at all**.
- **Input routing** (`Viewer::window_event`, three layers, and the order is the point): first the
  lifecycle events (resize, DPI, theme, drop, close) that nothing else may intercept; then the
  *ownership* gates - an armed keybind row, ImGui, the modal settings window, an open popup - each
  there because the gate before it would otherwise swallow the event; only what survives reaches
  `on_mouse` / `on_key`.

  ImGui sees every event first, then two booleans decide who owns it: `want_capture_mouse` (the
  pointer is over a widget) and `want_text_input` (a text field has focus, so keys are typing, not
  commands). That *replaces* the entire hand-rolled hover/capture/hit-test/focus layer. One
  exception: a pan/zoom drag already in flight keeps the mouse to the end of the gesture even if the
  cursor strays over the chrome, or the drag would stick on crossing it.

  Keys need three cases ImGui's booleans don't cover, and each is a bug if you skip it. A **keybind
  capture** takes every key *before* ImGui sees it, Esc included (ImGui would read Esc as "close the
  modal" instead of "cancel the capture"). The **settings window** is modal, so keys are its, not
  the viewer's - but `want_capture_keyboard` can't express that, because ImGui sets it `true` for
  the whole time *any* modal is open; `want_text_input` is the one that means "a text box has
  focus". And an open **popup menu** is *not* modal, so ImGui leaves the flag false and every key
  would fall straight through - Esc would close the window out from under the menu.
- **Keybinds are physical and portable** (D9). A chord names a `KeyCode` - the key at the position
  `F` has on a US keyboard - not the character it types, so one `config.toml` means the same thing
  on every layout and every OS. The modifier is `Primary`: Ctrl on Windows, ⌘ on macOS, resolved at
  match time (`Ctrl+` and `Cmd+` are still accepted as spellings of it, so an older file reads).
  Chords match **exactly**, so a modifier held by accident no longer triggers the plain command;
  the one concession is `Shift+=`, bound alongside `=`, because that is how you type `+`. Only
  bindings that *differ* from the defaults are written back, so a user who never rebinds anything
  keeps an empty `[keybinds]` table and inherits future default changes.
- **DPI awareness:** winit reports the scale factor and its changes. On a DPI change the style is
  rescaled and the icon atlas is re-rastered - and that is *all*: ImGui 1.92's dynamic font system
  rasterizes glyphs on first use, so **there is no font atlas to rebuild**. (Do not build one,
  either: caching it would mean serializing ImGui's internal glyph structures, and it would save
  ~1 ms - the fonts are not the cost.) The platform backend runs with its own DPI handling **locked
  to 1.0**: the whole UI lays out in physical pixels (`ui::theme::Metrics` scales from the DPI
  itself), so ImGui's coordinate space must be the framebuffer's, not winit's logical one.
- **Dark mode:** the system preference comes from winit (`Window::theme()` / `ThemeChanged`) on
  both OSes - the registry read and the `DwmSetWindowAttribute` call are gone. That preference is
  the **only** theme input the app takes from the system, and all it decides is which of the
  stylesheet's two token blocks (`[colors.dark]` / `[colors.light]`) is in force; every color,
  accent included, is the stylesheet's. A theme change re-skins live.
- **Fonts:** the system UI font is read from the running machine (`segoeui.ttf` on Windows,
  `SFNS.ttf` on macOS); Fire bundles none, and falls back to ImGui's built-in font if it cannot be
  read.
- **The stylesheet (`crates/fire/src/ui/theme.toml`):** every color, metric and spacing value the UI
  draws with, in one commented file - the two styles (chrome and settings form), both palettes, the
  bar heights, the paddings and roundings. Colors are a small grammar (`#hex`, `none`, a token name,
  `lift(X, a)`, `alpha(X, a)`, `contrast(X)`), so *derived* colors - a hover state, a tick that stays
  readable on whatever accent is set - stay in the data rather than the code. `ui::theme` parses it,
  resolves it against the mode's tokens, and applies it; the token → `StyleColor` mapping is the only
  styling decision left in Rust. **Control sizes** (`[chrome.controls]` / `[form.controls]`) are the
  one thing that cannot be a style field: ImGui derives a checkbox, a tab, an input and a button all
  from `font size + 2 × frame_padding.y`, so sizing one without the others means pushing a
  `FramePadding` around that widget - `theme::push_control` does it, and every width or reserve the
  layout measures for that control is measured under the same push. **Release builds embed it**
  (`include_str!`) and never touch the disk; **debug builds** load it from the source tree and
  `hotstyle.rs` watches it - save the file and every open window restyles (`AppEvent::ThemeReloaded`
  → `Viewer::restyle`: metrics, both styles, the icon atlas, the clear color, repaint). A stylesheet
  is installed only once it parses *and* every color in it resolves, so a typo prints and changes
  nothing rather than putting a broken window on screen.
- **Icons:** `build.rs` rasterizes the SVGs to A8 coverage masks; they are packed at runtime into
  one RGBA8 **atlas strip** (white RGB, coverage in alpha) uploaded as a single sokol_gfx image.
  ImGui's shader multiplies texel by vertex color, so `(1,1,1,a) * tint` gives any tint from one
  texture - no per-tint CPU work, which is what the old GDI path did on every repaint. The
  stylesheet's `[icon_scale]` (a per-icon shrink, for artwork that doesn't fill its box like the
  rest) is **baked into the atlas**, not applied at the draw call: the master is rastered into a
  smaller box *centred in a full-size cell*, so the cell - and therefore the UV grid, the draw size
  and ImGui's derived button size - stays a fixed `icon_px`, and the shrunk icon is a true
  downsample rather than a re-scaled cell. `Imgui::refresh_icons` watches the scales as well as the
  size, since a hot reload can move one without the other. An `Icon` is a *cell*, not a drawing: two
  variants may name the same SVG (`B` and `BackdropBlack`) so that two buttons sharing artwork can
  still be sized apart.

---

## 9. Window / session model and configuration

- **Session model:** a window holds a current image, view state (zoom, pan, channel
  toggles, exposure, tonemap), and a folder cursor for ←/→ navigation across siblings. The
  cursor (`folder.rs`) is built off-thread: opening a file scans its directory for sibling
  images on a background thread that posts the sorted list back (`AppEvent::FolderScanned`), so
  the image shows first and the count fills in after. It is a snapshot taken at open
  time and re-scanned only on a fresh open.
- **Where an open lands:** `open-in = new-window | reuse-window`, per §3.
- **Window placement:** each window opens at the size/position it had when the last one closed -
  the restored (non-maximized) rect plus a maximized flag, captured on close and persisted to
  `window.toml` in the config directory (`window_state.rs`), then re-applied next launch. The
  window is **never** resized to the image - every open lands in fit-to-window mode. On Windows the
  launcher's "Run" setting (the shortcut's Normal/Minimized/Maximized, read from
  `STARTUPINFO.wShowWindow`) overrides the show state; off Windows that leaf answers `None` and the
  remembered state stands.
- **Settings:** stored as **TOML** in the per-user config directory - `%APPDATA%\fire` on Windows,
  `~/Library/Application Support/fire` on macOS, `$XDG_CONFIG_HOME/fire` elsewhere (one definition,
  `util::fire_dir`, so the files cannot drift apart). Fire writes a fully commented
  `config.toml` on first run and never rewrites it on its own; it is editable directly *and* from
  the in-app settings window (`crate::ui::settings`) - a tabbed ImGui `BeginPopupModal`
  (General / Flipbook / Keybinds / Context menu) with OK/Cancel/Apply. Settings are per-user rather
  than per-window: the window that applied them tells the shell, which hands the same config to
  every other open window (and nobody re-saves it).

  It is drawn **inside the frame we were already painting**, which is the whole difference from the
  2,150-line hand-painted Win32 dialog it replaced: no second window, no nested message pump, and
  therefore none of the `&mut` aliasing that pump forced. The state lives in the viewer, is edited
  during the paint, and the shell applies what the frame returns.

  **It has its own style** (`ui::theme::form`), and that is a decision rather than an omission. The
  chrome's style (`ui::theme::apply`) is a *toolbar*: buttons transparent until touched, no field
  frames, tight spacing - because it sits over an image and must not compete with it. A dialog that
  inherited it has invisible buttons and inputs whose edges you cannot see. So the settings window
  starts from ImGui's *factory geometry* (`render::imgui::FormStyle` snapshots the style at context
  creation, before `ui::theme` overwrites it - the only moment it exists) and `theme::form` paints
  the stylesheet's palette onto it. Same colours as the chrome, form shape. Two ImGui-default
  behaviours are corrected on the way: `WindowBg`/`PopupBg` are ~94 % opaque (right for a debug
  overlay on a 3D scene, wrong here - the viewport's empty-state card ghosted through), and the tab
  bar fills the *unselected* tabs while leaving the selected one to blend into the page, which
  reads as "this tab is disabled and those are buttons".

  **Its layout has no pixel constants.** It opens at a fraction of the viewport and is resizable from
  there; the footer is pinned to the bottom by giving the tab content a *negative-height* `BeginChild`
  (`[0, -footer]`), so the settings scroll above OK/Cancel/Apply instead of the scrollbar running past
  them; and each control's width is `content_region_avail − (the tab's longest label, measured in the
  live font)`, which both stretches the controls to the window and aligns every label into one column,
  from the same number. Labels are drawn on the **left**, with the widget given a hidden `##id` -
  ImGui's native order puts a widget's label *after* it, which reads as "New window ▼ Opening an
  image" and strands the labels in a ragged right-hand column. Nothing here to re-tune for a font, a
  DPI, or a resize.

  Two things it cannot do itself: **keybind capture** (a chord is a physical key code, which only the
  shell's event handler sees; while a row is armed the shell routes every key to it, Esc included)
  and **"Browse…"** (a native file dialog - see below). **Esc/Enter are the shell's too** - ImGui
  does not close a modal on Escape, and a dialog you cannot escape is a trap.

  Changes apply live where that isn't hostile (watcher, backdrop, zoom/exposure steps, zoom-snap
  levels, keybinds, menu contents), on the next image where re-fitting under the user would be
  (open-fit, tonemap, flipbook playback defaults), and on the next launch for `open-in`. *Not yet:*
  hot-reloading `config.toml` when it changes on disk (only the displayed image is watched - §10).
- **The octagon overlay's options** are a floating ImGui window over the viewport rather than a
  settings tab, because they are adjusted *while looking at the image*. Its persisted defaults live
  under `[octagon]` in the config, behind a `remember` flag that is off by default.
- **Accent color:** the highlight throughout the UI (latched toolbar buttons, checkmarks, the selected
  tab's rule) is the stylesheet's `accent` token - a color you set per mode in `ui/theme.toml`, not
  the OS accent. Anything drawn *on* it uses `contrast(accent)`, which picks black or white by
  luminance, so a pale accent doesn't produce white-on-yellow.

### 9.1 No modal loop inside an event-loop handler

**Nothing called from a winit callback may pump an event loop of its own.** This is a rule the
Win32 shell did not have, and breaking it is not a glitch - it aborts the process.

Found the hard way: the Open… picker crashed Fire on macOS, reliably, as soon as the mouse moved
over the panel. `rfd`'s `pick_file` puts up an app-modal `NSOpenPanel` and calls `runModal`, which
pumps its own loop. It was being called from the idle step - deliberately, from the Win32 days:
"never from inside a redraw". But **every** one of our handlers, the idle step included, runs
inside winit's dispatcher, which holds a `RefCell` borrow for the whole call. AppKit's modal loop
then routes a mouse event through winit's `sendEvent:` override, a gesture recognizer spins a
*third* loop, a run-loop block re-enters winit's `handle_event`, and it panics: *"tried to handle
event while another event is currently being handled"*. The panic unwinds into a CoreFoundation
callback, where unwinding is forbidden, so the process aborts - the panic firewall is nowhere near
that path and could not have caught it anyway. Windows has the same guard in its runner and so is
exposed to the same class of bug.

So **every native dialog runs on a worker thread and answers with an `AppEvent`**
(`app::viewer::Dialog` → `AppEvent::DialogDone`). `rfd` hands the panel to the main thread itself,
so the modal loop runs from the *run loop* rather than from inside our handler and re-entrancy
never arises. As a bonus the window behind the picker stays live: an image forwarded from a second
launch loads and redraws *while* the picker is up, which is exactly the dispatch that used to
abort. The same rule retired the last blocking call - the "could not open a window" message box is
recorded and shown by `main` after `run_app` returns.

### 9.2 Timers, and the event-driven invariant

Timers are **deadlines the loop sleeps on**, never threads (D8). `app::timers` is a `BinaryHeap` of
`(Instant, seq)` with a side table of `(WindowId, TimerKind)` - three kinds: animated-GIF playback,
the flipbook pump, and the text caret's blink. Arming pushes and returns a `seq`; the idle step
pops everything due, dispatches it to the owning window, and then sets
`ControlFlow::WaitUntil(next)` or `Wait` if the heap is empty. Cancellation is by sequence number
rather than removal - a popped entry whose `seq` the window no longer wants is simply dropped - so
a re-armed timer never has to find its predecessor in the heap.

That last step is the whole of the event-driven invariant's enforcement: with no timer armed the
process sleeps in the OS until an event arrives.

**Every handler runs behind a panic firewall.** A panic in a handler must not unwind into winit's
dispatcher - the Win32 wndproc had the same rule - so each entry point catches, logs, and carries
on with the window alive.

---

## 10. Viewer features

- Channel isolation (solo R/G/B/A, alpha-as-grayscale, RGB↔RGBA composite).
- Pan / zoom / fit / 1:1; LMB drag-pan (the image can be pushed fully off any edge - Fit/1:1
  recenters it); mouse-wheel and RMB-vertical-drag zoom, both about the cursor; and on macOS
  **pinch-to-zoom**, which maps `WindowEvent::PinchGesture`'s incremental magnification onto the
  same about-cursor zoom (D15). **1:1 means one texel per *physical* pixel**, so it stays crisp on
  a Retina display - fit and 1:1 are in physical pixels end to end, and it is the *gesture* math
  that has to convert.
- The wheel's job is configurable (`wheel-action`): zoom about the cursor, or step through the
  folder. Ctrl+wheel zooms either way, so choosing navigation does not cost you wheel zoom.
- **Zoom-snap detents** (`ZoomDetent`, `render/view.rs`): the RMB scrubby-zoom notches at the
  configured zoom levels (`zoom-snap-levels`) instead of sliding past them, which is what makes
  landing exactly on 100 % possible with a drag. Crossing a level pins the zoom there and absorbs
  the next `zoom-snap` worth of travel; past that the zoom resumes *from the snap*, so breaking out
  is continuous rather than a jump, and a held drag walks through snap after snap. A step that
  clears a level by more than the release distance - a flick - passes straight through, so the
  detents notch a deliberate drag without braking a fast one. All the math is in log-zoom units and
  window-system-free (unit-tested); an empty ladder or a non-positive release is snapping off.
- HDR exposure (stops) + tonemap operator (Reinhard / ACES).
- **Animated GIF playback:** an animated GIF plays automatically at its authored per-frame delays.
  The decode delivers all frames (§6); an `Anim` timer, rescheduled each tick to the next frame's
  delay, advances the surface's frame index, uploads that frame as the texture and requests a
  redraw. It is (re)armed on every adopt and stopped when a still image or a failed load takes over,
  so it follows ←/→ navigation and hot-reload and never outlives the animated image. Playback is
  pan/zoom/channel/exposure-agnostic (those still just change the uniform block). Only GIF is
  animated for now.
- **Flipbook (sprite-sheet) playback:** a still image laid out as a `cols × rows` grid of frames
  is played back as an animation without ever re-uploading the texture - the whole sheet stays one
  texture and playback only moves the uniform block's cell offsets + blend. The grid is
  **content-detected** off-thread on the decode worker (`flipbook::detect`, YIN period detection
  over luma/alpha - *the pixels decide the grid*, since filenames can be wrong or missing; a `_8x8`
  filename token is only a last-resort fallback), sent *after* the decode so the scan never delays
  the image, and surfaced as a dismissible hint that never enters the mode on its own. Once in
  flipbook mode a transport band (`crate::transport`, drawn in ImGui) exposes cols/rows/count, FPS,
  play/pause, scrub and cross-frame blend; a scrub drag pauses playback for its duration. Defaults
  and per-path overrides live in the Flipbook settings tab. Pure grid/frame math is in `flipbook.rs`
  (unit-tested, no window system or GPU).

  Its ~60 Hz timer neither paces the animation nor, normally, the frames: playback position is
  derived from elapsed time, so the sheet plays at its own `fps` whenever it is sampled, and while
  the window is visible each frame is asked for by the *previous* frame's present, which blocks
  until vblank. The timer **starts** the pump and **carries** it when nothing else would.
- **The octagon overlay:** Unity VFX Graph's octagon particle shape drawn over the image (or the
  current flipbook frame), so an artist can see what an octagon-cropped particle would keep of the
  texture. Eight vertices in two sets - four pinned at the quad's edge midpoints, four sliding
  diagonally inward from the corners with the crop factor - so on a square quad all eight sides
  stay the same length at every crop. The lines are an ImGui draw list, the "hide outside" fade is
  two uniform-block floats in the fragment shader, and the geometry itself is pure unit-tested math
  in `octagon.rs`. It tracks the image in full screen too.
- **Drag-and-drop open:** winit reports the drop; it goes through the same `Viewer::open` path as a
  launch or a forward. One client rect covers image and chrome alike, so there is no second surface
  to register.
- **Folder navigation:** ←/→ walk the sibling images in the current file's directory (wrapping at
  both ends), in file-manager natural order (case-insensitive, digit-runs by value so `img2`
  precedes `img10`); the status bar shows the position/count (`3 / 27`).
- **Hot-reload:** the displayed image re-decodes automatically when its file changes on disk
  (`watcher.rs`, on by default; `hot-reload = false` disables it). One long-lived thread per window
  owns an OS watch through the `notify` crate (`ReadDirectoryChangesW` on Windows, FSEvents on
  macOS) on the current image's *directory*, non-recursively, which survives editors' atomic
  save-and-rename; it debounces write bursts and gates on a modified-time/size change (so a pure
  metadata touch - or the viewer's own decode read - can't trigger a reload loop), then sends
  `AppEvent::FileChanged`. The reload keeps the current pixels on screen until the new decode lands
  (no blank flash) and preserves the view (zoom/pan/channel/exposure) when the new image has the
  same dimensions, only re-fitting if the dimensions changed. The watch follows ←/→ navigation and
  is generation-tagged for stale-drop, exactly like decodes and folder scans.
- **Full screen** (F11, or middle-click over the viewport): winit's borderless full screen, which
  on macOS is `toggleFullScreen:` - the native space transition. Esc always leaves full screen; the
  `esc-closes-window` setting governs whether it *also* closes an ordinary window.
- **File actions and Open in…:** "Show on Disk" (Explorer on Windows, Finder on macOS), copy the
  file, its path or its name, and any number of user-configured external programs (`[[open-with]]`, nestable into
  submenus, with `{path}` substituted into the arguments). Which built-in items appear is
  configurable (`[context-menu]`).

---

## 11. OS integration

**Windows - association only** (no thumbnail handler): the installer registers a per-format `HKCU`
ProgID (`Fire.png`, `Fire.tga`, …) whose friendly type name is what Explorer shows in the Type
column, an `OpenWithProgids` entry, the `.ext` default ProgID for formats the user ticked, and a
Default-Programs `Capabilities` block so Fire appears in Settings → Default apps. Uninstall removes
all of it. Windows protects the per-extension default with a hashed `UserChoice`, so the installer
can claim types with no choice set but cannot silently override one the user has already assigned.

**macOS - document types, volunteered not claimed.** The `.app`'s `Info.plist` declares one
`CFBundleDocumentTypes` entry listing every extension, with **`LSHandlerRank = Alternate`**: Fire
shows up in "Open With" and can be made the default, without taking `.png` away from Preview on
install. One entry rather than one per format, because with `Alternate` we do not own the UTI and
the per-type name never surfaces. The extension list is **parsed out of `fire-decode`'s
`SUPPORTED_EXTENSIONS`** by `scripts/build-mac.sh`, so unlike the installer's copy it cannot drift;
61 extensions in, and LaunchServices resolves them to 48 claimed UTIs (the real ones -
`public.png`, `com.ilm.openexr-image`, `com.adobe.photoshop-image`, every camera-raw UTI - plus
dynamic ones for formats macOS has no UTI for, like `.qoi`, `.ff`, `.x3f`). `NSSupportsSuddenTermination`
is pointedly *not* declared: it would let the OS skip the `atexit` that removes the instance socket
(§3).

Decoders are factored into the standalone `fire-decode` crate, so an `IThumbnailProvider` (or a
Quick Look extension) can be added later reusing that core with no rework.

---

## 12. Workspace layout

```
fire/
├─ crates/
│  ├─ fire/           # the viewer: winit shell, sokol_gfx render, decode pool, instance socket
│  │  ├─ src/app/     #   the ApplicationHandler (Fire) + one Viewer per window + the timer queue
│  │  ├─ src/render/  #   view math, gpu.rs, d3d11.rs | metal.rs, mips.rs, imgui.rs, the shader
│  │  ├─ src/ui/      #   pure immediate-mode UI + the stylesheet (theme.toml)
│  │  └─ simgui/      #   sokol_imgui.h + cimgui.h, compiled as C by build.rs
│  ├─ fire-decode/    # uniform decode core (zune/image/exr/psd_sdk/libheif/lcms2)
│  ├─ fire-ipc/       # the instance-socket wire format (shared, dependency-free)
│  ├─ psd-sdk-sys/    # FFI bindings + cc build of psd_sdk
│  └─ heif-sys/       # FFI bindings + link of libheif (AVIF/HEIF/HEIC), vendored per target
├─ vendor/sokol-rust/ # pinned upstream floooh/sokol-rust; excluded from the workspace
├─ assets/            # app icons + the toolbar SVGs
├─ installer/         # the Inno Setup script (Windows)
├─ scripts/           # packaging, shader generation, the TTFP harness
└─ Cargo.toml         # workspace
```

Key dependencies: `winit` (window, event loop, input, DPI, drag-and-drop, theme, full screen),
`sokol` (sokol_gfx, vendored), `dear-imgui-rs` + `dear-imgui-winit` + `sokol_imgui` (the chrome),
`interprocess` (the instance socket), `dirs` (the config directory), `rfd` (native dialogs and the
startup error box), `zune-image`/`image`/`tiff`/`exr`/`lcms2`/`psd_sdk`/`libheif` (decode),
`serde`/`toml`/`notify` (config + hot-reload), `crossbeam-channel` (worker messaging). Windows adds
`windows` (typed COM for the D3D11 device and DXGI swapchain) and `windows-sys` (the five platform
leaves); macOS adds `muda` (the menu bar) and the `objc2` family (the `CAMetalLayer`, the delegate
hook, the pasteboard) at winit's own versions, so only `muda` is an extra compile.

`winit` and the two `dear-imgui-*` crates are pinned to **exact** versions. They move together, and
`dear-imgui-sys`'s cimgui is what `simgui.c` is compiled against (§15), so a bump is a three-place
change: the crate versions, `simgui/cimgui.h`, and a rebuild proving the defines still match.

### 12.1 The platform leaves

Everything below is the platform-specific code that remains; `platform.rs` says in its header that
nothing *else* in `fire` should mention an OS, and this is what it points at.

| Concern | Windows | macOS | Shared via |
| --- | --- | --- | --- |
| Window, loop, DPI, DnD, theme change, full screen, placement | - | - | `winit` |
| GPU device + swapchain | `render/d3d11.rs`: D3D11 + DXGI flip-model swapchain | `render/metal.rs`: `MTLDevice` + `CAMetalLayer` on winit's view | `sokol_gfx` above them |
| Shader bytecode | HLSL → DXBC (`fxc`, build.rs) | MSL → `.metallib` (`xcrun metal`, build.rs) | one `sokol-shdc` source + reflection |
| Config dir | `%APPDATA%\fire` | `~/Library/Application Support/fire` | `dirs` |
| Dark mode | - | - | `winit` `Window::theme()` / `ThemeChanged` |
| Open-file dialog + startup error box | - | - | `rfd` |
| Hot-reload watch | `ReadDirectoryChangesW` | FSEvents | `notify` |
| IPC transport | named pipe | Unix socket file (runtime dir) | `interprocess` |
| Foreground handoff on forward | `AllowSetForegroundWindow` leaf | not needed | - |
| Launcher "Run" show state | `GetStartupInfoW` leaf | no equivalent (`None`) | `platform.rs` |
| Clipboard (Copy File / Path / Name) | `CF_HDROP` + text leaf | `NSPasteboard` file URL + text | `platform.rs` |
| Show in Explorer / Reveal in Finder | leaf | leaf | `platform.rs` |
| UI font | `segoeui.ttf` | `SFNS.ttf` | `platform.rs` |
| Open-file events from the OS | `argv[1]` | `openfiles.rs`: `application:openURLs:` | both call `Viewer::open` |
| Menu bar | none | `menubar.rs`: `muda`, App / File / Window | - |
| File association | `HKCU` ProgID (installer) | `CFBundleDocumentTypes`, `LSHandlerRank = Alternate` | both from one extension table |
| Native decoder libs | vendored `.lib` | vendored arm64 `.a` | one `VENDOR.txt` recipe |
| Icon / metadata | `winresource` | `Info.plist` + `.icns` | both from `product.json` |
| Packaging | Inno Setup | `build-mac.sh` → signed, notarized `.dmg` | `product.json` |

**The macOS menu bar** deserves a note. A Mac app without one reads as broken and, more concretely,
is awkward to quit - ⌘Q is the menu's, not the window's. `menubar.rs` builds the minimum that makes
Fire behave like a Mac app (application, File, Window) and routes the two items that are *Fire's*
back into the same `KeyAction` path a keystroke takes, so the menu and the keyboard cannot drift.
Everything else is a `muda` **predefined** item, which maps onto AppKit's own responder-chain
selectors (`terminate:`, `hide:`, `performMiniaturize:`) and so behaves exactly as users expect and
cannot desynchronise from app state. winit installs a default menu bar of its own during
`applicationDidFinishLaunching`, which would replace ours wholesale, so it is turned off
(`with_default_menu(false)`) - and whatever replaces it must therefore carry Quit itself.

Two rules there, both learned: **menu accelerators intercept keys before winit sees them**, so each
app item is given the chord the user has actually bound to that action rather than a hardcoded
⌘O/⌘W - a menu accelerator that disagreed with the keybind would silently shadow it. And there is
deliberately **no "Close Window" item**, tempting as the Mac convention is: muda's predefined one
hardcodes ⌘W, which is already Fire's *Close image* chord on both OSes, and two items claiming one
accelerator leaves AppKit to pick. The red button still closes the window.

---

## 13. Build and distribution

- `cargo build --release` produces a **single executable**. On Windows it links only system DLLs
  (D3D11/DXGI and friends - no redistributable, no bundled runtime, and with the shader
  precompiled, not even `d3dcompiler`); on macOS it links the system frameworks the vendored sokol
  tree names for itself (`Cocoa` / `QuartzCore` / `Metal`). The C++ `psd_sdk` and the
  `libheif`/`libde265`/`dav1d` decoder stack are built/linked via the `cc`/`bindgen` build scripts
  in `psd-sdk-sys` and `heif-sys`; `fire`'s own `build.rs` compiles the shader bytecode (§5.3),
  compiles `simgui.c`, rasterizes the toolbar SVGs, and on Windows embeds the `.ico` + version
  resource via `winresource`.
- **`product.json` (repo root) is the single source of product metadata** - name, version,
  publisher, copyright, homepage, description. `build.rs` reads it to fill the Windows version
  resource and to re-export the values as `FIRE_*` compile-time env vars the app reads (window
  title, the empty-window identity card); both packaging scripts read the same file. Bump the
  version there and it flows into the application and the packages alike.
- **Windows: an unsigned Inno Setup installer** (`installer/fire.iss`, built by
  `scripts/build-installer.ps1` - see [installer/README.md](../installer/README.md)). Per-user install
  (no admin, matching the `HKCU` association model), a wizard page offering Fire as the default
  viewer per format plus an "All supported image formats" master toggle (default off - never steals
  associations the user didn't pick), and clean uninstall. No `Run`/autostart entry - nothing stays
  resident. No code signing yet: expect a SmartScreen prompt on first run.
- **macOS: a signed, notarized `.dmg`** (`scripts/build-mac.sh`). It builds the release binary,
  makes the `.icns` from the 1024² master, writes the `Info.plist` from `product.json` and
  `fire-decode`'s extension table (§11), signs with `codesign --options runtime --timestamp`,
  submits to `notarytool --wait`, staples, and wraps the result in a `.dmg` with an `/Applications`
  symlink. It runs **by hand on the dev Mac** (D11) and takes both credentials from the keychain,
  so the Developer ID certificate and the App Store Connect key never have to exist as CI secrets
  on a public repo. `--no-notarize` / `--no-sign` step down from that for iteration and say plainly
  that what they produce is not shippable - macOS 15 removed the Control-click bypass, so an
  un-notarized build needs System Settings → Privacy & Security → Open Anyway.
- **CI** ([.github/workflows/ci.yml](../.github/workflows/ci.yml)) is a two-host matrix, and both legs
  are mandatory (D23): clippy on Windows never sees `render/metal.rs`, `openfiles.rs` or
  `menubar.rs`, and clippy on macOS never sees `render/d3d11.rs` or the `windows-sys` leaves, so a
  single-host CI cannot keep the workspace lint-clean. Each host runs two jobs: `check` with
  `--no-default-features` (no vendored native trees, so no bindgen and no LLVM needed - it covers
  `fire-ipc`, the pure-Rust decode core and the whole shell), and `full`, gated on a restored
  vendor cache, which adds the PSD/HEIF FFI tests and the release build. The vendor cache key
  carries the runner OS and arch, or the arm64 `.a`s and the x64 `.lib`s would collide. Per D11 CI
  stops at build-and-test on macOS: it never signs, notarizes or packages.

  The mac leg needs one thing the Windows leg does not: the **Metal toolchain**, which since
  Xcode 26 is an optional ~700 MB component present on some runner images and not others, and which
  `build.rs` needs for *any* build. The job tests for it by **running**
  `xcrun -sdk macosx metal --version`, because `xcrun --find metal` succeeds either way - what it
  finds without the toolchain is a stub that only fails when used - and downloads it only if that
  fails.

---

## 14. Scope vs. deferred

**Shipping:** a single self-contained native app on Windows and macOS; one process, N windows, with
`open-in` deciding where a forwarded open lands, and **foreground activation on the Windows forward
path (§4.1)**; GPU render through sokol_gfx with channel/alpha/gamma/exposure/tonemap; async worker
decode; zune + image + tiff + exr + psd_sdk + libheif decoders; camera-raw embedded-preview decode;
animated GIF playback; ICC honoring via lcms2; tonemap-to-SDR HDR with exposure; downscale-to-fit
RAM guard; content-detected **flipbook (sprite-sheet) playback** with a transport band; the
**octagon overlay**; folder ←/→ navigation; hot-reload of the displayed image; **DPI-aware,
dark-mode-aware ImGui toolbar + status bar + settings window**; portable physical-key keybinds;
open-in-editor and the clipboard actions; file association on both OSes; an unsigned Windows
installer and a signed, notarized macOS `.dmg`.

**In progress / deferred:** pixel inspector; a custom background-color *picker* (the settings ship
the four preset backdrops; a custom color needs a shader/uniform change); compare/tabs mode
(anticipated as extra image sub-rects and viewports within one window, not as child windows);
Explorer `IThumbnailProvider` / Quick Look; **full raw development** (demosaic the sensor mosaic
instead of showing the embedded preview - a separate opt-in mode, kept off the fast path);
hot-reload of `config.toml`; Windows code signing; an Intel/universal macOS build; tiled/virtual
texturing for gigapixel sources.

---

## 15. Key risks and notes

- **Cold start must stay cheap.** The whole bet is that a lean native binary reaches first-pixel
  fast. Device creation is the longest single item on the launch path; it is off the critical path
  today only because it runs on its own thread and the window is created *before* the join (D18).
  Get that ordering wrong and the window's 9-31 ms serialize after the device. If a heavy
  dependency creeps back in, the cold-start cost reappears.
- **No modal loop inside a handler** (§9.1). It is not a glitch, it is a process abort, and the
  panic firewall cannot catch it. Every native dialog goes to a worker thread.
- **`sokol_imgui` / `cimgui` ABI lockstep.** `simgui.c` and `dear-imgui-sys` compile the same ImGui
  structs in two translation units. The five cimgui defines and the `SOKOL_*` backend must match
  exactly; a mismatch is **not a link error, it is a silent layout difference**. Bump the
  `dear-imgui-*` crates and the vendored `simgui/cimgui.h` as one change.
- **The two backend modules are twins by hand.** `render/mod.rs` aliases one of them as `backend`;
  nothing enforces that the other keeps up. Anything added to one must be added to the other, and
  only CI's second host will notice if it is not.
- **The vendored sokol tree** (`vendor/sokol-rust`) is a pinned checkout, not a registry
  dependency; updating it is manual, and it is where `sg_swapchain` / `ShaderDesc` field changes
  would land.
- **GPU device loss - deliberately unhandled.** A device can be lost (TDR, driver update, GPU
  reset). The renderer does not recreate the device or swapchain, by design: this is a stateless
  viewer with no unsaved data, so the recovery story is "relaunch". A failed acquire skips the
  frame rather than drawing into nothing. WARP remains a fallback only at *creation* time (no
  hardware / RDP), not a mid-session failover.
- **Single-process crash exposure** (D7). Accepted knowingly: FFI already runs under `catch_unwind`
  on a worker with validated inputs, and a viewer has no unsaved state, but a true segfault in
  libheif/psd_sdk closes every window rather than one.
- **Foreground lock (§4.1).** Windows only, but the easiest thing to get wrong and the most visible
  when it is: without the `AllowSetForegroundWindow` handoff, a forwarded open silently fails to
  come to the front.
- **`psd_sdk` is C++.** Treat every FFI call as a panic boundary (`catch_unwind`, validated inputs)
  so a malformed file can't take down the process.
- **ICC + zune tension.** Honoring profiles forces some formats off the zune hot path onto the
  `image` decoder that exposes ICC bytes; verify which formats this affects so you know where the
  fast path actually applies.
- **Large-image RAM (+ VRAM).** The decoded image is retained in RAM (the upload source and the
  pixel-inspector backing) *and* lives as a GPU texture with a mip chain (~4/3× its size in VRAM);
  the chain is also built in RAM on the worker before upload. The `max_dim` guard bounds the worst
  case; revisit if gigapixel sources become common.
- **The dev and shipping macOS bundles share the instance socket**, because its name belongs to the
  product rather than to the bundle id. Launching one while the other runs forwards the open to
  whichever got there first - worth knowing while testing, not a bug.
- **First-run UX.** The Windows installer is unsigned → SmartScreen warning; document the "More
  info → Run anyway" step until signing is added. The macOS `.dmg` is signed *and* notarized, and
  the stapled ticket means it opens on a Mac that is offline.

---

## Appendix A - the decision record

The shared shell replaced a Windows-only Win32 + Direct3D 11 one. These are the decisions that
produced it, kept because each says what it *costs* as well as what it buys — which is what a
future revision needs in order to revisit one. They are cited by number from the sections above.

Two entries (D14, D17) belong to a branch that was measured and abandoned; they are kept as
numbered holes rather than renumbered, so a `D18` in this document means what it has always meant.

| # | Decision | Why | Status and what it cost |
| -- | -------- | --- | ----------------------- |
| D1 | **One shared shell — winit + sokol_gfx — on both OSes**, not AppKit+Metal beside Win32+D3D11 | Minimum platform code: the per-OS part becomes a device/swapchain module, not a whole renderer | **Shipped.** Windows was re-plumbed and its time-to-first-pixel re-measured (§1) |
| D2 | **Windows migrates too**, gated on the TTFP benchmark | Keeping a whole D3D11 renderer behind a trait *is* the two-shell maintenance the port existed to avoid | **Met.** Budget was ≤ 10 ms median regression on the 8.9 MB case and ≤ 5 ms on the 38 KB one; measured within noise (§1) |
| D3 | **`dear-imgui-rs` + `dear-imgui-winit` (input) + `sokol_imgui` (renderer)** replace `dear-imgui-sys` and the C++ win32/dx11 shims | No per-OS backend code of our own; the renderer is the same header sokol_gfx's author maintains beside it | **Shipped.** `ui/` moved from a raw 10-function ABI to the safe API, and `simgui.c` must compile against the same cimgui `dear-imgui-sys` links (D22) |
| D4 | **The shader is precompiled to bytecode on both OSes**, from one annotated-GLSL source through `sokol-shdc` (§5.3) | Nothing on the cold-start path on either OS, and a broken shader is a build error; one source beats two hand-kept-in-sync twins once a second backend exists | **Shipped.** Costs `fxc` + the Windows SDK on Windows and the Metal toolchain on macOS (D24). The HLSL half wants a Windows build to confirm after any shader change |
| D5 | **One process, N windows** everywhere; instance mode became `open-in = new-window \| reuse-window` | Finder never launches a second process — it sends an open-file event to the running app — so per-launch processes have no mac equivalent, and winit runs N windows in one loop cleanly | **Shipped.** Windows `NewWindow` users get the same UX from one process; crash isolation is now per-process (D7) |
| D6 | **IPC through the `interprocess` crate** — named pipe / Unix socket behind one API | One forward path, one test | **Shipped.** macOS also needed the Apple-Event hook (§4) feeding the same open path |
| D7 | **Accept single-process crash exposure** | FFI already runs under `catch_unwind` on a worker with validated inputs, and a viewer has no unsaved state | **Accepted.** A true segfault in libheif/psd_sdk closes every window, not one. If it bites, a decode subprocess is the fallback and `fire-decode`'s uniform interface makes that a bounded change |
| D8 | **Timers are `ControlFlow::WaitUntil` + a deadline min-heap** (§9.2) | Preserves the event-driven invariant — no input, no timer → no frame — with zero threads | **Shipped.** A small scheduler in the app; every timer (GIF, flipbook, caret) goes through it |
| D9 | **Keybinds are physical `KeyCode`s by name plus a `Primary` modifier** (Ctrl on Windows, ⌘ on macOS) | Layout-independent; one `config.toml` works on both | **Shipped.** One-time migration of the old VK-code chords; `Ctrl+` / `Cmd+` still parse as `Primary+` |
| D10 | **macOS is Apple Silicon only**, with vendored arm64 static libs for libheif/libde265/dav1d and a `cc`-built psd_sdk | Same vendoring model as the Windows `.lib`s; no Intel users to serve | **Shipped** (see D25). A universal binary is deferred, and `build-mac.sh` checks the architecture rather than pretending |
| D11 | **Build, sign and notarize only on the dev Mac**, via `scripts/build-mac.sh`; CI never builds a mac artifact | Keeps the Developer ID certificate and the App Store Connect key off CI entirely, on a public repo | **Shipped.** Releases are a manual step rather than CI-triggered; both credentials come from the keychain |
| D12 | **Packaging is `build-mac.sh` + an `Info.plist` from `product.json`**, into a signed and notarized `.dmg` | Mirrors `build-installer.ps1`; the plist is hand-tuned anyway, which `cargo-bundle` would have hidden | **Shipped.** Hand-writing it paid twice: the document types are parsed out of `fire-decode`'s extension table so they cannot drift (§11), and `NSSupportsSuddenTermination` is pointedly *not* declared — it would let the OS skip the `atexit` that removes the instance socket (§3) |
| D13 | **Windows first, then macOS** | The shared code and the whole TTFP risk *were* the Windows migration; mac is leaves plus packaging | **Done** |
| D14 | *(belongs to the wgpu branch: a pinned backend, no debug layers, decode started before device creation)* | — | **Superseded** (Appendix B.2). The one surviving idea became D18 |
| D15 | **Pinch-to-zoom maps onto the wheel zoom; 1:1 is one texel per *physical* pixel** | Crisp on Retina, matches what artists mean by 100 %, and keeps the zoom-snap ladder in image space | **Shipped**, with the predicted cost inverted: fit and 1:1 are already in physical px end to end, so `scale_factor` enters *nothing* there — 1:1 is one texel per physical pixel by construction, measured exact on a 2× display. It enters the *gesture* math instead |
| D16 | **A minimal macOS menu bar via `muda`; full screen via winit** | A Mac app without a menu bar reads as broken; native full screen gives the space transition | **Shipped**, with two corrections to the premise: winit already installs a default menu bar carrying ⌘Q, so it had to be disabled or it replaced ours wholesale — which also means the app was never actually unquittable. Full screen needed no code at all: winit's `Fullscreen::Borderless` *is* `toggleFullScreen:` on macOS |
| D17 | *(belongs to the wgpu branch: measure `request_adapter`, then decide on a DXGI hal leaf)* | — | **Superseded** (Appendix B.2): the ~140 ms turned out to be D3D12 driver init, not enumeration, so the leaf would have saved little — and could not have been built anyway |
| D18 | **GPU bring-up on its own thread, started on the first line of `main`; the window is created *before* the join** | Device creation is the longest single item on the launch path and needs no window — but neither does the window need to wait for it | **Shipped**, and it is the one ordering that matters. `Viewer::new` takes the GPU as a closure; get it wrong and the window's 9-31 ms serialize after the device (§1) |
| D19 | **The shell owns the device and the swapchain; sokol_gfx is handed them** (`sg_environment` / `sg_swapchain`) | sokol_app's window model was a dealbreaker (Appendix B.3); this keeps winit's window *and* sokol's one drawing API | **Shipped on both.** ~250 lines per OS — the only GPU-API-specific code left — with `render/mod.rs` aliasing one as `backend` so `gpu.rs` carries no `cfg` (§5) |
| D20 | **The swapchain backbuffer is plain UNORM; the fragment shader sRGB-encodes its own output** | Flip-model swapchains disallow `*_SRGB` formats, and ImGui's colors are already sRGB, so one UNORM target is correct for both passes | **Shipped.** The old two-render-target-view trick is gone; the shader owns the encode and must not be "fixed" into a linear write. The *format* is per-OS (§5.1) |
| D21 | **The mip chain is built on the CPU, on the decode worker** | sokol_gfx has no `GenerateMips`, and its rules forbid rendering into an image created with data | **Shipped.** ~5 ms on an 8.9 MB image, off the UI thread; the upload is one `sg_make_image` carrying every level |
| D22 | **`sokol-rust` is vendored; `sokol_imgui.h` is compiled by `fire`'s `build.rs`** | The crates.io `sokol` name belongs to an unrelated 2019 crate, and sokol_imgui must be compiled with the *same* cimgui defines and `SOKOL_*` backend as its neighbours or the struct layouts differ | **Shipped.** A vendored tree to update by hand, and three sets of defines that must stay in lockstep — a mismatch is silent (§15) |
| D23 | **The whole dev pipeline runs on macOS** — clippy, `cargo test`, the native decoders, the TTFP harness, the release build and packaging | A platform you cannot lint, test or measure on is a platform you cannot maintain; the alternative is mac fixes only Windows CI can verify | **Shipped.** The two sys crates lost their Windows-only short-circuit, the vendor layout went per-target, CI grew a mac leg (§13) and `ttfp.ps1` gained a portable twin |
| D24 | **Metal shaders are precompiled to a `.metallib` with `xcrun metal`**, so the macOS toolchain floor is full Xcode rather than the Command Line Tools | Keeps D4's "no shader compile on the cold-start path" on *both* OSes; the wgpu branch lost ~32 ms to exactly this | **Shipped.** Every dev machine and the CI runner need Xcode plus an 839 MB `MetalToolchain` component, and its absence is near-silent: asked for bytecode without it, sokol-shdc emits shader *source* and exits 0 (§13) |
| D25 | **The arm64 HEIF stack is built with vcpkg (`arm64-osx` static)** mirroring `VENDOR.txt`, and the vendored tree goes per-target — one directory per target holding both its `include/` and `lib/` | One vendoring story on both OSes, one dav1d port patch, and a self-contained `.app` with no dylib embedding or per-dylib signing | **Shipped.** Headers had to be split per target too (the two landed on different libheif versions), and the mac build needs a third port patch, `ENABLE_PLUGIN_LOADING=OFF` — without it dav1d becomes an unreachable dynamic plugin and *every* AVIF silently fails to decode |

---

## Appendix B - how the shell was chosen

Three shells were built and measured against the D2 gate before this one was adopted. All numbers
are `scripts/ttfp.ps1`: kernel process creation → first image-bearing present, 12 interleaved
launches per cell, release, idle machine, RTX 4080.

### B.1 - the risk, as assessed beforehand

`wgpu::Instance::request_adapter` has historically enumerated every adapter and created a D3D12
device per adapter to query it — reported at up to hundreds of ms on multi-GPU machines, which
would not hide under a ~140 ms decode. The prepared fix was a `cfg(windows)` leaf that picked one
adapter through `IDXGIFactory6` and handed it to wgpu through `wgpu_hal::dx12`. Phase 1's first
job was to measure whether that was needed.

### B.2 - attempt 1, wgpu: the gate is missed by ~135 ms

| Image | Win32 + D3D11 | `shell/wgpu` | Δ | Budget (D2) |
| --- | --- | --- | --- | --- |
| 38 KB PNG | 126-134 ms | 264-277 ms | **+138-143 ms** | ≤ 5 ms |
| 8.9 MB PNG | 144-145 ms | 278 ms | **+133 ms** | ≤ 10 ms |

About 235 ms of that is GPU bring-up: instance ~20 ms, `request_adapter` ~140 ms, `request_device`
~31 ms, and the viewport pipeline (naga → HLSL → FXC → PSO) ~32 ms. Moving the bring-up to a worker
thread changed nothing — the join wait was ~230 ms, because the bring-up *is* the critical path and
nothing on the main thread is long enough to hide it under.

Both premises of B.1 were wrong, and that is what killed the branch rather than the raw number.
The cost is **not** per-adapter enumeration: DXGI listed three adapters, but a warm second
enumeration takes ~1 ms — the ~140 ms is the driver's one-time D3D12 initialisation, which *any*
first device creation pays. And the leaf could not have been built against wgpu-hal 30 anyway:
`dx12::Adapter::expose` is `pub(super)` and needs the hal instance's private fields, so it would
have required a fork. D3D11 reaches first pixel ~100 ms sooner than D3D12-through-wgpu on this
machine, and that is an API and driver cost, not something Fire can engineer around.

What survived: the winit shell, the ImGui backends, the timer heap, the `interprocess` instance
model and the `KeyCode` keybinds are all independent of wgpu, and carried into the next attempt.
The pipeline compile also became D24's evidence — it is the same ~32 ms that precompiling to
bytecode exists to avoid.

### B.3 - attempt 2, sokol_app: the gate is met, the window model is lost

Replacing the *whole* shell with sokol — sokol_app for the window, device, frame loop and input,
sokol_gfx for drawing, sokol_imgui for the chrome — met the gate comfortably (both images within
noise of `main`, sd 4-8 ms). It was rejected anyway, on what sokol_app has no API for: one window
per process, a continuous frame loop (one frame per vsync, so the "idle costs ~0" invariant does
not hold), window position and maximized state, the launcher's Run setting, the system light/dark
theme, raising the window on a forwarded open, and a parent for the file dialogs. That list is
most of §9 and §10.

### B.4 - attempt 3, winit + sokol_gfx: adopted

The two were recombined: winit keeps the window, the event loop and input; sokol_gfx stays the one
drawing API; sokol_imgui stays the renderer, in its `SOKOL_IMGUI_NO_SOKOL_APP` mode with
`dear-imgui-winit` feeding it input. The price is the swapchain glue — `render/d3d11.rs` and its
macOS twin (D19).

| Image | Win32 + D3D11 | `shell/winit-sokol` | Δ | Budget (D2) |
| --- | --- | --- | --- | --- |
| 38 KB PNG | 133.5 / 132.4 ms | 135.4 / 131.6 ms | +1.9 / -0.8 ms | ≤ 5 ms |
| 8.9 MB PNG | 142.6 / 144.0 ms | 142.2 / 143.3 ms | -0.4 / -0.7 ms | ≤ 10 ms |

Release phases: the D3D11 device ~135 ms on the bring-up thread, `sg_setup` 0.3 ms, pipeline 1 ms;
on the main thread the winit window 9 ms, the swapchain 1.6 ms, ImGui 1.7 ms. The one thing that
mattered was D18's ordering — a first run before that fix read +8 ms on the small image.

Also verified on the branch: two windows in one process under `open-in = "new-window"`; an idle
window with an image open at 0.0 ms of CPU over 5 s; and position/maximized restore, theme,
launcher Run state and dialog parenting all intact.

One defect outlived two of the three shells and is worth remembering as a shader lesson, not a
presentation one: a flickering 1 px line on all four image edges, seen first on the sokol_app
prototype and reproduced here. It was neither the CPU mip chain nor a swapchain/client size
mismatch — it was an implicitly-derived LOD on a sample inside a per-pixel branch, undefined where
the quad diverges. Fixed in `897bc7e` by making every tap explicit; see the rule at the end of
§5.3.
