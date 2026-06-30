# Fire installer

Unsigned [Inno Setup](https://jrsoftware.org/isinfo.php) installer for `fire.exe`.

## Build

```pwsh
pwsh scripts/build-installer.ps1
```

That script is the entry point — don't run `ISCC` on `fire.iss` directly. It:

1. Reads **`product.json`** (repo root) — the single source of product metadata.
2. Syncs the Cargo workspace version to match.
3. Regenerates `assets/fire.ico` from `assets/icon.png` (ImageMagick).
4. Builds `fire.exe` in release (`build.rs` embeds the same `product.json` values).
5. Writes `installer/product.generated.iss` — the `#define`s `fire.iss` `#include`s (git-ignored).
6. Compiles `fire.iss` with `ISCC` → **`dist/Fire-<version>-Setup.exe`**.

Flags: `-SkipBuild` (reuse the existing `target/release/fire.exe`), `-SkipIcon`.

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

## Editing associations

Supported formats and their extensions live in `fire.iss` (`[Tasks]` + `[Registry]`). When
`fire-decode` gains a format, add a task and its per-extension registry rows there.
