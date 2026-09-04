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
| D4 | **Shader is precompiled to bytecode on both OSes** - HLSL → DXBC by `fxc` today; for macOS, **adopt `sokol-shdc`**: one annotated-GLSL source generating HLSL + MSL *and* the `ShaderDesc` reflection, with the MSL compiled to a `.metallib` by `xcrun metal` (D24) | Nothing on the cold-start path (no runtime shader compile) on either OS, and a broken shader is a build error; one source beats two hand-kept-in-sync twins once a second backend exists | `fxc` + the Windows SDK on Windows, the Metal toolchain on macOS (D24). Phase 2 rewrites the shader in sokol-shdc's dialect and replaces the hand-written `ShaderDesc` in `render::gpu::make_shader` with generated code | Windows shipped; sokol-shdc planned |
| D5 | **One process, N windows** everywhere (instance mode is `open-in = new-window \| reuse-window`) | Finder never launches a second process - it sends an open-file event to the running app - so per-launch processes have no mac equivalent; winit runs N windows in one loop cleanly | Windows NewWindow users get the same UX from one process; crash isolation is per-process (D7) | Shipped |
| D6 | **IPC via the `interprocess` crate** (named pipe / Unix socket behind one API) on both OSes | One forward path, one test | macOS also needs the Apple-Event hook (§5) feeding the same open path | Shipped (mac hook planned) |
| D7 | **Accept single-process crash exposure** | FFI already runs under `catch_unwind` on a worker with validated inputs; a viewer has no unsaved state | A true segfault in libheif/psd_sdk closes every window, not one | Accepted |
| D8 | **Timers: `ControlFlow::WaitUntil` + a deadline min-heap** | Preserves the event-driven invariant (no input, no timer → no frame) with zero threads | Small scheduler in the app; every timer (GIF, flipbook, caret) goes through it | Shipped |
| D9 | **Keybinds: physical `KeyCode` by name + a `Primary` modifier** (Ctrl on Windows, ⌘ on macOS) | Layout-independent, one `config.toml` works on both | One-time migration of existing VK-code chords; `Ctrl+`/`Cmd+` still parse as `Primary+` | Shipped |
| D10 | **macOS: Apple Silicon only, vendored arm64 static libs** for libheif/libde265/dav1d + `cc`-built psd_sdk | Same vendoring model as the Windows `.lib`s; no Intel users to serve | Re-run `VENDOR.txt` on a Mac; universal deferred | Planned |
| D11 | **Build, sign and notarize only on the dev Mac, via `scripts/build-mac.sh`** - CI never builds a mac artifact | Keeps the Developer ID cert and App Store Connect key off CI entirely, on a public repo; CI's mac leg (D23) stays lint/test only | Releases are a manual step on the dev Mac rather than a CI-triggered build | Planned |
| D12 | **Packaging: `scripts/build-mac.sh` + `Info.plist` template from `product.json`, signed + notarized `.dmg`** | Mirrors `build-installer.ps1`; plist is hand-tuned anyway (cargo-bundle would hide it) | `.dmg` is more script than `.zip`; accepted for polish | Planned |
| D13 | **Windows first, then macOS** | The shared code and the TTFP risk were the Windows migration; mac is leaves + packaging | Colleagues wait one extra phase | Done |
| D14 | *(wgpu-era: pinned backend, no debug layers, decode kicked off before device creation)* | - | - | Superseded (A.2); the surviving idea is D18 |
| D15 | **Pinch-to-zoom mapped to the wheel zoom; 1:1 = one texel per *physical* pixel** | Crisp on Retina, matches what artists mean by 100 %, zoom-snap ladder stays in image space | `scale_factor` enters the fit/1:1 math | 1:1-in-physical-px shipped; pinch planned |
| D16 | **Minimal macOS menu bar via `muda`; F11 / Ctrl-Cmd-F → winit native fullscreen** | A Mac app without a menu bar can't Cmd-Q and reads as broken; native fullscreen gives the space transition | ~40 lines, all `cfg(target_os = "macos")` | Planned |
| D17 | *(wgpu-era: measure `request_adapter`, then decide on a DXGI hal leaf)* | - | - | Superseded (A.2): the ~140 ms was D3D12 driver init, not enumeration |
| D18 | **GPU bring-up on its own thread, started on the first line of `main`; the window is created *before* the join** | Device creation is the longest single item on the launch path and needs no window - but neither does the window need to wait for it | `Viewer::new` takes the GPU as a closure; get the order wrong and the window's 9-13 ms serialize after the device (§8) | Shipped |
| D19 | **The shell owns the device and the swapchain; sokol_gfx is handed them** (`sg_environment` / `sg_swapchain`) | sokol_app's window model was a dealbreaker (A.3); this keeps winit's window *and* sokol's one drawing API | ~230 lines per OS - the only GPU-API-specific code left | Windows shipped; Metal planned |
| D20 | **The swapchain backbuffer is plain `R8G8B8A8_UNORM`; the pixel shader sRGB-encodes its own output** | Flip-model swapchains disallow `*_SRGB` formats, and ImGui's colors are already sRGB, so a single UNORM target is correct for both passes | The old two-RTV (`UNORM` + `UNORM_SRGB` view) trick is gone; the shader owns the encode and must not be "fixed" into a linear write | Shipped |
| D21 | **The mip chain is built on the CPU, on the decode worker** | sokol_gfx has no `GenerateMips`, and its rules forbid rendering into an image created with data | ~5 ms on an 8.9 MB image, off the UI thread; the upload is one `sg_make_image` carrying every level | Shipped |
| D22 | **`sokol-rust` is vendored (`vendor/sokol-rust`); `sokol_imgui.h` is compiled by `fire`'s build.rs** | The crates.io `sokol` name belongs to an unrelated 2019 crate; sokol_imgui must be compiled with the *same* cimgui defines and the same `SOKOL_*` backend as its neighbours or the struct layouts differ | A vendored tree to update by hand; three sets of defines (backend, `SOKOL_IMGUI_NO_SOKOL_APP`, the cimgui five) that must stay in lockstep | Shipped |
| D23 | **The whole dev pipeline runs on macOS**, not just the app: clippy, `cargo test`, the native decoders, the TTFP harness, the release build and packaging | A platform you cannot lint, test or measure on is a platform you cannot maintain; the alternative is mac fixes that only Windows CI can verify | The two sys crates lose their Windows-only short-circuit, the vendor layout goes per-target, CI grows a mac leg, `ttfp.ps1` gets a portable twin (§7) | Planned |
| D24 | **Metal shaders are precompiled to a `.metallib` with `xcrun metal`** - so the toolchain floor on macOS is **full Xcode** (plus the separately-downloaded Metal toolchain on Xcode 16+), not Command Line Tools | Keeps D4's "no shader compile on the cold-start path" on both OSes; the wgpu branch lost ~32 ms to exactly this (A.2), and TTFP is the project's primary metric | Every dev machine and the CI runner need Xcode, not CLT; a second offline compile step in build.rs | Planned |
| D25 | **The arm64 HEIF stack is built with vcpkg (`arm64-osx` static), mirroring `VENDOR.txt`**, and the vendored libs move to per-target subdirectories | One vendoring story on both OSes, one dav1d port patch, a self-contained `.app` with no dylib embedding or per-dylib signing | A one-time `brew install cmake ninja meson nasm pkg-config` + vcpkg bootstrap on the Mac; `heif-sys`'s hardcoded `lib/` path and `.lib` names become target-aware | Planned |

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

Pinch (`WindowEvent::PinchGesture`, macOS) will drive the same about-cursor zoom as the wheel
(D15, planned).

---

## 4. Shader

`render/shader.hlsl` survives the migration. `build.rs` compiles each entry point with `fxc`
(`vs_5_0` / `ps_5_0`) to a `.dxbc` in `OUT_DIR`, which `render::gpu` embeds via `include_bytes!`
and hands to sokol_gfx as bytecode: no runtime shader compilation, nothing on the cold-start
path, and a broken shader is a build error rather than a launch-time failure.

What sokol_gfx cannot reflect out of DXBC is written by hand beside it in
`render::gpu::make_shader`: the one 128-byte uniform block at `b0` (fragment stage), the texture
at `t0`, the anisotropic sampler at `s0`, the point sampler at `s1`, and which sampler pairs
with the texture. The stages are the same and in the same order as before: sample (point when
magnifying, aniso+mip when minifying, chosen by the two samplers) → HDR exposure/tonemap (float
formats only) → channel isolation → checkerboard composite. `Rgba16Unorm` still does its
sRGB→linear in the shader.

**One rule the shader must keep:** never `Sample` inside a per-pixel branch. The letterbox and
outline tests above the sampling are branches, and an implicitly-derived LOD inside a branch is
undefined where the quad diverges - which is what produced a flickering 1 px line on all four
image edges. The minify path uses `SampleGrad` with explicit derivatives; the fix is commit
`897bc7e` and the flicker is gone.

**Phase 2 (D4):** the Metal build needs an MSL twin, and two hand-written shaders kept in sync
is exactly the duplication this plan exists to avoid. So the shader moves to `sokol-shdc` -
sokol's own offline compiler, which takes one annotated-GLSL source and emits HLSL *and* MSL
(compiled per backend) plus the generated `ShaderDesc`, replacing both the `fxc` step and the
hand-written reflection above. It is a prebuilt binary (`floooh/sokol-tools-bin`), so it is
vendored or fetched like any other build tool and its output is checked in, keeping a plain
`cargo build` free of it. The port is a one-time rewrite of ~220 lines of HLSL; the acceptance
test is that the Windows output is pixel-identical.

---

## 5. Platform leaves (the complete inventory)

Everything below is the platform-specific code that remains. Each is one small module or a
`cfg` block; nothing else in `fire` should mention an OS - `platform.rs` says so in its header,
and this table is what it is pointing at.

| Concern | Windows | macOS | Shared via |
| --- | --- | --- | --- |
| Window, loop, DPI, DnD, theme change, fullscreen, placement | - | - | `winit` |
| GPU device + swapchain | `render/d3d11.rs` (~230 lines): D3D11 device + DXGI flip-model swapchain | `render/metal.rs` (planned): a `CAMetalLayer` on the winit window, handing sokol_gfx its `MTLDevice` and per-frame drawable | `sokol_gfx` above them |
| Shader bytecode | HLSL → DXBC (`fxc`, build.rs) | MSL → `.metallib` (`xcrun metal`, build.rs; planned - D4, D24) | one `sokol-shdc` source once it lands |
| Config dir | `%APPDATA%\fire` | `~/Library/Application Support/fire` | `dirs` |
| Dark mode | - | - | `winit` `Window::theme()` / `ThemeChanged` (registry read removed) |
| Open-file dialog + startup error boxes | - | - | `rfd` |
| Hot-reload watch | `ReadDirectoryChangesW` | FSEvents | `notify` (unchanged) |
| IPC transport | named pipe | Unix socket file (runtime dir) | `interprocess` |
| Foreground handoff on forward | `AllowSetForegroundWindow` leaf | not needed | - |
| Launcher "Run" show state | `GetStartupInfoW` leaf | no equivalent (returns `None`) | `platform.rs` |
| Clipboard (Copy File / Path / Name) | `CF_HDROP` + text leaf | planned | `platform.rs` |
| Show in Explorer / Reveal in Finder | leaf | leaf | `platform.rs` |
| Open-file events from the OS | argv | `application:openFiles:` delegate via `objc2-app-kit` (~50 lines, planned) | both call the same open path |
| Menu bar | none | `muda` minimal (App / File / Window) dispatching `Action`s (planned) | - |
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

**Planned for macOS.** `muda` (menu bar), `objc2` + `objc2-app-kit` (the open-file delegate
hook), and whatever `render/metal.rs` needs for a `CAMetalLayer` (`objc2-quartz-core` /
`objc2-metal`).

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
  `stapler` for D12 and Instruments for launch-path profiling. Verify with
  `xcrun --find metal notarytool stapler` before believing an install is complete.
* **rustup**, stable channel. `rust-toolchain.toml` currently pins
  `targets = ["x86_64-pc-windows-msvc"]`, which makes a Mac download a Windows std it will never
  use; make the list host-conditional or add `aarch64-apple-darwin` beside it.
* **Homebrew: `cmake ninja meson nasm pkg-config`**, and a bootstrapped **vcpkg** - for the
  one-time HEIF build of D25 only. A normal `cargo build` needs none of them.

Everything else, clang and the macOS SDK already provide: bindgen's libclang (the sys crates'
"check that `libclang.dll` is on PATH" message is Windows-shaped, but it is the same dylib),
`lcms2`'s static Little-CMS, `dear-imgui-sys`'s cimgui, and the vendored sokol tree - which
compiles itself as Objective-C and links `Cocoa` / `QuartzCore` / `Metal` on its own
(`vendor/sokol-rust/build.rs`), so the Metal backend costs nothing at the tooling level.

### 7.2 The Windows-only short-circuits

`heif-sys` and `psd-sdk-sys` both write a `compile_error!` stub and return early when
`CARGO_CFG_TARGET_OS != "windows"`, so `fire`'s default features cannot build on a Mac at all
until they are ported. They are very different amounts of work:

* **`psd-sdk-sys` is nearly free.** The vendored psd_sdk is already clang-aware (`PsdPch.h` sets
  `PSD_USE_CLANG`; `PsdPlatform.h` gates `<windows.h>` on `_WIN32`), and `wrapper.cpp` reads
  through its own `MemoryFile : psd::File`, so `NativeFile` is never instantiated. The mac build
  drops the short-circuit, excludes `PsdNativeFile.cpp` (Win32 `CreateFileW` + overlapped IO)
  alongside the `_Linux` / `_Mac` files already excluded, keeps `-std=c++17`, and links `c++`.
  No new tooling.
* **`heif-sys` needs the libs built first (D25).** `vendor/lib/` holds `heif.lib`,
  `libde265.lib` and `dav1d.lib` - MSVC x64, committed, and useless on arm64 - under a path
  build.rs hardcodes. So the layout goes per-target (`lib/x64-windows/`, `lib/arm64-macos/`)
  with build.rs selecting one, and the mac arm links `c++` explicitly: Mach-O objects carry no
  `/DEFAULTLIB` directives, so the C++ runtime libheif and libde265 need will not link itself.

### 7.3 What builds today, and what does not

`cargo test -p fire-ipc` and `cargo test -p fire-decode --no-default-features` should pass on a
Mac as soon as rustup is installed - that is the "the toolchain works" signal, and the first
Phase 2 milestone.

`fire` itself will not compile, and it is not a tooling gap: `render/gpu.rs` has an
unconditional `use crate::render::d3d11;` and calls `d3d11::Device::create()` in `bring_up`,
while `render/mod.rs` gates that module to `cfg(windows)`; only `make_shader` has a
non-Windows arm, and it is a stub that returns an error. `render/metal.rs` (Phase 2 step 2) is
what unblocks it.

### 7.4 CI and the harness

CI gains a `macos-latest` leg mirroring the two Windows jobs - `check` with
`--no-default-features` (no vendored input, no libclang needed) and `full` gated on a restored
vendor tree. The `full` leg's cache key must include the target, or the arm64 `.a`s and the x64
`.lib`s collide in one cache entry. Per D11, CI stops there: it never signs, notarizes or
packages a `.dmg` - that only runs on the dev Mac via `scripts/build-mac.sh`, so the Developer ID
cert and App Store Connect key never need to exist as CI secrets.

Both legs are mandatory rather than nice-to-have: clippy on Windows never sees `render/metal.rs`
and clippy on macOS never sees `render/d3d11.rs`, so a single-host CI cannot keep the workspace
lint-clean once the second leaf exists.

`scripts/ttfp.ps1` is PowerShell; it gets a shell twin (or runs under `pwsh`). `ttfp.rs`'s
non-Windows arm measures from the process's own clock, which cannot see loader/dyld time, so mac
numbers compare mac builds against each other - the Windows figures in §8 are not a baseline
for them.

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
   toolchain without needing a single line of new code.
2. Native decoders (§7.2): drop the Windows-only short-circuit in `psd-sdk-sys` and exclude
   `PsdNativeFile.cpp`; build the arm64 HEIF stack with vcpkg (D25), move the vendored libs to
   per-target subdirectories, teach `heif-sys` to pick one and to link `c++`. Ends with
   `cargo test --workspace` passing everything that does not need a window.
3. `render/metal.rs`: a `CAMetalLayer` on the winit window, handing sokol_gfx its `MTLDevice` at
   setup and its per-frame drawable + render-pass descriptor through `sg_swapchain` - the twin
   of `render/d3d11.rs`, same shape, same rough size. `SOKOL_BACKEND=METAL` already flows
   through both the vendored sokol build and `simgui.c` (build.rs picks `SOKOL_METAL` from the
   target OS).
4. Shader: adopt `sokol-shdc` (D4) so one source produces both DXBC and MSL plus the reflection,
   and compile the MSL to a `.metallib` with `xcrun metal` in build.rs (D24); verify the Windows
   output is unchanged before deleting the hand-written path. First `cargo run -p fire`.
5. Leaves: the open-file delegate hook, the `muda` menu bar, native fullscreen mapping, pinch,
   the clipboard twin.
6. Retina: `scale_factor` into fit/1:1; verify the zoom-snap ladder lands on true 100 %.
7. Measure: the mac twin of `scripts/ttfp.ps1` (§7.4) and a launch-path breakdown, so the Metal
   bring-up gets the same scrutiny the D3D11 one did. There is no cross-OS budget - the number
   to beat is the next mac build's.
8. CI (§7.4): the `macos-latest` matrix leg, with the vendor cache keyed per target.
9. `build-mac.sh`: `.app` layout, `Info.plist` from `product.json` (every extension from the
   installer's list, `LSHandlerRank = Alternate`, `CFBundleIconFile`), `codesign --options
   runtime --timestamp`, `notarytool submit --wait`, `stapler`, `hdiutil` → `dist/Fire-<ver>.dmg`;
   run by hand on the dev Mac, whose keychain holds the Developer ID cert (D11) - no CI job
   invokes it.
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
  Metal toolchain is a further download. Every dev machine and the CI runner pay it, and the
  failure mode is a `xcrun metal` that is simply absent. If that becomes intolerable, the
  fallback is shipping MSL source and letting sokol_gfx compile it at pipeline creation - which
  puts shader compilation back on the launch path, the cost D4 exists to avoid.
- **The vendored native trees are now per-target (D25)** - two sets of artifacts under one
  `vendor/`, produced by two runs of the same recipe, cached by CI under keys that must not
  collide. The failure mode is a silently stale or wrong-arch lib, which surfaces as a link
  error at best and a mismatched ABI at worst. Keep `VENDOR.txt` the single recipe for both.
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
