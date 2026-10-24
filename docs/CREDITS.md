# Credits

Fire is built on the projects listed below. For license terms and per-package copyright notices,
see [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## Decoding

- **[zune-image](https://github.com/etemesi254/zune-image)** - Caleb Etemesi and the zune-image
  developers. Format detection, the JPEG, BMP and PPM decoders, the embedded JPEG previews in
  camera raw files, and the first frame of animated WebP.
- **[libwebp](https://chromium.googlesource.com/webm/libwebp)** - Google. Lossy WebP, through
  **[libwebp-sys](https://github.com/NoXF/libwebp-sys)** by XianYou and Kornel Lesiński.
- **[image](https://github.com/image-rs/image)** - the image-rs developers. PNG, GIF, Radiance
  HDR, TGA and ICO, and the fallback decoder. Includes
  **[png](https://github.com/image-rs/image-png)**,
  **[gif](https://github.com/image-rs/image-gif)**,
  **[fdeflate](https://github.com/image-rs/fdeflate)** and
  **[weezl](https://github.com/image-rs/weezl)**, plus
  **[moxcms](https://github.com/awxkee/moxcms)** / **[pxfm](https://github.com/awxkee/pxfm)** by
  Radzivon Bartoshyk.
- **[image-webp](https://github.com/image-rs/image-webp)** - the image-rs developers. Lossless
  WebP.
- **[tiff](https://github.com/image-rs/image-tiff)** - the image-rs developers. TIFF, read directly
  to keep 16-bit samples and extra alpha channels. Includes
  **[fax](https://github.com/pdf-rs/fax)** by the pdf-rs contributors.
- **[exr](https://github.com/johannesvollmer/exrs)** - Johannes Vollmer and contributors. OpenEXR,
  with **[lebe](https://github.com/johannesvollmer/lebe)**.
- **[jxl-oxide](https://github.com/tirr-c/jxl-oxide)** - Wonwoo Choi. JPEG XL.
- **[qoi](https://github.com/aldanor/qoi-rust)** - Ivan Smirnov. QOI.
- **[ddsfile](https://github.com/cwfitzgerald/ddsfile)** - Michael Dilger and Connor Fitzgerald.
  DDS headers: the D3D9 and DX10 formats and the cubemap, array and volume layouts.
- **[bcdec_rs](https://github.com/ScanMountGoat/bcdec_rs)** - ScanMountGoat, a port of
  **[bcdec](https://github.com/iOrange/bcdec)** by Sergii "iOrange" Kudlai. BC1-BC7 block
  decoding, BC6H included.
- **[libheif](https://github.com/strukturag/libheif)** and
  **[libde265](https://github.com/strukturag/libde265)** - Dirk Farin and struktur AG. HEIF/HEIC.
- **[dav1d](https://code.videolan.org/videolan/dav1d)** - VideoLAN and the dav1d authors. AV1
  decoding for AVIF.
- **[psd_sdk](https://github.com/MolecularMatters/psd_sdk)** - Stefan Reinalter / Molecular
  Matters. The composite image of Photoshop `.psd` and `.psb` files. Fire's patches are listed in
  `crates/psd-sdk-sys/vendor/VENDOR.txt`.
- **[miniz_oxide](https://github.com/Frommi/miniz_oxide)** - Frommi and oyvindln, from miniz by
  Rich Geldreich. Inflates ZIP-compressed PSD layer channels.

## Color

- **[Little-CMS](https://littlecms.com)** - Marti Maria Saguer. ICC color management, through
  **[lcms2](https://github.com/kornelski/rust-lcms2)** by Kornel Lesiński.
- **[half](https://github.com/VoidStarKat/half-rs)** - Kathryn Long. `f16` for HDR pixels.

## Window, GPU and interface

- **[winit](https://github.com/rust-windowing/winit)** - the winit contributors, Pierre Krieger,
  Kirill Chibisov and others. Window, event loop, input, DPI, drag-and-drop, the light/dark
  preference and full screen, with **[dpi](https://github.com/rust-windowing/winit)**,
  **[raw-window-handle](https://github.com/rust-windowing/raw-window-handle)** and
  **[cursor-icon](https://github.com/rust-windowing/cursor-icon)**.
- **[sokol](https://github.com/floooh/sokol)** and
  **[sokol-rust](https://github.com/floooh/sokol-rust)** - Andre Weissflog (bindings by Erik
  Wilhelm Gren). `sokol_gfx` draws over Direct3D 11 and Metal; `sokol_imgui` renders Dear ImGui.
- **[Dear ImGui](https://github.com/ocornut/imgui)** - Omar Cornut. The interface: toolbar, status
  bar, transport, popups and settings window.
- **[cimgui](https://github.com/cimgui/cimgui)** - Stephan Dilly and contributors. The C API Dear
  ImGui is bound through.
- **[dear-imgui-rs and dear-imgui-winit](https://github.com/Latias94/dear-imgui-rs)** - Mingzhen
  Zhuang. Rust bindings for Dear ImGui and its winit input backend.

## Typefaces

- **Segoe UI** (Windows) and **SF Pro** (macOS) - Microsoft and Apple. Interface text, read from
  the OS at runtime. Fire bundles no fonts.
- **ProggyClean** - Tristan Grimmer. Dear ImGui's built-in font, used if the system font cannot be
  read.

## Tone mapping

- **Erik Reinhard and colleagues** - "Photographic Tone Reproduction for Digital Images" (2002).
  The Reinhard tone mapper.
- **Krzysztof Narkowicz** - "ACES Filmic Tone Mapping Curve" (2016). The ACES tone mapper.

## Platform

- **[windows and windows-sys](https://github.com/microsoft/windows-rs)** - Microsoft. Win32 and
  COM bindings: the D3D11 device and DXGI swapchain, the clipboard, the foreground handoff, the
  launcher's show state, and the Explorer thumbnail provider, its registration and the thumbnail
  cache purge.
- **[objc2](https://github.com/madsmtm/objc2)** - Mads Marquart, Steven Sheldon and contributors.
  Objective-C bindings for the macOS `CAMetalLayer`, Finder opens and the pasteboard.
- **[muda](https://github.com/tauri-apps/muda)** - the Tauri programme. The macOS menu bar, with
  **[keyboard-types](https://github.com/pyfisch/keyboard-types)** by Pyfisch.
- **[interprocess](https://github.com/kotauskas/interprocess)** - Goat (kotauskas). The instance
  socket: a named pipe on Windows, a Unix socket on macOS.
- **[libc](https://github.com/rust-lang/libc)** - the Rust project. Socket-file cleanup, and the
  process start time on macOS.
- **[rfd](https://github.com/PolyMeilex/rfd)** - Bartłomiej Maryńczak. The open-file dialog and
  the startup error box.
- **[dirs](https://github.com/soc/dirs-rs)** - Simon Ochsenreither. Per-user config location, with
  **[option-ext](https://github.com/soc/option-ext)**.
- **[notify](https://github.com/notify-rs/notify)** - Félix Saparelli, Daniel Faust, Aron Heinecke
  and contributors. Reloads the open image when it changes on disk.
- **[Inno Setup](https://jrsoftware.org/isinfo.php)** - Jordan Russell and Martijn Laan. The
  Windows installer.

## Infrastructure

- **[serde](https://github.com/serde-rs/serde)** and **[toml](https://github.com/toml-rs/toml)** -
  Erick Tryzelaar, David Tolnay, Ed Page and contributors. The config file.
- **[crossbeam-channel](https://github.com/crossbeam-rs/crossbeam)** - the Crossbeam project
  developers. Channels from the decode pool and file watcher to the UI thread.
- **[rayon](https://github.com/rayon-rs/rayon)** - Josh Stone, Niko Matsakis and contributors.
  Threading in jxl-oxide.
- **[tracing](https://github.com/tokio-rs/tracing)** - Eliza Weisman and the Tokio contributors.
  Diagnostics in jxl-oxide.
- **[bytemuck](https://github.com/Lokathor/bytemuck)** - Daniel "Lokathor" Gee. The shader uniform
  block and 16-bit pixel casts.
- **[syn](https://github.com/dtolnay/syn)**, **[quote](https://github.com/dtolnay/quote)**,
  **[proc-macro2](https://github.com/dtolnay/proc-macro2)**,
  **[thiserror](https://github.com/dtolnay/thiserror)** - David Tolnay.
- **[flate2](https://github.com/rust-lang/flate2-rs)**,
  **[zerocopy](https://github.com/google/zerocopy)**,
  **[parking_lot](https://github.com/Amanieu/parking_lot)**,
  **[smallvec](https://github.com/servo/rust-smallvec)**,
  **[hashbrown](https://github.com/rust-lang/hashbrown)** and others - see
  [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for the complete list of 167 crates.

## Build tooling

Used to build Fire; not linked into the binary.

- **[bindgen](https://github.com/rust-lang/rust-bindgen)** and
  **[cc](https://github.com/rust-lang/cc-rs)** - the Rust project. FFI bindings and C/C++
  compilation for psd_sdk, libheif, sokol and sokol_imgui.
- **`sokol-shdc`** - Andre Weissflog. Shader cross-compilation to Direct3D and Metal, with
  reflection.
- **`fxc` (Windows SDK) and `xcrun metal` (Xcode)** - Microsoft and Apple. Offline shader
  compilers.
- **[resvg](https://github.com/linebender/resvg)** - Yevhenii Reizner and the Linebender project.
  Rasterizes the toolbar SVGs and decodes the logo at build time.
- **[serde_json](https://github.com/serde-rs/json)** - Erick Tryzelaar, David Tolnay and
  contributors. Reads `product.json` at build time.
- **[winresource](https://github.com/BenjaminRi/winresource)** - Benjamin Richner. Windows version
  resource and icon.
- **[ImageMagick](https://imagemagick.org)** - ImageMagick Studio LLC. `fire.ico`, at packaging
  time.
- **`codesign`, `notarytool`, `iconutil` and `hdiutil`** - Apple. macOS signing, notarization, the
  app icon and the `.dmg`.
- **The Rust project and its contributors** - the language, compiler, standard library and Cargo.

## Development

- **[Claude Code](https://claude.com/claude-code)** - Anthropic. Coding assistant.

## Original work

The following are original to Fire and MIT licensed with the rest of the project: the toolbar
icons (`assets/icons/`), the application icon and logo (`assets/icon.png`, `assets/icon-256.png`,
`assets/fire.ico`), the theme (`crates/fire/src/ui/theme.toml`), the shader
(`crates/fire/src/render/shader.glsl`), the camera raw preview extraction
(`crates/fire-decode/src/raw.rs`), the EXIF orientation handling (`crates/fire-decode/src/exif.rs`),
the PSD layer parser (`crates/fire-decode/src/psd_layers.rs`) and the Explorer thumbnail provider
(`crates/fire-shellext/`, `crates/fire-thumbs/`).

---

_Something missing or miscredited? Open an issue at
<https://github.com/psmyles/fire/issues/new>._
