# Fire - macOS port plan

Fire shipped as a Windows-only Win32 + Direct3D 11 executable. This document is the resolved
plan for making it run on macOS with the **minimum amount of platform-specific code** - the
governing constraint - while keeping the project's primary metric, time-to-first-pixel, where
it is. Every decision below was made against that constraint; where it costs something on
Windows, the cost is named and gated by measurement.

Phase 1 (the Windows migration) is **built and measured** on branch `shell/winit-sokol`; the
TTFP gate is met. Sections 1-6 describe what is in the tree today, with the parts still to be
written for macOS marked *planned*. Appendix A keeps the three shell attempts that got us here,
as written at the time - the wgpu branch that failed the gate, the sokol_app prototype that
passed it but lost the window model, and the winit + sokol_gfx recombination that is now the
shell.

The *dev pipeline* is part of the port, not an afterthought (D23): build, clippy, `cargo test`,
the native decoders, the TTFP harness and the release/packaging chain all have to run on a Mac,
not just `cargo build`. §7 is what that costs and what currently blocks it.

It is a companion to `architecture.md`, not a replacement: the decode pipeline (§6), color
management (§7), the ImGui chrome design (§8) and the viewer features (§10) are unchanged.
What changes is the *shell* - the window, the GPU presentation, the event loop, the process
model - and the build/distribution chain.

---

## 1. The governing decision

There are two ways to run on a second OS: a second native shell (AppKit + Metal beside
Win32 + D3D11), or one shared shell. A second native shell would be *faster to first light* -
nothing on Windows moves - but leaves two full shells to maintain forever, each with its own
message loop, swapchain, DPI, dark-mode, DnD, timer and dialog code. That is the opposite of
the constraint.

**So the whole app moves to one shared shell: `winit` for windowing and input, `sokol_gfx` for
the GPU, and Dear ImGui through `dear-imgui-winit` (input) + `sokol_imgui` (rendering).** The
one per-OS piece is the *device and swapchain* sokol_gfx draws through - `render/d3d11.rs` on
Windows (~230 lines), a `CAMetalLayer` twin on macOS - because sokol_gfx does not own a window:
it is handed a device at `sg_setup` (through `sg_environment`) and a render target per frame
(through `sg_swapchain`), and the shell owns everything around them.

That is the whole platform surface of the renderer. Everything above it - the pipeline, the
samplers, the textures, the mip upload, the draw, the ImGui pass - is one API on both OSes, and
everything above *that* (`crate::ui`, the app) names no GPU API at all.

Windows was migrated first, on a branch, and gated on the existing TTFP benchmark (§8). macOS
is then the leaves in §5, the MSL half of the shader (§4), the Metal device/layer module, and
packaging.

What survives untouched, because it never had Win32 in it: `fire-decode` and every decoder,
`fire-ipc`'s wire format, `flipbook.rs`, `render/view.rs` (zoom-snap math), `folder.rs`, the
config/theme parsing, and the entire `ui/` layer (`ViewSnapshot` in, `ui::Frame` out). That
last one is the payoff of the GDI→ImGui migration: the chrome ported for free.

---

## 2. Decisions

Each is stated with the reason and what it costs, so a future revision can revisit it.
*Shipped* = in the tree on `shell/winit-sokol`. *Planned* = Phase 2 or later.

| #  | Decision | Reason | Cost | Status |
| -- | -------- | ------ | ---- | ------ |
| D1 | **Shared shell: winit + sokol_gfx on both OSes** (not AppKit+Metal beside Win32+D3D11) | Minimum platform code: the per-OS part is a device/swapchain module, not a renderer | Windows re-plumbed; TTFP re-measured (§8) | Shipped |
| D2 | **Windows migrates too**, gated on the TTFP benchmark | Keeping a whole D3D11 renderer behind a trait *is* the two-shell maintenance | Budget: ≤ 10 ms median regression on the 8.9 MB case; ≤ 5 ms on the 38 KB case | Met (§8) |
| D3 | **`dear-imgui-rs` Context + `dear-imgui-winit` (input) + `sokol_imgui` (renderer)** replace `dear-imgui-sys` + the C++ win32/dx11 shims | No per-OS backend code of our own; the renderer is the same header sokol_gfx's author maintains beside it | `ui/` moved from the raw 10-function ABI to the safe API; `simgui.c` must compile against the same cimgui `dear-imgui-sys` links (D22) | Shipped |
| D4 | **Shader is precompiled to bytecode on both OSes** - HLSL → DXBC by `fxc` today; for macOS, **adopt `sokol-shdc`**: one annotated-GLSL source generating HLSL + MSL *and* the `ShaderDesc` reflection, with the MSL compiled to a `.metallib` by `xcrun metal` (D24) | Nothing on the cold-start path (no runtime shader compile) on either OS, and a broken shader is a build error; one source beats two hand-kept-in-sync twins once a second backend exists | `fxc` + the Windows SDK on Windows, the Metal toolchain on macOS (D24). The shader is now one annotated-GLSL source; `scripts/gen-shaders.sh` generates the per-backend sources *and* the `ShaderDesc` into `render/generated/`, checked in, and build.rs compiles the host's pair to bytecode. `make_shader` keeps only the bytecode swap | Shipped (HLSL half needs a Windows build to confirm) |
| D5 | **One process, N windows** everywhere (instance mode is `open-in = new-window \| reuse-window`) | Finder never launches a second process - it sends an open-file event to the running app - so per-launch processes have no mac equivalent; winit runs N windows in one loop cleanly | Windows NewWindow users get the same UX from one process; crash isolation is per-process (D7) | Shipped |
| D6 | **IPC via the `interprocess` crate** (named pipe / Unix socket behind one API) on both OSes | One forward path, one test | macOS also needs the Apple-Event hook (§5) feeding the same open path, which reaches it by adding `application:openURLs:` to winit's delegate class at runtime | Shipped |
| D7 | **Accept single-process crash exposure** | FFI already runs under `catch_unwind` on a worker with validated inputs; a viewer has no unsaved state | A true segfault in libheif/psd_sdk closes every window, not one | Accepted |
| D8 | **Timers: `ControlFlow::WaitUntil` + a deadline min-heap** | Preserves the event-driven invariant (no input, no timer → no frame) with zero threads | Small scheduler in the app; every timer (GIF, flipbook, caret) goes through it | Shipped |
| D9 | **Keybinds: physical `KeyCode` by name + a `Primary` modifier** (Ctrl on Windows, ⌘ on macOS) | Layout-independent, one `config.toml` works on both | One-time migration of existing VK-code chords; `Ctrl+`/`Cmd+` still parse as `Primary+` | Shipped |
| D10 | **macOS: Apple Silicon only, vendored arm64 static libs** for libheif/libde265/dav1d + `cc`-built psd_sdk | Same vendoring model as the Windows `.lib`s; no Intel users to serve | Re-run `VENDOR.txt` on a Mac; universal deferred | Planned |
| D11 | **Build, sign and notarize only on the dev Mac, via `scripts/build-mac.sh`** - CI never builds a mac artifact | Keeps the Developer ID cert and App Store Connect key off CI entirely, on a public repo; CI's mac leg (D23) stays lint/test only | Releases are a manual step on the dev Mac rather than a CI-triggered build | Shipped (`scripts/build-mac.sh`, run with no arguments; both credentials come from the keychain) |
| D12 | **Packaging: `scripts/build-mac.sh` + `Info.plist` template from `product.json`, signed + notarized `.dmg`** | Mirrors `build-installer.ps1`; plist is hand-tuned anyway (cargo-bundle would hide it) | `.dmg` is more script than `.zip`; accepted for polish. The plist turned out to be worth hand-writing for a second reason: the document types are parsed out of `fire-decode`'s one extension table, so unlike the installer's copy they cannot drift, and `NSSupportsSuddenTermination` is pointedly *not* declared - it would let the OS skip the `atexit` that removes the instance socket | Shipped |
| D13 | **Windows first, then macOS** | The shared code and the TTFP risk were the Windows migration; mac is leaves + packaging | Colleagues wait one extra phase | Done |
| D14 | *(wgpu-era: pinned backend, no debug layers, decode kicked off before device creation)* | - | - | Superseded (A.2); the surviving idea is D18 |
| D15 | **Pinch-to-zoom mapped to the wheel zoom; 1:1 = one texel per *physical* pixel** | Crisp on Retina, matches what artists mean by 100 %, zoom-snap ladder stays in image space | `scale_factor` enters the fit/1:1 math | Shipped. Pinch maps `WindowEvent::PinchGesture`'s incremental magnification to the wheel's about-cursor zoom (NaN filtered, as winit permits one). The cost line proved wrong: fit and 1:1 are already in physical px end to end, so `scale_factor` enters nothing there — 1:1 is one texel per physical pixel by construction, measured exact on a 2× display (§ Phase 2 step 6). It enters the *gesture* math instead, which is the opposite conversion |
| D16 | **Minimal macOS menu bar via `muda`; fullscreen via winit** | A Mac app without a menu bar reads as broken; native fullscreen gives the space transition | ~180 lines in `menubar.rs`, all `cfg(target_os = "macos")`, plus two crates (`muda`, `keyboard-types`) that reuse winit's objc2 family. Two corrections to the premise: winit *already* installs a default menu bar carrying ⌘Q, so it had to be disabled (`with_default_menu(false)`) or it replaced ours wholesale — and it means the app was never actually unquittable. Fullscreen needed no code: winit's `Fullscreen::Borderless` is `toggleFullScreen:` on macOS, the native space transition already | Shipped |
| D17 | *(wgpu-era: measure `request_adapter`, then decide on a DXGI hal leaf)* | - | - | Superseded (A.2): the ~140 ms was D3D12 driver init, not enumeration |
| D18 | **GPU bring-up on its own thread, started on the first line of `main`; the window is created *before* the join** | Device creation is the longest single item on the launch path and needs no window - but neither does the window need to wait for it | `Viewer::new` takes the GPU as a closure; get the order wrong and the window's 9-13 ms serialize after the device (§8) | Shipped |
| D19 | **The shell owns the device and the swapchain; sokol_gfx is handed them** (`sg_environment` / `sg_swapchain`) | sokol_app's window model was a dealbreaker (A.3); this keeps winit's window *and* sokol's one drawing API | ~230 lines per OS - the only GPU-API-specific code left; `render/mod.rs` aliases one of them as `backend` so `gpu.rs` carries no `cfg` | Shipped both (Metal's swapchain half unexercised until the shader lands) |
| D20 | **The swapchain backbuffer is plain UNORM; the pixel shader sRGB-encodes its own output** | Flip-model swapchains disallow `*_SRGB` formats, and ImGui's colors are already sRGB, so a single UNORM target is correct for both passes | The old two-RTV (`UNORM` + `UNORM_SRGB` view) trick is gone; the shader owns the encode and must not be "fixed" into a linear write. The *format* is per-OS - `R8G8B8A8_UNORM` on D3D11, `BGRA8Unorm` on Metal, because a `CAMetalLayer` does not accept RGBA8 - so `SWAPCHAIN_FORMAT` lives in the backend module. Channel order only; the shader is unchanged | Shipped |
| D21 | **The mip chain is built on the CPU, on the decode worker** | sokol_gfx has no `GenerateMips`, and its rules forbid rendering into an image created with data | ~5 ms on an 8.9 MB image, off the UI thread; the upload is one `sg_make_image` carrying every level | Shipped |
| D22 | **`sokol-rust` is vendored (`vendor/sokol-rust`); `sokol_imgui.h` is compiled by `fire`'s build.rs** | The crates.io `sokol` name belongs to an unrelated 2019 crate; sokol_imgui must be compiled with the *same* cimgui defines and the same `SOKOL_*` backend as its neighbours or the struct layouts differ | A vendored tree to update by hand; three sets of defines (backend, `SOKOL_IMGUI_NO_SOKOL_APP`, the cimgui five) that must stay in lockstep | Shipped |
| D23 | **The whole dev pipeline runs on macOS**, not just the app: clippy, `cargo test`, the native decoders, the TTFP harness, the release build and packaging | A platform you cannot lint, test or measure on is a platform you cannot maintain; the alternative is mac fixes that only Windows CI can verify | The two sys crates lose their Windows-only short-circuit, the vendor layout goes per-target, CI grows a mac leg, `ttfp.ps1` gets a portable twin (§7) | Shipped, except packaging (step 9) |
| D24 | **Metal shaders are precompiled to a `.metallib` with `xcrun metal`** - so the toolchain floor on macOS is **full Xcode** (plus the separately-downloaded Metal toolchain on Xcode 16+), not Command Line Tools | Keeps D4's "no shader compile on the cold-start path" on both OSes; the wgpu branch lost ~32 ms to exactly this (A.2), and TTFP is the project's primary metric | Every dev machine and the CI runner need Xcode, not CLT; a second offline compile step in build.rs. The toolchain is an **839 MB** `xcodebuild -downloadComponent MetalToolchain`, and its absence is near-silent (§7.1) | Shipped |
| D25 | **The arm64 HEIF stack is built with vcpkg (`arm64-osx` static), mirroring `VENDOR.txt`**, and the vendored tree goes per-target - one directory per target holding *both* its `include/` and `lib/` | One vendoring story on both OSes, one dav1d port patch, a self-contained `.app` with no dylib embedding or per-dylib signing | A one-time `brew install cmake ninja meson nasm pkg-config` + vcpkg bootstrap on the Mac; `heif-sys`'s hardcoded `lib/` path and `.lib` names become target-aware. Headers turned out to need the split too (the two targets landed on different libheif versions), and the mac build needs a third port patch, `ENABLE_PLUGIN_LOADING=OFF` (§7.2) | Shipped |

---

## 3. Architecture after the port

```
Explorer / Finder open
        │  Windows: fire.exe "path"          macOS: Apple Event → application:openFiles:
        ▼
┌───────────────────────────────────────────────────────────────────┐
│  fire (one process)                                               │
│                                                                   │
│  main(): GPU bring-up thread starts here (D18)                    │
│          read config → try to become the instance (interprocess)  │
│          owner: run the event loop      | client: forward & exit  │
│                                                                   │
│  winit event loop (ControlFlow::Wait / WaitUntil)                 │
│   ├─ Window 1 ─ swapchain + Viewer (view, chrome, timers)         │
│   ├─ Window 2 ─ …                                                 │
│   └─ deadline heap → next WaitUntil                               │
│                                                                   │
│  render:  one device + one sokol_gfx for the process              │
│           swapchain per window (render/d3d11.rs | metal.rs)       │
│           image pass  = one fullscreen triangle, precompiled HLSL │
│           UI pass     = sokol_imgui into the same pass            │
│                                                                   │
│  decode worker pool  ──EventLoopProxy──▶ event loop               │
│  fire-decode core (unchanged)                                     │
└───────────────────────────────────────────────────────────────────┘
```

### 3.1 Process and instance

`instance_mode` became `open_in: NewWindow | ReuseWindow`. Launch:

1. Try to bind the local socket (`interprocess`). Success → this is the owner; run the loop.
2. Bind fails as "already taken" → connect, send the `fire-ipc` message (unchanged wire
   format), exit. If the owner turns out to be unreachable - it was exiting as we launched -
   fall through and run un-coordinated rather than lose the open.

The owner applies `open_in`: `NewWindow` creates a window in the same loop, `ReuseWindow`
routes to the focused window. No mutex - the socket bind *is* the mutex. "Already taken" is
`AddrInUse` on a Unix socket and `PermissionDenied` on a Windows named pipe (which
`interprocess` creates with `FILE_FLAG_FIRST_PIPE_INSTANCE`); `ipc_server::is_taken` knows
both. The name is namespaced where the OS has a namespace for one (Windows named pipes, Linux
abstract sockets) and a socket file under the user's runtime dir otherwise (macOS).

On macOS the socket is only reached by a bare binary invocation; Finder, Dock and `open`
deliver files as Apple Events to the running `.app`, which the delegate hook (§5) routes to
the same open path. Cold start with no running instance is identical on both.

**A Unix socket outlives its owner, and that had to be handled.** Where the name lives in an OS
namespace (Windows named pipes, Linux abstract sockets) the kernel frees it when the owner dies;
a socket *file* just stays. An owner killed with `SIGKILL` — or crashed, which D7 accepts as a
real outcome — therefore left a file that answered no connection but still failed every future
`bind` with `AddrInUse`, and the fallback path treated that as "an owner exists". Two
consequences, both found by running the bundled app from Finder: every later launch stalled for
the two-second connect timeout, and a launch *with no path to forward* exited immediately with no
window at all — `forward(None)` returned `Ok` without ever connecting, and `Ok` means "forwarded,
now exit". A double-clicked Fire flashed in the Dock and vanished, permanently, until the file
was deleted by hand. The fix is in three places: `forward(None)` now connects, so it can fail;
`ipc_server::rebind_after_stale` unlinks a socket only once a forward has already proved nobody
answers; and `main` then re-binds and serves, so the first launch after a crash repairs the state
instead of running un-coordinated. Windows cannot reach any of it — there is no file to go
stale — but the no-path probe makes it honest there too.

**That was the recovery, not the cure**, and step 7's measurement showed how much was still being
left to it. `SIGKILL` is not the common way a socket goes stale on macOS — **⌘Q is**. AppKit's
`terminate:` ends in `exit()`, so `main` never returns and the `Listener` is never dropped: every
ordinary quit left a socket, and the next launch paid the full connect timeout to rediscover that
(168 ms → 2196 ms, measured). The owner now registers an `atexit` `unlink` of its own socket, and
a refusal — as opposed to a name that is merely not there yet — is retried for 150 ms rather than
two seconds. See Phase 2 step 7.

The Windows foreground handoff (`AllowSetForegroundWindow` on the forwarding side,
`focus_window()` on the owner) stays exactly as in `architecture.md` §4.1, as a `cfg(windows)`
leaf in `platform.rs` called from the forward path.

### 3.2 Rendering

One device and one `sg_setup` for the process (sokol_gfx is a process-wide singleton); one
swapchain per window, created by the shell. Frame:

```
RedrawRequested → swapchain.render_view()     (recreated after a resize; None → skip the frame)
                  sg_begin_pass(swapchain)     clear to the chrome colour
                  image pass : viewport = the image sub-rect, one triangle, 128-byte
                               uniform block via sg_apply_uniforms
                  UI pass    : simgui_render() into the same pass
                  sg_end_pass, sg_commit
                  present    : Present(1, 0), vsync-paced
```

`Present` reports whether anyone is looking: DXGI answers `DXGI_STATUS_OCCLUDED` *immediately*
when the window is hidden or fully covered, and playback paced on a present that no longer
blocks would spin, so the caller is told.

sRGB is D20: one plain UNORM target, the image shader encoding its own output, ImGui's
already-sRGB colors landing untouched (sokol_imgui picks its gamma from the swapchain format,
which says UNORM). Texture formats map as in §5.1 of `architecture.md`: 8-bit sources upload as
`Srgb8a8` (hardware sRGB→linear on sample), float sources are already linear, 16-bit unorm is
sRGB-decoded in the shader; where the device lacks a format the pixels are converted on the way
in. The mip chain is the app's (D21) and every level is uploaded in the one `sg_make_image`.
The 128-byte constant buffer is now a sokol_gfx uniform block - same contents, same "pan/zoom
change a transform, not pixels" property.

Rendering stays **event-driven**: `request_redraw` only after input, a decode landing, or a
timer. An idle window with an image open measured 0.0 ms of CPU over 5 s.

Resize drops the render-target view and calls `ResizeBuffers`; a zero dimension (a minimized
window) is remembered but not applied - DXGI refuses it - and the frame is skipped. A
device-removed reset shows up as a failed `GetBuffer` / `CreateRenderTargetView`, which skips
the frame rather than drawing into nothing; relaunch remains the recovery story, since there is
nothing to save.

### 3.3 Timers

`app::timers`: a `BinaryHeap` of `(Instant, seq)` with a side table of `(WindowId, TimerKind)`.
Arming pushes and returns a `seq`; `about_to_wait` pops everything due, dispatches (advance the
GIF frame, run the flipbook pump, blink the caret), then sets `ControlFlow::WaitUntil(peek)` or
`Wait` if empty. Cancellation is by sequence number rather than removal - a popped entry whose
`seq` the window no longer wants is simply dropped - so a re-armed timer never has to find its
predecessor in the heap. The existing `sync_animation` / flipbook logic is unchanged; it calls
`timers.arm` instead of `SetTimer`.

### 3.4 Input

`dear-imgui-winit` feeds ImGui; the same two booleans (`want_capture_mouse`,
`want_text_input`) gate who owns an event, and the three keyboard special cases from
`architecture.md` §8 (keybind capture, modal settings, non-modal popup) carried over verbatim -
they are app logic, not Win32 logic. A chord is `(Modifiers, KeyCode)` serialized by name
(`"Primary+O"`, `"F11"`, `"Num+"`); `Primary` resolves at match time; chords match exactly, so
a modifier held by accident no longer triggers the plain command.

The platform backend runs with its DPI handling **locked to 1.0**: the whole UI lays out in
physical pixels (`ui::theme::Metrics` scales from the DPI itself), so ImGui's coordinate space
must be the framebuffer's, not winit's logical one. A DPI change is then just
`set_font_scale_dpi` - ImGui 1.92 rasterizes glyphs on first use, so there is no atlas to
rebuild; only the icon texture, a real raster, is rebuilt.

Pinch (`WindowEvent::PinchGesture`, macOS) drives the same about-cursor zoom as the wheel (D15).

### 3.5 No modal loop inside a winit handler

**Nothing called from a winit callback may pump an event loop of its own.** That is a rule the
Win32 shell did not have, and breaking it is not a glitch - it aborts the process.

Found the hard way (2026-09-04): the Open… picker crashed Fire on macOS, reliably, as soon as the
mouse moved over the panel. `rfd`'s `pick_file` puts up an app-modal `NSOpenPanel` and calls
`runModal`, which pumps its own loop. We were calling it from `about_to_wait` - deliberately, from
the Win32 days: "never from inside a redraw". But **every** one of our handlers, the idle step
included, runs inside winit's dispatcher, which holds a `RefCell` borrow for the whole call.
AppKit's modal loop then routes a mouse event through winit's `sendEvent:` override, a gesture
recognizer spins a *third* loop (`runUntilDate:`), a run-loop block re-enters winit's
`handle_event`, and it panics: *"tried to handle event while another event is currently being
handled"*. The panic unwinds into a CoreFoundation callback, where unwinding is forbidden, so the
process aborts - `firewall`'s `catch_unwind` is nowhere near that path, and could not have caught
it anyway. Windows has the same guard in its runner and so is exposed to the same class of bug.

The fix is to start the picker on a worker thread and answer with an `AppEvent`
(`app::viewer::Dialog`). `rfd` hands the panel to the main thread itself (`dispatch_sync` on
macOS, per-call COM init on Windows), so the modal loop runs from the *run loop* rather than from
inside our handler, and re-entrancy never arises. As a bonus the window behind the picker stays
live - verified on screen: an image forwarded from a second launch loaded and redrew *while* the
picker was up, which is exactly the dispatch that used to abort.

The same rule retired the last blocking call: the "could not open a window" message box in
`create_viewer` is now recorded and shown by `main` after `run_app` returns.

---

## 4. Shader

`render/shader.glsl` is **the one source**: sokol-shdc's annotated GLSL (Vulkan syntax, separate
texture and sampler objects). `scripts/gen-shaders.sh` turns it into everything else, all of it
checked in under `render/generated/`:

* `shader_viewport_hlsl5_{vertex,fragment}.hlsl` and `..._metal_macos_{vertex,fragment}.metal` -
  the per-backend sources, which **build.rs compiles to bytecode**: `fxc` to `.dxbc` on Windows,
  `xcrun metal` + `metallib` to a `.metallib` per stage on macOS (one library per stage, because
  SPIRV-Cross names every entry point `main0` and two functions cannot share a library).
* `shader.rs` - the sokol_gfx reflection: the 128-byte uniform block, the texture, the two
  samplers, which sampler pairs with the texture, and the per-backend entry-point names. This is
  what used to be written out by hand in `render::gpu::make_shader`; all that is left there is
  swapping the generated desc's `source` for build.rs's `bytecode`.

So a plain `cargo build` never needs sokol-shdc, there is no runtime shader compile on either OS,
and a broken shader is still a build error. Editing the shader means editing the `.glsl`, running
the script and committing both - the script's output is platform-independent text, so either OS
can regenerate it.

`gpu.rs` asserts `size_of::<Params>()` equals the *generated* uniform-block struct's size, so an
edit that changes the block fails the build rather than producing a wrong-looking image.

The stages are unchanged from the hand-written HLSL: sample (point when magnifying, aniso+mip when
minifying, chosen by the two samplers) → HDR exposure/tonemap (float formats only) → channel
isolation → checkerboard composite, with `Rgba16Unorm` sRGB→linear in the shader. The generated
HLSL's `packoffset`s and its `b0`/`t0`/`s0`/`s1` registers came out byte-identical to the
hand-written cbuffer, and neither backend's generated code inserts a Y-flip: `gl_FragCoord` maps
to `SV_Position` and `[[position]]`, both top-left origin, which is what the pixel math assumes.

**One rule the shader must keep:** never sample inside a per-pixel branch without explicit
derivatives. The letterbox and outline tests above the sampling are branches, and an
implicitly-derived LOD inside a branch is undefined where the quad diverges - which is what
produced a flickering 1 px line on all four image edges. Every tap is `textureLod` or
`textureGrad`; the fix is commit `897bc7e` and the flicker is gone.

---

## 5. Platform leaves (the complete inventory)

Everything below is the platform-specific code that remains. Each is one small module or a
`cfg` block; nothing else in `fire` should mention an OS - `platform.rs` says so in its header,
and this table is what it is pointing at.

| Concern | Windows | macOS | Shared via |
| --- | --- | --- | --- |
| Window, loop, DPI, DnD, theme change, fullscreen, placement | - | - | `winit` |
| GPU device + swapchain | `render/d3d11.rs` (~230 lines): D3D11 device + DXGI flip-model swapchain | `render/metal.rs` (~210 lines): a `CAMetalLayer` on the winit window, handing sokol_gfx its `MTLDevice` and per-frame drawable | `sokol_gfx` above them |
| Shader bytecode | HLSL → DXBC (`fxc`, build.rs) | MSL → `.metallib` (`xcrun metal` + `metallib`, build.rs) | one `sokol-shdc` source (`render/shader.glsl`) generating both, plus the reflection |
| Config dir | `%APPDATA%\fire` | `~/Library/Application Support/fire` | `dirs` |
| Dark mode | - | - | `winit` `Window::theme()` / `ThemeChanged` (registry read removed) |
| Open-file dialog + startup error boxes | - | - | `rfd` |
| Hot-reload watch | `ReadDirectoryChangesW` | FSEvents | `notify` (unchanged) |
| IPC transport | named pipe | Unix socket file (runtime dir) | `interprocess` |
| Foreground handoff on forward | `AllowSetForegroundWindow` leaf | not needed | - |
| Launcher "Run" show state | `GetStartupInfoW` leaf | no equivalent (returns `None`) | `platform.rs` |
| Clipboard (Copy File / Path / Name) | `CF_HDROP` + text leaf | `NSPasteboard` file URL (Copy File) + `pbcopy` (text) | `platform.rs` |
| Show in Explorer / Reveal in Finder | leaf | leaf | `platform.rs` |
| Open-file events from the OS | argv | `openfiles.rs` (~130 lines): `application:openURLs:` added to winit's delegate class at runtime | both call the same open path |
| Menu bar | none | `menubar.rs`: `muda` minimal (App / File / Window) dispatching `KeyAction`s | - |
| File association | `HKCU` ProgID (installer, unchanged) | `CFBundleDocumentTypes`, `LSHandlerRank = Alternate` (planned) | - |
| Native decoder libs | vendored `.lib` | vendored arm64 `.a` (planned) | same `VENDOR.txt` recipe |
| Icon / metadata | `winresource` | `Info.plist` + `.icns` (planned) | both from `product.json` |
| Installer | Inno Setup (unchanged) | `build-mac.sh` → signed, notarized `.dmg` (planned) | - |

Removed outright by Phase 1: the Win32 window class and wndproc, the D3D11 drawing code and the
C++ ImGui win32/dx11 backend shims, `SetTimer`, `DragAcceptFiles`, `GetWindowPlacement` /
`SetWindowPlacement`, the registry dark-mode read, and the named mutex. What is left of
`windows-sys` is the five leaves above.

---

## 6. Crates

**Shell / render.** `winit` (`=0.30.13`), `sokol` (path: `vendor/sokol-rust`, floooh/sokol-rust
@ b22a545), `dear-imgui-rs` (`=0.17.0`, `default-features = false`), `dear-imgui-winit`
(`=0.17.0`), plus `cc` as a build dependency for `simgui/simgui.c`. `windows` (0.61, Windows
only) carries the typed COM for `render::d3d11`; `windows-sys` (0.60, Windows only) is the
platform leaves.

**Shell services.** `interprocess` (2.4), `dirs` (6), `rfd` (0.17, `default-features = false`).

**Unchanged.** Everything in `fire-decode`, `notify`, `crossbeam-channel`, `serde` / `toml`, and
`winresource` + `resvg` + `serde_json` as build dependencies.

**Dropped.** `wgpu` and the WGSL path; `dear-imgui-wgpu`; direct use of `dear-imgui-sys` (it is
still linked, as the crate `dear-imgui-rs` sits on and the cimgui `simgui.c` compiles against);
the D3D11/DXGI *drawing* code that used to live behind `windows`. `bytemuck` remains a
`fire-decode` dependency only.

**macOS.** `muda` (0.19, `default-features = false`) for the menu bar, plus `objc2` (0.5) and
`objc2-foundation` / `objc2-app-kit` / `objc2-quartz-core` / `objc2-metal` (0.2) for
`render/metal.rs`, `openfiles.rs` and the pasteboard leaf. The `objc2` versions are winit's own,
so only `muda` and its `keyboard-types` are new compiles.

Pin `winit` and the two `dear-imgui-*` crates to exact versions; they move together, and
`dear-imgui-sys`'s cimgui is what `simgui.c` is compiled against (D22), so a bump is a
three-place change: the crate versions, `simgui/cimgui.h`, and a rebuild proving the defines
still match.

---

## 7. Development environment

D23: the dev loop is part of the port. Everything a Windows checkout can do - build, clippy,
`cargo test`, the native decoders, the TTFP harness, the release build, the installer - a Mac
checkout must do too. What that needs, and what is in the way.

### 7.1 What a Mac needs installed

* **Xcode** (full, not just Command Line Tools), plus the separately-downloaded Metal toolchain
  on Xcode 16+. D24 puts `xcrun metal` on the build path; Xcode also carries `notarytool` /
  `stapler` for D12 and Instruments for launch-path profiling. **`xcrun --find metal` is not a
  sufficient check**: on Xcode 16+ it finds a *stub* that exists before the Metal toolchain does,
  and only fails when run ("cannot execute tool 'metal' due to missing Metal Toolchain"). Install
  it with `xcodebuild -downloadComponent MetalToolchain` (839 MB) and verify by *running*
  `xcrun -sdk macosx metal --version`. The failure this hides is quiet in both directions:
  sokol-shdc, asked for bytecode without it, emits shader *source* and exits 0 - which would put
  shader compilation back on the launch path, the cost D4 exists to avoid.
* **rustup**, stable channel. `rust-toolchain.toml` currently pins
  `targets = ["x86_64-pc-windows-msvc"]`, which makes a Mac download a Windows std it will never
  use; make the list host-conditional or add `aarch64-apple-darwin` beside it.
* **Homebrew: `cmake ninja meson nasm pkg-config`**, and a bootstrapped **vcpkg** - for the
  one-time HEIF build of D25 only. A normal `cargo build` needs none of them.

`scripts/dev-app.sh` wraps the built binary in a minimal `.app` so the shell can be exercised the
way it will actually be run. That is not cosmetic: a bare executable gets no Dock presence, no
proper activation, and is not what `open` delivers file arguments to, so the leaves in step 5 -
the Apple-Event open path above all - cannot be tested without a bundle. It is the *dev* bundle
(no icon, no document types, unsigned); `build-mac.sh` (D12) remains the shipping one.

Everything else, clang and the macOS SDK already provide: bindgen's libclang (the sys crates'
"check that `libclang.dll` is on PATH" message is Windows-shaped, but it is the same dylib -
clang-sys finds it by itself inside `xcode-select -p`'s toolchain, so nothing needs setting; what
*did* need removing was `.cargo/config.toml`'s `LIBCLANG_PATH` pin at `C:\Program Files\LLVM\bin`,
a **global** override - cargo's `[env]` cannot be made host-conditional - that short-circuited that
search on every Mac build. clang-sys globs the same Windows location on its own, so the pin was
redundant there too),
`lcms2`'s static Little-CMS, `dear-imgui-sys`'s cimgui, and the vendored sokol tree - which
compiles itself as Objective-C and links `Cocoa` / `QuartzCore` / `Metal` on its own
(`vendor/sokol-rust/build.rs`), so the Metal backend costs nothing at the tooling level.

### 7.2 The Windows-only short-circuits

`heif-sys` and `psd-sdk-sys` both wrote a `compile_error!` stub and returned early when
`CARGO_CFG_TARGET_OS != "windows"`, so `fire`'s default features could not build on a Mac at all.
**Done** (2026-09-04); both were as sized, with three findings that were not in the plan:

* **`psd-sdk-sys` was nearly free**, as expected. The vendored psd_sdk is already clang-aware
  (`PsdPch.h` sets `PSD_USE_CLANG`; `PsdPlatform.h` gates `<windows.h>` on `_WIN32`), and
  `wrapper.cpp` reads through its own `MemoryFile : psd::File`, so `NativeFile` is never
  instantiated. The short-circuit is gone, `PsdNativeFile.cpp` (Win32 `CreateFileW` + overlapped
  IO) is excluded off Windows alongside the `_Linux` / `_Mac` files, `-std=c++17` stays, and `cc`
  emits the `c++` link flag itself for a `cpp(true)` build. No new tooling.
* **`heif-sys` needed the libs built first (D25).** The vendored tree is now one directory per
  target (`vendor/x64-windows/`, `vendor/arm64-macos/`), each holding its *own* `include/` as well
  as `lib/`, and build.rs picks one from `CARGO_CFG_TARGET_OS`/`_ARCH` and names `c++` explicitly
  on macOS - Mach-O objects carry no `/DEFAULTLIB` directives, so the C++ runtime libheif and
  libde265 need will not link itself.
* **Headers had to go per-target too.** vcpkg's current baseline is libheif 1.23.2 against the
  vendored Windows 1.23.0, so one shared `include/` would have had bindgen parse one version's
  headers and link the other's libs. The drift is additive today and harmless to `wrapper.c`'s
  subset, but the split makes that a structural guarantee rather than a standing inspection.
* **The mac build needs a third port patch: `-DENABLE_PLUGIN_LOADING=OFF`.** libheif defaults it
  ON, and where the platform supports plugins dav1d is then built as a *separate dynamic plugin*
  (`plugins/libheif/libheif-dav1d.so`) instead of being compiled in. In a static build that
  plugin is unreachable and the failure is silent - `libheif.a` links cleanly and simply has no
  dav1d references, so every AVIF fails to decode at runtime. Windows never hit this. The
  acceptance check is now in `VENDOR.txt`: `nm -u lib/libheif.a | grep -c dav1d` must be > 0.

### 7.3 What builds today, and what does not

`cargo test -p fire-ipc`, `cargo test -p fire-decode` (default features, so through both native
decoders), `heif-sys` and `psd-sdk-sys` all pass on the Mac: **109 tests**, `cargo fmt --check` and
clippy clean. The libheif fixtures are real `.avif` / `.heic` files asserted per-pixel, so the
static dav1d and libde265 links are proven, not merely resolved.

`fire` now compiles and `cargo test --workspace` passes: **206 tests**, `cargo fmt --check`
clean. Its build script needed one fix - `winresource` was guarded by a runtime
`if target_os == "windows"` rather than a `#[cfg]`, which still has to compile on a host where
that `cfg(windows)` build-dependency is absent, so `embed_resources` and `packed_version` are now
`#[cfg(windows)]` with no-op twins, gated on the *host* because that is what a build script is
compiled for.

`render/gpu.rs`'s unconditional `use crate::render::d3d11;` is gone: `render/mod.rs` now aliases
whichever platform module this build has as `backend`, and `gpu.rs` names only that. The two are
twins rather than a trait - the target set is closed, so an alias costs nothing at runtime and
keeps `cfg` out of the shared file - which makes the contract (`Device::create`,
`fill_environment`, `Swapchain::{new,size,resize,acquire,present}`, `SWAPCHAIN_FORMAT`) a thing
that must be kept in step by hand. `render/mod.rs`'s header says so.

Three things the Metal side does not share with D3D11, each load-bearing:

* **The backbuffer is `BGRA8Unorm`.** A `CAMetalLayer` accepts only a short list of formats and
  RGBA8 is not among them, so `SWAPCHAIN_FORMAT` moved into the backend module. Storage order
  only - the shader still writes float4 RGBA - so D20 is unchanged.
* **sokol presents the drawable, not the shell.** `sg_end_pass` calls `presentDrawable:` on
  whatever `sg_swapchain` pointed at, and `sg_commit` commits; `Swapchain::present` only releases
  the drawable. Presenting again there would be a double present.
* **The frame blocks at acquire, not at present.** `nextDrawable` waits for the display; D3D11
  waits inside `Present(1, 0)`. Both are "the handoff blocked", which is what playback is paced
  on (`viewer.rs`'s `Presented::Yes { waited }`), so `present` now *returns* that verdict and each
  backend measures it where its own wait happens. The 500 µs threshold moved with it.

What is left is the shader: `make_shader` still has only a `cfg(not(windows))` stub that returns
an error, so a launch reaches the window and then stops there. Phase 2 step 4 is what unblocks it.

### 7.4 CI and the harness

Both existing jobs became a two-host matrix (`windows-latest`, `macos-latest`, `fail-fast: false`
so one host's failure cannot hide the other's): `check` with `--no-default-features` (no vendored
input, no libclang needed) and `full` gated on a restored vendor tree. The vendor cache key now
carries `runner.os`/`runner.arch`, or the arm64 `.a`s and the x64 `.lib`s would collide in one
entry and each host would restore the other's and fail to link. Per D11, CI stops at
build-and-test: it never signs, notarizes or packages a `.dmg` - that only runs on the dev Mac via
`scripts/build-mac.sh`, so the Developer ID cert and App Store Connect key never need to exist as
CI secrets.

Both legs are mandatory rather than nice-to-have: clippy on Windows never sees `render/metal.rs`,
`openfiles.rs` or `menubar.rs`, and clippy on macOS never sees `render/d3d11.rs` or the
`windows-sys` leaves, so a single-host CI cannot keep the workspace lint-clean once the second
leaf exists.

Two things the mac leg needs that the Windows one does not. **The Metal toolchain is not
guaranteed on the runner**: since Xcode 26 it is an optional ~700 MB component, present on some
images and not others, and `fire`'s build.rs needs it for *any* build including the decoder-free
one (D24). The job tests for it by **running** `xcrun -sdk macosx metal --version` - `xcrun --find
metal` succeeds either way, because what it finds without the toolchain is a stub that fails at
use (§7.1) - and downloads it only if that fails. **libclang needs no install**: macOS ships one
inside Xcode and `clang-sys` finds it unaided, so the `choco install llvm` step is Windows-only.

Neither leg populates the vendor cache (there is no `cache/save` step, and there never was); on a
GitHub-hosted runner `full` reports that it skipped unless something else has filled the cache.
That is unchanged from the Windows-only version - the split exists so a fork's first push gets a
green badge over real coverage rather than a link error.

`scripts/ttfp.ps1` has a shell twin, `scripts/ttfp.sh` - same method, same interleaving, so a
number taken with either was taken the same way. It is written for the bash macOS actually ships
(3.2): a measurement harness that only runs where extra tools are installed is not much of a
harness. `ttfp.rs`'s non-Windows arm used to initialise its `OnceLock` start instant *inside* the
measurement and so always reported ~0.000 ms; the mac arm now reads the kernel's own
process-creation time via `proc_pidinfo(PROC_PIDTBSDINFO)` - the direct equivalent of the Windows
arm's `GetProcessTimes`, and `libproc` rather than the `kinfo_proc` sysctl this document first
guessed at, because `libc` exposes `proc_bsdinfo` on Apple and has no `kinfo_proc` there. Any
remaining platform gets `f64::NAN`, which the harness rejects: "not measured here" should not look
like a result. Mac numbers compare mac builds against each other; the Windows figures in §8 are not
a baseline for them.

Measuring it immediately proved the origin matters. On a **cold** launch `process start → main` is
**514 ms of a 692 ms** TTFP - the loader, before a line of ours runs. An `Instant` taken at the top
of `main` would have reported 178 ms and hidden the whole story.

---

## 8. Phases

### Phase 1 - Windows on winit + sokol_gfx (branch `shell/winit-sokol`) - done, gate met

What was done:

1. Kept the HLSL; moved `fxc` from "compile for our own D3D11 calls" to "compile bytecode for
   sokol_gfx", and hand-wrote the shader reflection (§4).
2. Replaced the D3D11 drawing in `render/gpu.rs` with sokol_gfx; added `render/d3d11.rs` (device
   + flip-model swapchain, D19) and `render/mips.rs` (CPU mip chain on the decode worker, D21).
3. Replaced the Win32 window/wndproc with the winit `ApplicationHandler`; timers via §3.3;
   every handler behind a panic firewall, as the wndproc was.
4. Moved `ui/` to `dear-imgui-rs`; wired `dear-imgui-winit` for input and `sokol_imgui`
   (`SOKOL_IMGUI_NO_SOKOL_APP`, compiled by build.rs) for rendering; one suspended context per
   window, activated for the duration of a closure so N windows never race over ImGui's global.
5. Instance model → one process / N windows over `interprocess`; kept the foreground leaf.
6. Keybind migration to `KeyCode` + `Primary`; config dir via `dirs`; `rfd` for Browse and the
   startup error boxes.

**The gate** (`scripts/ttfp.ps1`: kernel process creation → first image-bearing present, 12
interleaved launches per cell, release, idle machine, against an instrumented `main`):

| Image | `main` (Win32 + D3D11) | `shell/winit-sokol` | Δ | Budget (D2) |
| --- | --- | --- | --- | --- |
| 38 KB PNG | 133.5 / 132.4 ms | 135.4 / 131.6 ms | +1.9 / -0.8 ms | ≤ 5 ms |
| 8.9 MB PNG | 142.6 / 144.0 ms | 142.2 / 143.3 ms | -0.4 / -0.7 ms | ≤ 10 ms |

Release phase timings: the D3D11 device ~135 ms on the bring-up thread, `sg_setup` 0.3 ms,
pipeline 1 ms; on the main thread the winit window 9 ms, the swapchain 1.6 ms, ImGui 1.7 ms.
The one thing that mattered was D18's ordering.

Also verified: two windows in one process under `open-in = "new-window"`; an idle window with an
image open at 0.0 ms CPU over 5 s; position/maximized restore, theme, launcher Run state and
dialog parenting all intact. The edge flicker seen on the sokol_app prototype (A.3) reproduced
here and was fixed in `897bc7e` (§4), which places it in the shader rather than in either
presentation path.

### Phase 2 - macOS

Ordered so each step ends somewhere verifiable. Steps 1-2 are the dev-pipeline work of §7;
nothing after them can be checked without it.

1. Set the Mac up per §7.1, then get `cargo test -p fire-ipc` and
   `cargo test -p fire-decode --no-default-features` green. That is first light: it proves the
   toolchain without needing a single line of new code. **Done** (2026-09-04): arm64 dev Mac,
   Xcode-beta 27.0 (27A5218g) as the active developer directory, rustup/cmake/ninja/meson/nasm/
   pkg-config via Homebrew, vcpkg bootstrapped - both test crates pass.
2. Native decoders (§7.2): drop the Windows-only short-circuit in `psd-sdk-sys` and exclude
   `PsdNativeFile.cpp`; build the arm64 HEIF stack with vcpkg (D25), move the vendored libs to
   per-target subdirectories, teach `heif-sys` to pick one and to link `c++`. Ends with
   `cargo test --workspace` passing everything that does not need a window. **Done**
   (2026-09-04): 109 tests green, fmt and clippy clean; headers went per-target too and the mac
   libheif needed `ENABLE_PLUGIN_LOADING=OFF` (§7.2). Not yet re-verified on Windows - the
   vendor-path move and the `cfg` gates are the parts to watch (§7.4).
3. `render/metal.rs`: a `CAMetalLayer` on the winit window, handing sokol_gfx its `MTLDevice` at
   setup and its per-frame drawable + render-pass descriptor through `sg_swapchain` - the twin
   of `render/d3d11.rs`, same shape, same rough size. `SOKOL_BACKEND=METAL` already flows
   through both the vendored sokol build and `simgui.c` (build.rs picks `SOKOL_METAL` from the
   target OS). **Done** (2026-09-04): `fire` compiles and `cargo test --workspace` passes on the
   Mac (206 tests), and a launch gets as far as the step-4 shader stub - proving the *device*
   half, since `sg_setup` succeeds on the `MTLDevice` (0.83 ms) and the device itself costs
   33 ms against D3D11's ~135 ms on Windows. The *swapchain* half is compiled but unexercised:
   nothing reaches a frame until the shader lands. Three things the plan did not say (§7.3).
4. Shader: adopt `sokol-shdc` (D4) so one source produces both DXBC and MSL plus the reflection,
   and compile the MSL to a `.metallib` with `xcrun metal` in build.rs (D24); verify the Windows
   output is unchanged before deleting the hand-written path. First `cargo run -p fire`.
   **Done** (2026-09-04): `shader.hlsl` is gone, replaced by `shader.glsl` + `render/generated/`
   (§4). **First pixel on macOS** - debug and release both present image frames through the Metal
   swapchain with no sokol validation errors, which also exercises the whole of `render/metal.rs`
   for the first time. Release phases: Metal device 49 ms (D3D11 is ~135 ms), `sg_setup` 0.9 ms,
   pipeline 0.9 ms, window 32 ms (Windows: 9 ms), swapchain 0.4 ms, ImGui 3.4 ms. The
   hand-written HLSL path was deleted per the step's own instruction, so **the Windows half is
   generated but unbuilt** - that is the outstanding verification, and `git` holds the old file.
5. Leaves: the open-file delegate hook, the `muda` menu bar, native fullscreen mapping, pinch,
   the clipboard twin. **Done** (2026-09-04). Pinch (D15) and the menu bar (D16) are in, and
   fullscreen turned out to need nothing — winit's `Borderless` is already `toggleFullScreen:` on
   macOS. Verified on screen: the menu bar reads Fire / File / Window, and an image with alpha
   renders with the checkerboard, the boundary outline and true 100 % zoom, which exercises the
   Metal shader paths a green build cannot.

   **The open hook is `crates/fire/src/openfiles.rs`** (D5/D6), and it is the macOS half of the
   single-instance story rather than a nicety: Launch Services gives a fresh launch *no arguments*
   and gives a running app *no new process*, so without it the bundle opens blank from Finder and
   a running Fire ignores every later open — neither `main`'s `argv[1]` nor the instance socket
   ever sees the file. AppKit delivers it as `application:openURLs:` on the `NSApplicationDelegate`,
   which winit owns; of the three ways in, replacing the delegate crashes (winit's
   `ApplicationDelegate::get` panics if the app's delegate is not its own class) and registering an
   `NSAppleEventManager` handler loses (`NSApplication` installs its own during `finishLaunching`,
   after anything `main` could do), so the hook adds the one missing method to winit's delegate
   class with `class_addMethod` and re-sets the delegate — `setDelegate:` caches which methods
   exist, and winit called it before the method did. Verified end to end: `open -a` on a cold Fire
   shows the image, a second `open -a` lands in the *same* process, and two files at once open
   both under the `open-in` setting.

   A launch-by-open arrives between `applicationWillFinishLaunching:` and
   `applicationDidFinishLaunching:` — before winit reports `resumed`, so before any window exists.
   Those opens are held and handed to the first window as it is created rather than sent through
   the loop afterwards, which would have been a visible blank frame and, in `new-window` mode, a
   stray empty window in front of the image.

   **"Copy File" now puts a real file on the pasteboard**: an `NSURL` written to
   `NSPasteboard.generalPasteboard` (`clipboard info` reports `«class furl»`), so ⌘V in Finder
   copies the image rather than pasting its name. Unlike Copy Path / Copy File Name this could not
   shell out to `pbcopy` — a file URL is a pasteboard *type*, not a string that starts with
   `file:`. A non-UTF-8 path or a refused write still falls back to the path as text.

   Found while testing the bundled app, and fixed: a Unix socket outlives its owner, so any crash
   left a socket file that made every later launch stall two seconds and — with no path to
   forward — exit with no window at all (§3.1). `interprocess` reports namespaced names as
   *supported* on macOS and then implements the namespace with a file in the temp directory, so
   the first fix, which computed the path itself, silently did nothing; `try_overwrite` lets the
   crate do the deleting, since it is the one that knows where the socket is.
6. Retina: `scale_factor` into fit/1:1; verify the zoom-snap ladder lands on true 100 %.
   **Done** (2026-09-04), and the premise turned out to be half wrong: fit and 1:1 needed *no*
   `scale_factor` at all. `Viewport`, the zoom factor, the cursor and the image rect are already
   in physical px throughout, so `zoom = 1.0` is one texel per physical pixel by construction —
   which is exactly what D15 asks for. Measured rather than assumed: a synthetic 400×260 ruler
   image opened at actual size occupies **exactly 400 × 260 pixels** in a 5120×2880 screen
   capture, its 1 px border resolves to exactly one row of 400 red pixels, and its 8 px marker
   grid comes back at a spacing of exactly 8 with all 1568 markers present — no resampling
   anywhere on a 2× display. The chrome measures exact too: `status_h = 24` renders as 48 physical
   px and `toolbar_h = 38` as 76, so `Metrics::new(dpi)` is right.

   Where the scale factor *was* missing is the **gesture** math, which is the opposite case — it
   describes how far the hand moves, so it must not be in physical px. The scrubby-zoom
   sensitivity and the configured `zoom-snap` detent width were being compared against physical
   drag pixels, which on any Retina display made the zoom-drag twice as fast and the detent half
   as wide as configured; the double-click slop (4 px, a Windows `SM_CXDOUBLECLK` value, i.e.
   logical) was likewise halved. Both now convert through the window's backing scale, which leaves
   a 1× display — every Windows box at 100 % — behaving exactly as before.

   Also fixed: the Metal layer's **`contentsScale` was set once at creation and never updated**.
   `resize` alone is not enough, because Core Animation uses `contentsScale` to map the layer's
   point bounds onto the drawable's pixels; a window dragged from a Retina display to a 1× one
   would have had its drawable rescaled to fit and gone soft. The backend contract gained
   `set_scale_factor`, empty on D3D11 (DXGI has no scale between the swapchain and the window).
   **Not verified on hardware** — it needs a second display with a different backing scale.

   The zoom-snap ladder's exactness is a unit test, not a new finding: `snap_step` returns the
   ladder value verbatim rather than an `exp(ln(x))` round-trip of it, and
   `a_caught_snap_is_the_ladder_value_verbatim` asserts `assert_eq!` on every rung including 1.0.
   The config states the ladder in percent, so the 100 rung is `100.0 / 100.0` — exactly 1.0.
7. Measure: the mac twin of `scripts/ttfp.ps1` (§7.4) and a launch-path breakdown, so the Metal
   bring-up gets the same scrutiny the D3D11 one did. There is no cross-OS budget - the number
   to beat is the next mac build's. **Done** (2026-09-04).

   **The measurement found a 2-second bug before it could measure anything.** ⌘Q is AppKit's
   `terminate:`, which ends in `exit()`: `main` never returns, so the instance socket's `Listener`
   is never dropped and the socket *file* outlives its owner. The next launch then found the name
   taken, spent the whole 2 s connect timeout discovering nobody answers, and only then reclaimed
   it. Measured: **168 ms → 2196 ms on every launch after a normal quit**, reproducible on the
   `terminate:` path and not a corner case at all. Two fixes, both verified:
   * The owner - and only the owner, since a forwarding launch would be deleting someone else's -
     registers an `atexit` `unlink` of its socket (`ipc_server::unlink_on_exit`). That covers ⌘Q, a
     plain `main` return, and the TTFP stamp's own `exit(0)`. Eight back-to-back launches now leave
     no socket behind and hold ~170 ms.
   * The connect retry budget was one number for two unrelated failures. "The name is not there
     yet" (`NotFound` / `ERROR_PIPE_BUSY`) still gets the full 2 s; "the name is there and refuses"
     (`ConnectionRefused`) - a socket file whose owner is gone, which nothing will ever start
     answering - gets 150 ms, enough to cover the window between a live owner's `bind` and its
     `listen` and no more. `SIGKILL` recovery, where no `atexit` can run, went **2196 ms → 320 ms**.

   Making the socket's location knowable was the enabling change: `GenericNamespaced::is_supported()`
   answers "can I pass a bare name", not "does the kernel own the name", and on macOS it says yes
   and then emulates the namespace with a file in `/tmp`. macOS now takes the explicit-path branch,
   which also moves the socket out of a world-writable shared directory into the user's own - two
   people on one Mac were sharing `/tmp/fire.sock`.

   **Baseline** (`scripts/ttfp.sh`, release, 8 interleaved launches per cell, the same binary as
   both A and B so the columns also measure the harness's own noise):

   | Image | median | mean | sd | harness noise (A vs B, same binary) |
   | --- | --- | --- | --- | --- |
   | 130 KB JPEG (1280×853) | 170.8 ms | 172.7 ms | 8.9-11.6 | 0.0 ms |
   | 27.5 MB PNG (4096×4096) | 224.5 ms | 222.0 ms | 3.7-5.0 | 6.0 ms |

   **Warm launch-path breakdown** (`FIRE_TIMING=1`, ~172 ms total):

   | Phase | ms | |
   | --- | --- | --- |
   | process start → `main` | ~11 | loader + runtime; nothing of ours |
   | `main` → first `resumed` | ~75 | AppKit `finishLaunching` + activation, inside `run_app` |
   | window creation | ~31 | |
   | swapchain / ImGui | 0.3 / 2.2 | |
   | **GPU join wait** | **0.01** | the 34 ms Metal device is *entirely* hidden behind the window - D18 doing exactly what it was for |
   | first frame + vsync | ~52 | the remainder |

   Two things worth carrying into any later optimisation. The largest single item is **AppKit's own
   launch (~75 ms), which is not ours to remove**, and the second is window creation (~31 ms) -
   which together are why the mac number sits where it does. And the first launch after a build is
   an outlier well beyond the loader cost (pipeline 58 ms vs 0.9 ms warm, ImGui 9.8 vs 2.2), so a
   warm-up launch before measuring is not optional.
8. CI (§7.4): the `macos-latest` matrix leg, with the vendor cache keyed per target.
   **Done** (2026-09-04), and one assumption in this document was wrong.

   `cargo clippy --workspace -- -D warnings` failed on the *vendored* `sokol` tree
   (`vendor/sokol-rust/build.rs:93`, upstream's own style, on both hosts). The fix was supposed to
   be CI scoping, but **`--exclude sokol` does not work**: cargo only caps lints on crates it does
   not consider local, and *every* path dependency is local, member or not - so the vendored build
   script is linted no matter how the command line is scoped. What does work is
   `exclude = ["vendor/sokol-rust"]` in the root manifest. `sokol` sat inside the workspace
   directory and cargo makes any path dependency there a member automatically; saying it is not
   one takes it out of `--workspace` while leaving it built exactly as before, and needs no patch
   to the pinned tree (which was the thing to avoid). Verified: `cargo clippy --workspace
   --all-targets -- -D warnings` exits 0.

   Every command the workflow runs was run on this Mac first - both clippy invocations, all four
   test steps and the release build, all green. **The workflow file itself is unverified** until it
   runs on GitHub: whether `macos-latest` ships the Metal toolchain is the one thing that cannot be
   checked from here, which is why the job tests for it and downloads it rather than assuming.
9. `build-mac.sh`: `.app` layout, `Info.plist` from `product.json` (every extension from the
   installer's list, `LSHandlerRank = Alternate`, `CFBundleIconFile`), `codesign --options
   runtime --timestamp`, `notarytool submit --wait`, `stapler`, `hdiutil` → `dist/Fire-<ver>.dmg`;
   run by hand on the dev Mac, whose keychain holds the Developer ID cert (D11) - no CI job
   invokes it. **Written and exercised** (2026-09-04), with one part that could not be run here.

   The document types are **read out of `SUPPORTED_EXTENSIONS` in `fire-decode`** rather than
   copied from the installer's list. That table is *the* one (§ the const's own doc), and
   `installer/fire.iss` only keeps a second copy because an Inno Setup script can import nothing -
   which is why a test has to police it. Parsing the const here means the plist needs no such
   test, and the script fails loudly if the parse ever stops finding a plausible table rather than
   shipping a bundle Finder never offers. Verified: 54 extensions in, and LaunchServices resolves
   them to **48 claimed UTIs** - the real ones (`public.png`, `com.ilm.openexr-image`,
   `com.adobe.photoshop-image`, every camera-raw UTI) plus dynamic ones for the formats macOS has
   no UTI for (`.qoi`, `.ff`, `.x3f`, `.kdc`, `.mef`, `.pnm`, `.jfif`).

   One entry rather than one per format, because with `LSHandlerRank = Alternate` we do not own
   the UTI and the per-type name never surfaces - grouping would buy nothing and cost a second
   hand-maintained list. `Alternate` is the point: Fire volunteers for these files and shows up in
   "Open With" without taking `.png` away from Preview on install.

   **What was verified**: the `.icns` built from the 1024² master with `sips` + `iconutil`; the
   bundle layout and a `plutil -lint`-clean plist; `codesign --force --options runtime
   --timestamp` producing `flags=0x10000(runtime)`, a secure timestamp and `Mach-O thin (arm64)`,
   passing `--verify --deep --strict`; the hardened bundle launching and opening an image; the
   `.dmg` mounting with the app and the `/Applications` symlink; and the auto-detect refusing,
   with instructions, when no Developer ID certificate is present.

   **The full release path ran end to end** (2026-09-05), once the Developer ID Application
   certificate and a `notarytool` keychain profile were in place. Both submissions came back
   `Accepted` first try, both artifacts stapled and validated, and the check that actually matters
   passes on the app, on the `.dmg`, and on the app *inside* the mounted `.dmg`:

   ```
   accepted
   source=Notarized Developer ID
   origin=Developer ID Application: Chandan Singh (48QFANT8RD)
   ```

   Signature: hardened runtime (`flags=0x10000(runtime)`), secure timestamp, `TeamIdentifier`
   set, `Mach-O thin (arm64)`. The stapled ticket means it opens on a Mac that is offline.

   Neither credential is ever passed on a command line: `scripts/build-mac.sh` with **no
   arguments** is the shipping build, taking the identity and the notary profile from the
   keychain. `--no-notarize` and `--no-sign` step down from there for iteration, and say plainly
   that what they produce is not shippable - including that macOS 15 removed the Control-click
   bypass, so an un-notarized build now needs System Settings → Privacy & Security → Open Anyway.
   A failed submission prints the `notarytool log <id>` command rather than dying on a bare exit
   status.

   Also worth knowing while testing: the shipping bundle (`com.psmyles.fire`) and the dev one
   (`com.psmyles.fire.dev`) **share the instance socket**, because its name belongs to the product
   rather than to the bundle. Launching one while the other runs forwards the open to whichever
   got there first.
10. Hand the dmg to the colleagues; the first thing to test is Finder double-click on an
    already-running Fire (the Apple-Event path) and drag onto the Dock icon.

### Phase 3 - cleanup

The Win32 shell code and the old D3D11 drawing path are already gone; what remains is
documentation. Update `architecture.md` §2-5, §8-9, §12-13 to describe the shared shell, fold
this document's decisions into it, and sweep the stale references the migration left behind
(e.g. `window_state.rs` still cites `crate::win` and `GetWindowPlacement` for a rectangle winit
now reports).

---

## 9. Risks

- **`sokol_imgui` / `cimgui` ABI lockstep (D22)** - `simgui.c` and `dear-imgui-sys` compile the
  same ImGui structs in two translation units. The five cimgui defines and the `SOKOL_*` backend
  must match exactly; a mismatch is not a link error, it is a silent layout difference. Bump the
  `dear-imgui-*` crates and the vendored `simgui/cimgui.h` as one change.
- **The vendored sokol tree** - `vendor/sokol-rust` is a pinned checkout, not a registry
  dependency; updating it is manual, and it is where `sg_swapchain` / `ShaderDesc` field changes
  would land. The `ShaderDesc` written by hand in `make_shader` is the part most exposed to that
  - and the part sokol-shdc would generate (D4).
- **The Metal half is unproven** - `render/metal.rs`, the MSL shader and the sokol_gfx Metal
  backend have not been built. The shape mirrors a working Windows module, but the first Mac
  build is where D1's "one drawing API" claim is actually tested.
- **Event-driven invariant** - ImGui's backends want to redraw forever and winit makes `Poll`
  easy. `request_redraw` must stay tied to input / decode / timer; check idle CPU as before
  (target: the file watcher and nothing else). Measured at 0.0 ms today; keep measuring.
- **Shader branches and derivatives** - the edge-flicker class of bug (§4). Any new per-pixel
  branch around a sample is a regression waiting to happen; the rule is in the shader's header.
- **Finder open semantics** - files arrive *after* launch as Apple Events, sometimes several
  at once, sometimes before the first window exists. The open path must tolerate "no window
  yet" and a batch.
- **The toolchain floor is full Xcode (D24)** - not Command Line Tools, and on Xcode 16+ the
  Metal toolchain is a further 839 MB download. Every dev machine and the CI runner pay it. The
  failure mode is worse than "absent": `xcrun --find metal` *succeeds* against a stub, and
  sokol-shdc asked for bytecode without the real toolchain emits source and exits 0, so the
  build silently degrades into runtime shader compilation - the cost D4 exists to avoid. §7.1
  has the check that actually detects it. If the floor becomes intolerable, that same degraded
  mode is the deliberate fallback.
- **The vendored native trees are now per-target (D25)** - two sets of artifacts under one
  `vendor/`, produced by two runs of the same recipe, cached by CI under keys that must not
  collide. The failure mode is a silently stale or wrong-arch lib, which surfaces as a link
  error at best and a mismatched ABI at worst. Keep `VENDOR.txt` the single recipe for both.
  The sharpest instance is already known: with libheif's default `ENABLE_PLUGIN_LOADING=ON` the
  mac build links cleanly and drops dav1d on the floor (§7.2), so re-vendoring must re-run
  `nm -u lib/libheif.a | grep -c dav1d` rather than trust a green build.
- **Notarization** - hardened runtime + a statically linked LGPL libheif is fine, but the
  first submission will find an entitlement or signing gap; budget an afternoon.
- **Single-process crash exposure (D7)** - accepted; if it bites, the decode-subprocess
  design is the fallback and `fire-decode`'s uniform interface makes it a bounded change.

---

## Appendix A - how the shell was chosen

Three shells were built and measured against the D2 gate before this one was adopted. The
write-ups below are kept **as written at the time**, so their cross-references point at the
pre-revision numbering of this document: A.1 was §7.2, A.2 was §7.3, A.3 was §7.4 and A.4 was
§7.5. **A `§7.x` inside this appendix means the old Phases section, not the current §7
(Development environment).** The original D4 (hand-written WGSL) and D14 / D17 (the wgpu launch
path and adapter selection) belong to A.1-A.2 and were superseded with that branch.

### A.1 - GPU init cost on Windows (the known risk, as assessed before Phase 1)

`wgpu::Instance::request_adapter` has historically enumerated every adapter and created a
D3D12 device per adapter to query it - reported at up to hundreds of ms on multi-GPU
machines. That would not hide under a ~140 ms decode. Per D17, the first thing Phase 1 does
after step 2 is **measure it** on the dev machine (single GPU, and if possible on a
laptop with iGPU + dGPU).

If it is not negligible, the prepared fix is a `cfg(windows)` leaf, `gpu/adapter_win.rs`:
create `IDXGIFactory6`, pick one `IDXGIAdapter` (the adapter owning the window's monitor,
via `MonitorFromWindow` → `EnumOutputs`; or simply `EnumAdapters1(0)`), expose it through
`wgpu_hal::dx12` and hand it to `Instance::create_adapter_from_hal` - one device created,
nothing enumerated. Fallback on device-creation failure: `EnumWarpAdapter` through the same
path, never `request_adapter`. Keep a debug flag `--wgpu-request-adapter` that uses the
portable path, so "my leaf or wgpu" is one relaunch to bisect. The hal `expose` signature has
moved between wgpu releases - it is the one thing in this plan tied to the pinned version.

Independent of the adapter question: pin `Backends::DX12` (Windows) / `Backends::METAL`
(macOS), `InstanceFlags::empty()` in release, and start the decode worker *before* touching
wgpu so device creation overlaps decode.

### A.2 - Phase 1 result (2026-09-04) - the gate is not met

Phase 1 was built on branch `shell/wgpu` (worktree `D:\Dev\fire-wgpu`, wgpu 30.0.1, winit
0.30.13, dear-imgui-* 0.17.0; 94 tests, clippy clean). Measured with `scripts/ttfp.ps1`
(kernel process creation → first image-bearing present, 12 interleaved launches per cell,
idle machine, RTX 4080):

| Image | `main` (D3D11) | `shell/wgpu` | Δ | Budget (D2) |
| --- | --- | --- | --- | --- |
| 38 KB PNG | 126–134 ms | 264–277 ms | **+138–143 ms** | ≤ 5 ms |
| 8.9 MB PNG | 144–145 ms | 278 ms | **+133 ms** | ≤ 10 ms |

Where it goes (release, `FIRE_TIMING`): instance ~20 ms, `request_adapter` ~140 ms,
`request_device` ~31 ms, viewport pipeline (naga → HLSL → FXC → PSO) ~32 ms - about 235 ms of
GPU bring-up. Window ~28 ms, ImGui ~7 ms, surface ~5 ms. Bringing the GPU up on a worker thread
from the first line of `main` changes nothing (join wait ~230 ms): the bring-up *is* the critical
path, and nothing on the main thread is long enough to hide it under.

Two findings against §7.2:

- The cost is not per-adapter enumeration. DXGI lists three adapters here (the RTX 4080 twice,
  plus WARP), but a *warm* second enumeration takes ~1 ms; the ~140 ms is the driver's one-time
  D3D12 initialisation, which any first device creation pays. A one-adapter leaf would save little.
- The leaf cannot be built against wgpu-hal 30 anyway: `dx12::Adapter::expose` is `pub(super)`
  and needs the hal instance's private fields, so it would require a wgpu-hal fork.

Conclusion: on this machine D3D11 reaches first pixel ~100 ms sooner than D3D12-through-wgpu,
and that is an API/driver cost, not something Fire can engineer around. Per §7 step 7 the
branch does not merge as-is and this document needs revising. The winit shell, the ImGui
backends, the timer heap, the `interprocess` instance model and the `KeyCode` keybinds are
independent of wgpu and carry over to whichever revision is chosen (e.g. one winit shell with
D3D11 on Windows and wgpu/Metal on macOS behind a small GPU trait - the "two backends" cost D2
tried to avoid, but paid only in the render module, not in the shell).

### A.3 - Phase 1, second attempt: sokol (2026-09-04) - the gate is met

Branch `shell/sokol` (worktree `D:\Dev\fire-sokol`, commit 1a5d398, on top of `shell/wgpu`) replaces
the whole shell with sokol: sokol_app (window, D3D11/Metal device + swapchain, frame loop, input),
sokol_gfx (one drawing API), sokol_imgui (Dear ImGui platform + renderer backend, compiled in
build.rs against the cimgui that dear-imgui-sys links). sokol-rust is vendored at `vendor/sokol-rust`.

Gate (release, n=12, interleaved, idle machine, two runs):

| Image | `main` (D3D11) median | `shell/sokol` median | delta |
| --- | --- | --- | --- |
| T_fx_Rounded_Spark_01.png (38 KB) | 146.5 / 148.7 ms | 146.4 / 145.0 ms | 0 / -3.7 ms |
| T_fx_Waterfall_Bubbles_D_Looping_8x8_FB.png (8.9 MB) | 156.0 / 157.1 ms | 155.6 / 154.4 ms | -0.4 / -2.7 ms |

Budget was <= 5 ms / <= 10 ms regression; measured delta is within noise (sd 4-8 ms). Release
phase timings on the big image: window + D3D11 device (sokol_app) ~127 ms since main, sg_setup
0.2 ms, pipeline 1.3 ms, ImGui 1.8 ms, CPU mip chain 5 ms (on the decode worker, off the UI
thread). `main`'s absolute numbers are ~15 ms higher than in 7.3 (machine state on the day); the
interleaved A/B is what the gate compares. The sokol stamp sits just before sokol_app's Present
where main's sits after it; on a not-yet-full flip-model swapchain that is a sub-millisecond bias.

What the sokol shell gives up (no API in sokol_app): one window per process (`open-in =
"new-window"` no longer binds the instance socket; `reuse-window` still forwards), a continuous
frame loop (one frame per vsync - the event-driven "idle costs ~0" invariant does not hold),
window position / maximized state, the launcher's Run setting, the light theme (no system theme
query), raising the window on a forwarded open, a parent for the file dialogs. The mip chain is
CPU-built: sokol_gfx has no GenerateMips and forbids rendering into an image that carries data.

Decision needed: adopt sokol as the shell (and revise sections 2-6 accordingly), accepting the
list above, or keep D3D11 on Windows behind a GPU trait with wgpu on macOS only.

### A.4 - Phase 1, third attempt: winit + sokol_gfx (2026-09-04) - the gate is met, the window model is back

sokol_app's window model (one window per process, a continuous frame loop, no position/maximized
restore, no theme query, no raise, parentless dialogs) was ruled a dealbreaker, so the shell was
recombined: **winit** owns the window, the event loop and input (everything §7.3's branch had),
**sokol_gfx** stays the one drawing API, and **sokol_imgui** stays the ImGui renderer, compiled in
its `SOKOL_IMGUI_NO_SOKOL_APP` mode with `dear-imgui-winit` feeding it input. The price is the
swapchain glue: `render/d3d11.rs` (~230 lines) creates the D3D11 device and the flip-model
swapchain - the same ones the Win32 shell used - and hands them to sokol_gfx through
`sg_environment` / `sg_swapchain`. The macOS twin is a `CAMetalLayer` on the winit window.

Branch `shell/winit-sokol` (worktree `D:\Dev\fire-winit-sokol`, commit 9b9fac3, on top of
`shell/wgpu`; 97 tests, clippy and fmt clean, release exe 13.6 MB). Measured with
`scripts/ttfp.ps1` (12 interleaved launches per cell) against the instrumented `main`:

| Image | `main` (D3D11) | `shell/winit-sokol` | Δ | Budget (D2) |
| --- | --- | --- | --- | --- |
| 38 KB PNG | 133.5 / 132.4 ms | 135.4 / 131.6 ms | +1.9 / -0.8 ms | ≤ 5 ms |
| 8.9 MB PNG | 142.6 / 144.0 ms | 142.2 / 143.3 ms | -0.4 / -0.7 ms | ≤ 10 ms |

(Two runs; a first run before the fix below read +8 ms on the small image.) Release phases: the
D3D11 device ~135 ms on the bring-up thread, `sg_setup` 0.3 ms, pipeline 1 ms; on the main
thread the winit window 9 ms, the swapchain 1.6 ms, ImGui 1.7 ms. The one thing that mattered:
the window must be created *before* the main thread joins the bring-up thread, or its 9-13 ms
serialize after the device - `Viewer::new` now takes the GPU as a closure and joins it only once
the window exists.

Verified on the branch: two windows in one process under `open-in = "new-window"`; an idle window
with an image open uses 0.0 ms of CPU over 5 s (event-driven loop); position/maximized restore,
theme, launcher Run state and dialog parenting are the §7.3 shell's code, unchanged.

Open: the sokol_app build (§7.4) showed a flickering 1 px line on all four image edges on a 4K
150 % monitor, windowed and maximized, that GDI screen capture could not reproduce; not the CPU
mip chain (every row written) and not a swapchain/client size mismatch (measured equal). Whether
it survives this build tells whether it lives in sokol_gfx's rendering or in sokol_app's
presentation - to be checked by eye on `D:\Dev\fire-winit-sokol\target\release\fire.exe`.

Decision: this is the shape to adopt. §2-6 should be revised to say winit + sokol_gfx +
sokol_imgui with shell-owned swapchain glue per OS, and §7.3's "two backends" worry is moot: the
per-OS code is the ~200-line device/swapchain module, not a renderer.

**Repo state after the decision (2026-09-04).** Only `main` and `shell/winit-sokol` remain. The wgpu and sokol_app worktrees and branches were deleted: §7.3's commits survive as ancestors of `shell/winit-sokol`, §7.4's prototype (1a5d398) does not, and this document is all that is left of it. The instrumented `main` TTFP baseline worktree went with them; rebuilding it is a detached worktree at `main`, `crates/fire/src/ttfp.rs` copied from the branch, `mod ttfp;` in `main.rs` and one `stamp_first_pixel()` call in `GpuSurface::present`.

**Resolved since:** the edge flicker was the shader, not either presentation path - implicit
derivatives on a sample inside a per-pixel branch, fixed with `SampleGrad` in `897bc7e` (§4).
