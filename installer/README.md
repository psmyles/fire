# Fire installer (Windows)

Unsigned [Inno Setup](https://jrsoftware.org/isinfo.php) installer for `fire.exe`.

This is the **Windows** package. The macOS twin is `scripts/build-mac.sh`, which produces a signed
and notarized `dist/Fire-<version>.dmg`; it reads the same `product.json` and, unlike the list
below, takes its file associations straight out of `fire-decode`'s `SUPPORTED_EXTENSIONS`.

## Build

```pwsh
pwsh scripts/build-installer.ps1
```

That script is the entry point — don't run `ISCC` on `fire.iss` directly. It:

1. Reads **`product.json`** (repo root) — the single source of product metadata.
2. Syncs the Cargo workspace version to match.
3. Regenerates `assets/fire.ico` from `assets/icon.png` (ImageMagick).
4. Builds `fire.exe` and `fire_shellext.dll` (the Explorer thumbnail provider) in release
   (`build.rs` embeds the same `product.json` values).
5. Writes `installer/product.generated.iss` — the `#define`s `fire.iss` `#include`s (git-ignored).
6. Compiles `fire.iss` with `ISCC` → **`dist/Fire-<version>-Setup.exe`**.

Flags: `-SkipBuild` (reuse the existing `target/release/fire.exe` and `fire_shellext.dll`),
`-SkipIcon`.

### Prerequisites

- **Inno Setup 6** (`ISCC.exe` on `PATH` or under `Program Files\Inno Setup 6`).
- **ImageMagick** (`magick` on `PATH`) — only for the icon step; skip with `-SkipIcon`.

## What the installer does

- Per-user install (no admin) to `%LOCALAPPDATA%\Programs\Fire`, with a Start Menu shortcut and an
  optional desktop shortcut.
- A wizard page to set Fire as the default viewer, with a checkbox **per format** plus an **"All
  supported image formats"** master toggle. All off by default — an install never silently steals
  associations the user didn't pick.
- For each selected format it writes (under `HKCU`): a per-format `Fire.<format>` ProgID (e.g.
  `Fire.png`, `Fire.tga`) whose friendly type name is what Explorer shows in the **Type** column
  ("PNG image", "Truevision TGA image", …), an `OpenWithProgids` entry (adds Fire to "Open with"),
  the `.ext` default ProgID, and a Default-Programs `Capabilities` entry so Fire shows up in
  **Settings → Default apps**. Uninstall removes all of it. (The old single shared `Fire.Image`
  ProgID — which made every type read "Fire Image" — is deleted on install.)

> **Note on "default":** Windows 10/11 protect the per-extension default with a hashed `UserChoice`.
> The installer can make Fire the default for types that have no choice set yet, but it cannot
> silently override a type the user already assigned — for those, pick Fire via the "Open with"
> dialog or Settings → Default apps (where it now appears).

- An **Explorer Thumbnails** page of its own, right after the tasks: a checkbox per format,
  independent of the associations — Fire's `fire_shellext.dll` draws those formats' thumbnails
  whichever app opens them. Two columns, as in Settings ▸ Thumbnails: the formats Windows shows no
  thumbnail for (TGA, DDS, PSD, EXR, Radiance HDR, QOI, Netpbm, Farbfeld), pre-ticked, and the ones
  Windows already thumbnails, offered unticked. The installer does none of the registry work
  itself — it runs `fire.exe --set-thumbnails "<ids>"` after copying the files, and
  `fire.exe --remove-thumbnails` on uninstall, the same code Fire's Settings ▸ Thumbnails tab uses.
  Whatever provider a format had before is saved and put back. On an upgrade the page ticks what
  is on *now*, which the user may have changed in Settings since; Setup records that the choice was
  made (`Inno Setup CodeFile: ThumbnailsChosen` in its uninstall key), so a choice of nothing stays
  nothing. A silent install takes the same ticks without showing the page.
- The DLL is installed whatever is ticked, so formats can be switched on later from Settings.
  Explorer's thumbnail surrogate (`dllhost.exe`) can hold it open for a few minutes after use; an
  upgrade meets that through Setup's Restart Manager page, which offers to close it.

## Editing associations

Supported formats and their extensions live in `fire.iss` (`[Tasks]` + `[Registry]`). When
`fire-decode` gains a format, add a task and its per-extension registry rows there.

This is the one copy of that list which cannot `use` the real one — an Inno Setup script imports
nothing — so a test polices it: `fire-decode`'s `installer_associations_match_the_extension_table`
parses `fire.iss` and fails if the two sets disagree.

The thumbnails page's rows (`ListThumbFormats` in `[Code]`) mirror `fire-thumbs`'s
`formats::GROUPS`, and its `installer_thumbnail_page_matches_the_table` test holds them to it —
ids, order, which column, the extension probed on upgrade, and a caption that starts with the
table's label. A new format goes in `GROUPS` first; the test then says what the script is missing.
