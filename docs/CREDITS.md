# Credits

Fire is a small program standing on a lot of other people's work. Almost none of the hard parts -
decoding a dozen image formats correctly, managing color, drawing a UI - are Fire's own. This page
says who wrote them.

For the formal license terms and per-package copyright notices, see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## Decoding

Fire's whole reason to exist is getting a pixel on screen quickly, and that is almost entirely a
question of how fast someone else's decoder runs.

* **[zune-image](https://github.com/etemesi254/zune-image)** - Caleb Etemesi and the zune-image
  developers. The hot path. JPEG, PNG, BMP, PPM, QOI, PSD, farbfeld, HDR, JPEG XL - Fire's
  time-to-first-pixel is largely zune's decode speed.
* **[image](https://github.com/image-rs/image)** - the image-rs developers. The fallback decoder,
  the animated GIF path, and - measured, not assumed - the faster PNG and Radiance HDR decoders.
  With it come **[png](https://github.com/image-rs/image-png)**,
  **[gif](https://github.com/image-rs/image-gif)**,
  **[image-webp](https://github.com/image-rs/image-webp)**,
  **[fdeflate](https://github.com/image-rs/fdeflate)** and
  **[weezl](https://github.com/image-rs/weezl)**.
* **[tiff](https://github.com/image-rs/image-tiff)** - the image-rs developers. Fire drives it
  directly rather than through `image`, to keep 16-bit samples and unlabelled alpha channels.
* **[exr](https://github.com/johannesvollmer/exrs)** - Johannes Vollmer and contributors. OpenEXR,
  in pure Rust, with **[lebe](https://github.com/johannesvollmer/lebe)**.
* **[jxl-oxide](https://github.com/tirr-c/jxl-oxide)** - Wonwoo Choi. JPEG XL.
* **[ddsfile](https://github.com/cwfitzgerald/ddsfile)** - Michael Dilger and Connor Fitzgerald.
  The DDS container: the legacy D3D9 header, the DX10 extension, and the cubemap/array/volume
  layout rules.
* **[bcdec_rs](https://github.com/ScanMountGoat/bcdec_rs)** - ScanMountGoat, from
  **[bcdec](https://github.com/iOrange/bcdec)** by Sergii "iOrange" Kudlai. The BC1-BC7 block
  decoders behind Fire's DDS support, BC6H's HDR blocks included - a safe, dependency-free port.
* **[libheif](https://github.com/strukturag/libheif)** and
  **[libde265](https://github.com/strukturag/libde265)** - Dirk Farin and struktur AG. HEIF/HEIC.
* **[dav1d](https://code.videolan.org/videolan/dav1d)** - VideoLAN and the dav1d authors. The AV1
  decoder behind AVIF, and the reason AVIF opens as fast as it does.
* **[psd_sdk](https://github.com/MolecularMatters/psd_sdk)** - Stefan Reinalter / Molecular
  Matters. Photoshop `.psd` and `.psb`.
* **[kamadak-exif](https://github.com/kamadak/exif-rs)** - KAMADA Ken'ichi. EXIF parsing.

## Color

* **[Little-CMS](https://littlecms.com)** - Marti Maria Saguer. Two decades of ICC color
  management, and the reason an embedded profile is honored rather than ignored.
* **[moxcms](https://github.com/awxkee/moxcms)** and **[pxfm](https://github.com/awxkee/pxfm)** -
  Radzivon Bartoshyk.
* **[half](https://github.com/VoidStarKat/half-rs)** - Kathryn Long. `f16` for HDR pixels.

## Window, GPU and interface

Fire's shell is the same code on Windows and macOS because these projects absorb the difference.

* **[winit](https://github.com/rust-windowing/winit)** - the winit contributors, Pierre Krieger,
  Kirill Chibisov and many others. The window, the event loop, input, DPI, drag-and-drop, the
  light/dark preference and native full screen - on both operating systems - along with
  **[dpi](https://github.com/rust-windowing/winit)**,
  **[raw-window-handle](https://github.com/rust-windowing/raw-window-handle)** and
  **[cursor-icon](https://github.com/rust-windowing/cursor-icon)**.
* **[sokol](https://github.com/floooh/sokol)** and
  **[sokol-rust](https://github.com/floooh/sokol-rust)** - Andre Weissflog. `sokol_gfx` is the one
  drawing API over Direct3D 11 and Metal, and `sokol_imgui.h` is the Dear ImGui renderer Fire
  draws its chrome with. `sokol-shdc` turns Fire's one shader source into both backends' code and
  the reflection to bind it.
* **[Dear ImGui](https://github.com/ocornut/imgui)** - Omar Cornut. Every pixel of Fire's chrome:
  toolbar, status bar, transport, popups, settings window.
* **[cimgui](https://github.com/cimgui/cimgui)** - Stephan Dilly and contributors. The C API that
  makes binding ImGui from Rust tractable.
* **[dear-imgui-rs and dear-imgui-winit](https://github.com/Latias94/dear-imgui-rs)** - Mingzhen
  Zhuang. The safe Rust bindings, and the winit input backend Fire feeds ImGui through.

## Platform

* **[windows and windows-sys](https://github.com/microsoft/windows-rs)** - Microsoft. Rust
  bindings for the Win32 and COM surface Fire still names directly on Windows: the D3D11 device
  and DXGI swapchain, the clipboard, the foreground handoff, the launcher's show state.
* **[objc2](https://github.com/madsmtm/objc2)** - Mads Marquart, Steven Sheldon and contributors.
  The Objective-C bridge behind Fire's macOS leaves: the `CAMetalLayer` sokol_gfx draws through,
  the `application:openURLs:` hook that makes Finder opens work, and the pasteboard.
* **[muda](https://github.com/tauri-apps/muda)** - the Tauri programme. Fire's macOS menu bar,
  with **[keyboard-types](https://github.com/pyfisch/keyboard-types)** by Pyfisch.
* **[interprocess](https://github.com/kotauskas/interprocess)** - Goat (kotauskas). One API over
  a Windows named pipe and a Unix socket: the instance socket that makes Fire one process with
  as many windows as you like.
* **[rfd](https://github.com/PolyMeilex/rfd)** - Bartłomiej Maryńczak. The native open-file dialog
  and the startup error box.
* **[dirs](https://github.com/soc/dirs-rs)** - Simon Ochsenreither. Where the config lives on each
  OS, with **[option-ext](https://github.com/soc/option-ext)**.
* **[notify](https://github.com/notify-rs/notify)** - Félix Saparelli, Daniel Faust, Aron Heinecke
  and contributors. The file watcher behind hot-reload.
* **[Inno Setup](https://jrsoftware.org/isinfo.php)** - Jordan Russell and Martijn Laan. Fire's
  Windows installer.

## Infrastructure

The crates that do not show up in a feature list but without which none of the above compiles:

* **[serde](https://github.com/serde-rs/serde)**, **[toml](https://github.com/toml-rs/toml)** -
  Erick Tryzelaar, David Tolnay, Ed Page and contributors. The config file.
* **[crossbeam](https://github.com/crossbeam-rs/crossbeam)** - the Crossbeam project developers.
  The channels carrying decode results to the UI thread.
* **[rayon](https://github.com/rayon-rs/rayon)** - Josh Stone, Niko Matsakis and contributors.
* **[tracing](https://github.com/tokio-rs/tracing)** - Eliza Weisman and the Tokio contributors.
* **[bytemuck](https://github.com/Lokathor/bytemuck)** - Daniel "Lokathor" Gee. Safe POD casts in
  the decode path.
* **[syn](https://github.com/dtolnay/syn)**, **[quote](https://github.com/dtolnay/quote)**,
  **[proc-macro2](https://github.com/dtolnay/proc-macro2)**,
  **[thiserror](https://github.com/dtolnay/thiserror)** - David Tolnay. Half the Rust ecosystem,
  really.
* **[flate2](https://github.com/rust-lang/flate2-rs)**,
  **[miniz_oxide](https://github.com/Frommi/miniz_oxide)**,
  **[zerocopy](https://github.com/google/zerocopy)**,
  **[parking_lot](https://github.com/Amanieu/parking_lot)**,
  **[smallvec](https://github.com/servo/rust-smallvec)**,
  **[hashbrown](https://github.com/rust-lang/hashbrown)** and the rest of the long tail - see
  [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for the complete list of 161 crates.

## Build tooling

Not linked into the Fire binary, but Fire would not exist without them:

* **[bindgen](https://github.com/rust-lang/rust-bindgen)** and
  **[cc](https://github.com/rust-lang/cc-rs)** - the Rust project. The `-sys` crates.
* **[resvg](https://github.com/linebender/resvg)** - Yevhenii Reizner and the Linebender project.
  Rasterizes Fire's toolbar SVGs into the executable at build time.
* **[winresource](https://github.com/BenjaminRi/winresource)** - Benjamin Richner. The version
  resource and Explorer icon on Windows.
* **`fxc` (Windows SDK) and `xcrun metal` (Xcode)** - Microsoft and Apple. The offline shader
  compilers that keep shader compilation off Fire's launch path on each OS.
* **The Rust project and its contributors** - the language, the compiler, the standard library,
  and Cargo.

## Development

* **[Claude Code](https://claude.com/claude-code)** - Anthropic. Used throughout Fire's
  development as a coding assistant.

## Not third-party

Fire's toolbar icons (`assets/icons/`) and its stylesheet (`crates/fire/src/ui/theme.toml`) are
original work, MIT licensed along with the rest of Fire. The UI is rendered in the system font read
from the running machine - Segoe UI on Windows, SF Pro on macOS - under that machine's own OS
license; Fire bundles no fonts.

---

*Something missing or miscredited? Please open an issue at
<https://github.com/psmyles/fire>.*
