# Third-party notices

Fire (`fire.exe`) is a single statically-linked executable. Everything it needs at runtime is
compiled into that one file, so the binary you receive contains code from the projects listed
below. Their licenses require that their copyright notices and license terms travel with the
binary — this document, together with the [`licenses/`](licenses/) directory, is how they do.

Fire's own code is MIT licensed; see [`LICENSE`](LICENSE).

* **Section 1** covers the native C/C++ libraries. Read this one — it is short, and it contains
  the only obligation in the whole distribution that goes beyond attribution.
* **Section 2** covers the Rust crates (125 of them).
* **Section 3** notes what is *not* here, and why.

Generated against Fire 0.1.9 (`Cargo.lock`, target `x86_64-pc-windows-msvc`, runtime dependencies
only). Build-time-only tooling — `bindgen`, `cc`, `resvg`, `winresource`, `serde_json`, `fxc` — is
excluded: it produces `fire.exe` but no part of it is linked into `fire.exe`.

---

## 1. Native libraries

These are C/C++ libraries, vendored under `crates/*/vendor/` and statically linked.

### libheif — LGPL-3.0-only

*HEIF/AVIF container parsing and API. Version 1.23.0. Linked via `crates/heif-sys`.*
Copyright © Dirk Farin and contributors — <https://github.com/strukturag/libheif>

### libde265 — LGPL-3.0-only

*HEVC decoder, used for `.heic` / `.heif`. Version 1.1.1. Linked via `crates/heif-sys`.*
Copyright © struktur AG and contributors — <https://github.com/strukturag/libde265>

> ### Your rights under the LGPL
>
> libheif and libde265 are licensed under the **GNU Lesser General Public License, version 3**
> ([`licenses/LGPL-3.0.txt`](licenses/LGPL-3.0.txt), which incorporates
> [`licenses/GPL-3.0.txt`](licenses/GPL-3.0.txt)). Fire links them statically, so LGPL-3.0 §4
> entitles you to modify those libraries and relink Fire against your modified versions.
>
> Fire does not restrict that in any way:
>
> * **Fire's complete source is public and MIT licensed**, including every build script that
>   produces `fire.exe`.
> * **The exact static libraries linked into this build are in the source tree**, at
>   `crates/heif-sys/vendor/lib/` (`heif.lib`, `libde265.lib`), with their public headers in
>   `crates/heif-sys/vendor/include/`.
> * **`crates/heif-sys/vendor/VENDOR.txt` documents precisely how those `.lib` files were built**
>   — the upstream versions, the vcpkg triplet (`x64-windows-static-md`), the port patches, and
>   the post-build strip step — so you can reproduce or replace them.
>
> To relink: build your own `heif.lib` / `libde265.lib` from modified libheif/libde265 sources,
> drop them into `crates/heif-sys/vendor/lib/`, and run `cargo build -p fire --release`. The
> resulting `fire.exe` uses your libraries.
>
> Neither library was modified for Fire. Corresponding sources for the versions above are
> available from the upstream projects linked next to each entry.

### dav1d — BSD-2-Clause

*AV1 decoder, used for `.avif`. Linked via `crates/heif-sys`.*
Copyright © 2018-2024 VideoLAN and dav1d authors. All rights reserved.
<https://code.videolan.org/videolan/dav1d>
License text: [`licenses/BSD-2-Clause.txt`](licenses/BSD-2-Clause.txt)

### psd_sdk — BSD-2-Clause

*Photoshop `.psd` / `.psb` reader. Linked via `crates/psd-sdk-sys`; vendored at commit
`f514495`.*
Copyright © 2011-2020 Molecular Matters GmbH — <https://github.com/MolecularMatters/psd_sdk>
License text: [`licenses/BSD-2-Clause.txt`](licenses/BSD-2-Clause.txt)

### Little-CMS (lcms2) — MIT

*ICC color management. Linked via the `lcms2-sys` crate, which vendors it.*
Copyright © 2023 Marti Maria Saguer — <https://littlecms.com>
License text: [`licenses/Little-CMS-MIT.txt`](licenses/Little-CMS-MIT.txt)

### Dear ImGui — MIT

*The entire user interface, plus its `imgui_impl_win32` / `imgui_impl_dx11` backends. Linked via
the `dear-imgui-sys` crate, which vendors it through cimgui.*
Copyright © 2014-2026 Omar Cornut — <https://github.com/ocornut/imgui>
License text: [`licenses/Dear-ImGui-MIT.txt`](licenses/Dear-ImGui-MIT.txt)

**cimgui** — the C API layer Dear ImGui is bound through — is MIT, copyright © 2015 Stephan Dilly
(<https://github.com/cimgui/cimgui>). `dear-imgui-sys` additionally carries a stack-layout
compatibility shim derived from the MIT-licensed stack layout extension in `imgui-node-editor`,
copyright © 2019 Michał Cichoń.

---

## 2. Rust crates

125 crates are linked into `fire.exe`. Where a crate offers a choice of licenses, the column below
records **the license Fire elects**, not the full SPDX expression — Fire elects MIT wherever MIT is
offered. Full texts:

| License | Text |
|---|---|
| MIT (114 crates) | [`licenses/MIT.txt`](licenses/MIT.txt) |
| BSD-3-Clause (6) | [`licenses/BSD-3-Clause.txt`](licenses/BSD-3-Clause.txt) |
| BSD-2-Clause (2) | [`licenses/BSD-2-Clause.txt`](licenses/BSD-2-Clause.txt) |
| CC0-1.0 (1) | [`licenses/CC0-1.0.txt`](licenses/CC0-1.0.txt) |
| Unicode-3.0 (1, in addition to MIT) | [`licenses/Unicode-3.0.txt`](licenses/Unicode-3.0.txt) |
| IJG (1, in addition to MIT) | see the note below the table |

Some crates ship a license file with no copyright line filled in. Rather than invent one, those
rows name the authors the crate itself declares.

| Crate | Version | License | Copyright |
|---|---|---|---|
| `adler2` | 2.0.1 | MIT | Copyright (C) Jonas Schievink <jonasschievink@gmail.com> |
| `alloc-no-stdlib` | 2.0.4 | BSD-3-Clause | Copyright (c) 2016 Dropbox, Inc. |
| `alloc-stdlib` | 0.2.4 | BSD-3-Clause | (no notice in crate; authors: Daniel Reiter Horn <danielrh@dropbox.com>) |
| `bit_field` | 0.10.3 | MIT | Copyright (c) 2016 Philipp Oppermann |
| `bitflags` | 2.13.0 | MIT | Copyright (c) 2014 The Rust Project Developers |
| `brotli-decompressor` | 5.0.3 | MIT | Copyright (c) 2016 Dropbox, Inc. |
| `bytemuck` | 1.25.0 | MIT | Copyright (c) 2019 Daniel "Lokathor" Gee. |
| `bytemuck_derive` | 1.10.2 | MIT | Copyright (c) 2019 Daniel "Lokathor" Gee. |
| `byteorder-lite` | 0.1.0 | MIT | Copyright (c) 2015 Andrew Gallant |
| `cfg-if` | 1.0.4 | MIT | Copyright (c) 2014 Alex Crichton |
| `color_quant` | 1.1.0 | MIT | Copyright (c) 2016 PistonDevelopers |
| `crc32fast` | 1.5.0 | MIT | Copyright (c) 2018 Sam Rijs, Alex Crichton and contributors |
| `crossbeam-channel` | 0.5.15 | MIT | Copyright (c) 2019 The Crossbeam Project Developers |
| `crossbeam-deque` | 0.8.6 | MIT | Copyright (c) 2019 The Crossbeam Project Developers |
| `crossbeam-epoch` | 0.9.18 | MIT | Copyright (c) 2019 The Crossbeam Project Developers |
| `crossbeam-utils` | 0.8.21 | MIT | Copyright (c) 2019 The Crossbeam Project Developers |
| `dear-imgui-rs` | 0.15.1 | MIT | (no notice in crate; authors: Mingzhen Zhuang <superfrankie621@gmail.com>) |
| `dear-imgui-sys` | 0.15.1 | MIT | (no notice in crate; authors: Mingzhen Zhuang <superfrankie621@gmail.com>) |
| `either` | 1.16.0 | MIT | Copyright (c) 2015 |
| `equivalent` | 1.0.2 | MIT | Copyright (c) 2016--2023 |
| `exr` | 1.74.0 | BSD-3-Clause | Copyright (c) Contributors to the OpenEXR Project. All rights reserved. |
| `fax` | 0.2.7 | MIT | Copyright © 2021 The pdf-rs contributers. |
| `fdeflate` | 0.3.7 | MIT | (no notice in crate; authors: The image-rs Developers) |
| `flate2` | 1.1.9 | MIT | Copyright (c) 2014-2026 Alex Crichton |
| `foreign-types` | 0.5.0 | MIT | Copyright (c) 2017 The foreign-types Developers |
| `foreign-types-macros` | 0.2.3 | MIT | Copyright (c) 2017 The foreign-types Developers |
| `foreign-types-shared` | 0.3.1 | MIT | Copyright (c) 2017 The foreign-types Developers |
| `gif` | 0.14.2 | MIT | Copyright (c) 2015 nwin |
| `half` | 2.7.1 | MIT | (no notice in crate; authors: Kathryn Long <squeeself@gmail.com>) |
| `hashbrown` | 0.17.1 | MIT | Copyright (c) 2016 Amanieu d'Antras |
| `image` | 0.25.10 | MIT | (no notice in crate; authors: The image-rs Developers) |
| `image-webp` | 0.2.4 | MIT | (no copyright notice supplied by the crate) |
| `indexmap` | 2.14.0 | MIT | Copyright (c) 2016--2017 |
| `jpeg-encoder` | 0.7.0 | (MIT OR Apache-2.0) AND IJG | Copyright (c) 2021 Volker Ströbel <volkerstroebel@mysurdity.de> |
| `jxl-bitstream` | 1.1.0 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-coding` | 1.0.1 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-color` | 0.11.0 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-frame` | 0.13.3 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-grid` | 0.6.2 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-image` | 0.13.0 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-jbr` | 0.2.1 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-modular` | 0.11.3 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-oxide` | 0.12.6 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-oxide-common` | 1.0.0 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-render` | 0.12.4 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-threadpool` | 1.0.0 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `jxl-vardct` | 0.11.1 | MIT | (no notice in crate; authors: Wonwoo Choi <chwo9843@gmail.com>) |
| `kamadak-exif` | 0.6.1 | BSD-2-Clause | Copyright (c) 2016-2023 KAMADA Ken'ichi. |
| `lcms2` | 6.1.1 | MIT | Copyright (c) Kornel Lesiński |
| `lcms2-sys` | 4.0.6 | MIT | (no notice in crate; authors: Kornel Lesiński <kornel@geekhood.net>) |
| `lebe` | 0.5.3 | BSD-3-Clause | Copyright (c) 2022 Contributors to the lebe Project. All rights reserved. |
| `libc` | 0.2.186 | MIT | Copyright (c) The Rust Project Developers |
| `lock_api` | 0.4.14 | MIT | Copyright (c) 2016 The Rust Project Developers |
| `log` | 0.4.33 | MIT | Copyright (c) 2014 The Rust Project Developers |
| `miniz_oxide` | 0.8.9 | MIT | Copyright 2013-2014 RAD Game Tools and Valve Software; Copyright 2010-2014 Rich Geldreich and Tenacious Software LLC; Copyright 2016 Martin Molzer |
| `mint` | 0.5.9 | MIT | Copyright (c) 2017 Dzmitry Malyshau |
| `moxcms` | 0.8.1 | BSD-3-Clause | Copyright (c) Radzivon Bartoshyk. All rights reserved. |
| `mutate_once` | 0.1.2 | BSD-2-Clause | Copyright (c) 2019 KAMADA Ken'ichi. |
| `notify` | 8.2.0 | CC0-1.0 | (no notice in crate; authors: Félix Saparelli <me@passcod.name>, Daniel Faust <hessijames@gmail.com>, Aron Heinecke <Ox0p54r36@t-online.de>) |
| `notify-types` | 2.1.0 | MIT | Copyright (c) 2023 Notify Contributors |
| `num-traits` | 0.2.19 | MIT | Copyright (c) 2014 The Rust Project Developers |
| `once_cell` | 1.21.4 | MIT | (no notice in crate; authors: Aleksey Kladov <aleksey.kladov@gmail.com>) |
| `parking_lot` | 0.12.5 | MIT | Copyright (c) 2016 The Rust Project Developers |
| `parking_lot_core` | 0.9.12 | MIT | Copyright (c) 2016 The Rust Project Developers |
| `pin-project-lite` | 0.2.17 | MIT | (no copyright notice supplied by the crate) |
| `png` | 0.18.1 | MIT | Copyright (c) 2015 nwin |
| `proc-macro2` | 1.0.106 | MIT | (no notice in crate; authors: David Tolnay <dtolnay@gmail.com>, Alex Crichton <alex@alexcrichton.com>) |
| `pxfm` | 0.1.29 | BSD-3-Clause | Copyright (c) Radzivon Bartoshyk. All rights reserved. |
| `quick-error` | 2.0.1 | MIT | Copyright (c) 2015 The quick-error Developers |
| `quote` | 1.0.46 | MIT | (no notice in crate; authors: David Tolnay <dtolnay@gmail.com>) |
| `rayon` | 1.12.0 | MIT | Copyright (c) 2010 The Rust Project Developers |
| `rayon-core` | 1.13.0 | MIT | Copyright (c) 2010 The Rust Project Developers |
| `same-file` | 1.0.6 | MIT | Copyright (c) 2017 Andrew Gallant |
| `scopeguard` | 1.2.0 | MIT | Copyright (c) 2016-2019 Ulrik Sverdrup "bluss" and scopeguard developers |
| `serde` | 1.0.228 | MIT | (no notice in crate; authors: Erick Tryzelaar <erick.tryzelaar@gmail.com>, David Tolnay <dtolnay@gmail.com>) |
| `serde_core` | 1.0.228 | MIT | (no notice in crate; authors: Erick Tryzelaar <erick.tryzelaar@gmail.com>, David Tolnay <dtolnay@gmail.com>) |
| `serde_derive` | 1.0.228 | MIT | (no notice in crate; authors: Erick Tryzelaar <erick.tryzelaar@gmail.com>, David Tolnay <dtolnay@gmail.com>) |
| `serde_spanned` | 0.6.9 | MIT | Copyright (c) Individual contributors |
| `simd-adler32` | 0.3.9 | MIT | Copyright (c) [2021] [Marvin Countryman] |
| `smallvec` | 1.15.2 | MIT | Copyright (c) 2018 The Servo Project Developers |
| `syn` | 2.0.118 | MIT | (no notice in crate; authors: David Tolnay <dtolnay@gmail.com>) |
| `thiserror` | 2.0.18 | MIT | (no notice in crate; authors: David Tolnay <dtolnay@gmail.com>) |
| `thiserror-impl` | 2.0.18 | MIT | (no notice in crate; authors: David Tolnay <dtolnay@gmail.com>) |
| `tiff` | 0.11.3 | MIT | Copyright (c) 2018 PistonDevelopers |
| `toml` | 0.8.23 | MIT | Copyright (c) Individual contributors |
| `toml_datetime` | 0.6.11 | MIT | Copyright (c) Individual contributors |
| `toml_edit` | 0.22.27 | MIT | Copyright (c) Individual contributors |
| `toml_write` | 0.1.2 | MIT | Copyright (c) Individual contributors |
| `tracing` | 0.1.44 | MIT | Copyright (c) 2019 Tokio Contributors |
| `tracing-core` | 0.1.36 | MIT | Copyright (c) 2019 Tokio Contributors |
| `unicode-ident` | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 | (no notice in crate; authors: David Tolnay <dtolnay@gmail.com>) |
| `walkdir` | 2.5.0 | MIT | Copyright (c) 2015 Andrew Gallant |
| `weezl` | 0.1.12 | MIT | Copyright (c) HeroicKatora 2020 |
| `winapi-util` | 0.1.11 | MIT | Copyright (c) 2017 Andrew Gallant |
| `windows` | 0.61.3 | MIT | Copyright (c) Microsoft Corporation. |
| `windows_x86_64_msvc` | 0.53.1 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-collections` | 0.2.0 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-core` | 0.61.2 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-future` | 0.2.1 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-implement` | 0.60.2 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-interface` | 0.59.3 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-link` | 0.1.3 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-link` | 0.2.1 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-numerics` | 0.2.0 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-result` | 0.3.4 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-strings` | 0.4.2 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-sys` | 0.60.2 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-sys` | 0.61.2 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-targets` | 0.53.5 | MIT | Copyright (c) Microsoft Corporation. |
| `windows-threading` | 0.1.0 | MIT | Copyright (c) Microsoft Corporation. |
| `winnow` | 0.7.15 | MIT | (no copyright notice supplied by the crate) |
| `zerocopy` | 0.8.52 | MIT | Copyright 2019 The Fuchsia Authors. |
| `zerocopy-derive` | 0.8.52 | MIT | Copyright 2019 The Fuchsia Authors. |
| `zune-bmp` | 0.5.2 | MIT | Copyright (c) zune-image developers |
| `zune-core` | 0.5.1 | MIT | Copyright (c) zune-image developers |
| `zune-farbfeld` | 0.5.2 | MIT | Copyright (c) zune-image developers |
| `zune-hdr` | 0.5.2 | MIT | Copyright (c) zune-image developers |
| `zune-image` | 0.5.0 | MIT | Copyright (c) zune-image developers |
| `zune-inflate` | 0.2.54 | MIT | (no copyright notice supplied by the crate) |
| `zune-jpeg` | 0.5.15 | MIT | Copyright (c) zune-image developers |
| `zune-jpegxl` | 0.5.2 | MIT | Copyright (c) zune-image developers |
| `zune-png` | 0.5.2 | MIT | Copyright (c) zune-image developers |
| `zune-ppm` | 0.5.1 | MIT | Copyright (c) zune-image developers |
| `zune-psd` | 0.5.1 | MIT | Copyright (c) zune-image developers |
| `zune-qoi` | 0.5.2 | MIT | Copyright (c) zune-image developers |

### Note on `jpeg-encoder` (IJG)

`jpeg-encoder` is licensed `(MIT OR Apache-2.0) AND IJG` — the second term covers code derived
from the Independent JPEG Group's libjpeg. The crate ships no separate IJG text; the applicable
terms are those of the IJG distribution, which permit use, modification and redistribution
provided the origin is not misrepresented and any changes are marked. The crate is pulled in by
`image`'s `jpeg` feature. Fire decodes JPEG with `zune-jpeg` and never encodes, so this code is
reachable only through `image`'s fallback path.

### Note on `notify` (CC0-1.0)

CC0-1.0 is a public-domain dedication and imposes no attribution requirement. It is listed here
for completeness. Full text: [`licenses/CC0-1.0.txt`](licenses/CC0-1.0.txt).

---

## 3. What is not in this document

* **Fonts.** Fire bundles none. Its UI is rendered in Segoe UI, read at runtime from
  `C:\Windows\Fonts\segoeui.ttf` on the machine Fire is running on — the user's own copy, under
  the user's own Windows license. If it cannot be read, Fire falls back to Dear ImGui's built-in
  ProggyClean (public domain / MIT, bundled with Dear ImGui above).
* **Icons.** The toolbar icons in `assets/icons/` are original to Fire and are covered by Fire's
  own MIT license.
* **The Windows API.** Fire links `d3d11`, `dxgi`, `dwmapi`, `user32`, `gdi32`, `advapi32`,
  `shell32`, `comdlg32` and friends from the Windows SDK. These are operating-system components
  licensed to the user by Microsoft as part of Windows, not redistributed by Fire.
* **Build-time tooling.** `bindgen`, `cc`, `resvg`, `winresource`, `serde_json` and `fxc.exe`
  run during the build and contribute no code to `fire.exe`. Inno Setup builds the installer;
  its license permits distributing the installers it produces without attribution.
* **Rust and its standard library** (MIT OR Apache-2.0, © The Rust Project Developers), portions
  of which are linked into every Rust binary.

---

## Regenerating this document

The tables above are derived from `Cargo.lock` plus the license files in the local cargo registry.
Re-derive them after a dependency change with:

```sh
cargo tree -p fire -e normal --target x86_64-pc-windows-msvc --prefix none --format "{p}"
```

and cross-reference `cargo metadata --format-version 1` for each package's `license` field. The
native libraries in section 1 change only when `crates/*/vendor/` changes.
