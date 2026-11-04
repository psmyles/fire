# fire - Work Plan

**Status, November 2026:** A1 is done; the rest hasn't started. asset-rules is tagged `v0.1.0`.
The work is ordered for a stretch on a Mac; items that need Windows are collected at the end.

## Context

Two designs touch this repository:

- **Asset Check** checks art assets committed to a Unity project against a technical artist's rules,
  and shows artists what is wrong, why, and how to fix it. Fire's part is a new console tool,
  **`fire-cli`**, that checks images using fire-decode.
  - Design: `asset-check-plan.md` in [psmyles/fire-decode](https://github.com/psmyles/fire-decode)
    (private; it moves to the rules repo later). Decisions are numbered AC1-AC19.
- **The fire-decode migration** moves `crates/fire-decode`, `crates/psd-sdk-sys` and
  `crates/heif-sys` into their own repository, as the decoder shared by fire, 3D Review and bite.
  - Design: [`docs/fire-decode-plan.md`](fire-decode-plan.md). This is a copy of the canonical plan in
    psmyles/fire-decode; when either changes, update the other. Decisions are numbered FD1-FD20.
  - Most migration work happens in the fire-decode repo; see its `docs/PLAN.md`.

**Priority: Asset Check first.** Its pieces are plain Rust that work fully on a Mac, and they unblock
the artist pilot. The migration comes after, starting with what a Mac can do.

**Order across repositories:**
1. `asset-rules` - its `docs/PLAN.md`;
2. this repo's A1-A2, alongside 3D Review's `3d-review-cli` changes;
3. the Launcher's results table;
4. the migration.

**Do A1 before the fire-decode extraction** (`git filter-repo`), so the additions travel with the
crate's history.

## Before you start on the Mac

- **Full Xcode** plus the Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`), checked by
  *running* `xcrun -sdk macosx metal --version` (README, "Toolchain prerequisites"). Rust stable via
  rustup. bindgen finds Xcode's libclang by itself.
- **The vendored HEIF libraries for macOS were built for macOS 27.0** (`LC_BUILD_VERSION minos 27.0`).
  On an older macOS the linker warns and the app may fail at run time. The migration's re-vendor
  targets macOS 11.0 and fixes this (FD16).
- **Scripts:** `scripts/ttfp.sh` (time to first pixel), `scripts/build-mac.sh` (app bundle),
  `scripts/dev-app.sh` (development bundle).

## A. Asset Check

### A1. Three small fire-decode additions - done

All in `crates/fire-decode`, needed by `fire-cli`'s rules (AC12).

1. **`pub fn icc_is_srgb(profile: &[u8]) -> bool`.** It runs the ICC pass's sRGB probe with the
   relative colorimetric intent, so `texture.color_profile` can tell a Display P3 or Adobe RGB profile
   from an sRGB one.
   - Greyscale profiles are probed too: sRGB when the tone curve is sRGB's.
   - Every other colour space returns `false`.
2. **True PNG bit depth.** `DecodedImage::bit_depth` is the IHDR depth (1/2/4/8/16), and the new
   `DecodedImage::palette` is set for colour type 3. The decoded buffer is unchanged.
   - Only PNG sets `palette` so far.
   - The viewer's status bar now shows a 4-bit PNG as 4-bit.
3. **`DecodeError::NoMergedImage`**, a plain variant now; recorded in FD10.
   - The plan's premise was only half right. psd_sdk's `NoMergedImage` fires only when the image
     data section is missing, and none of 2,029 PSDs on the dev machine lacks one.
   - Photoshop, saved without Maximize Compatibility, writes a **white placeholder** and clears
     `hasRealMergedData` in the Version Info resource (1057). fire-decode reads that flag before
     psd_sdk runs, at about 0.1 µs a file. Both cases return the new variant.
   - In the viewer such a file is now an error with that message, not a white canvas. Its layers
     can't be browsed, since layer browsing starts from a decoded composite.

Each addition has tests. The workspace tests pass with default features and with
`--no-default-features`.
- TTFP wasn't re-measured. The added work is a 14-byte header read for PNG and a resource-section
  walk for PSD, with no pixel work.

### A2. `fire-cli`

A new crate, `crates/fire-cli`, with a console binary named `fire-cli` (no `windows_subsystem`). It is
installed beside the viewer, the way `3d-review-cli` sits beside `3d-review.exe` (AC12).

- **Dependencies:**
  - `fire-decode` from the workspace, with `psd` and `heif`;
  - `asset-rules` by git tag once `v0.1.0` exists. Until then, an untracked `.cargo/config.toml`:

    ```toml
    [patch."https://github.com/psmyles/asset-rules"]
    asset-rules = { path = "../asset-rules" }
    ```

- **Commands:**
  - `fire-cli check <files>... | --staged | --files-from <list>`, with `--rules <rules.json>` or
    `--profile <file>`, and `--results <file>`;
  - `fire-cli info <file>` - prints what fire-decode sees;
  - `fire-cli rules check <rules.json>` and `fire-cli rules explain <file>`, from asset-rules;
  - `--version` and `--help`.
  - `--files-from` exists because Windows limits a command line to about 32,000 characters.
- **Exit codes:** the same as 3d-review-cli's - 0 when nothing at warning level or worse was found, 1
  when something was found or a file couldn't be checked, 2 when the command line can't be followed.
- **Decoding:**
  - full size (`max_dim: 0`) with ICC conversion off (`honor_icc: false`), so the alpha scan and the
    colour-profile test see the real image;
  - files decoded in parallel on a small pool. The decode itself already threads its big passes, so
    don't run more files at once than there are cores.
- **Rules for v1** (AC12 has the full table):
  - `texture.format`
  - `texture.max_size` / `texture.min_size`
  - `texture.power_of_two` / `texture.multiple_of_4` / `texture.square`
  - `texture.unused_alpha` / `texture.alpha_required`
  - `texture.bit_depth`
  - `texture.color_profile`
  - `texture.generation_metadata` (AI prompts or workflows in PNG `tEXt`/`iTXt`/`zTXt` chunks, such as
    `parameters`, `prompt` and `workflow`, and in XMP; read by fire-cli itself)
  - `texture.file_size`
  - `texture.psd_composite` / `texture.psb`
- **Rules file format:** 3d-review-cli's profile envelope,
  `{version, generator, profile: {name, rules: {<id>: {enabled, severity, params, note}}}}`. Loaded
  leniently: unknown rules are skipped, and a missing rule takes the tool's default.
- **Messages** (AC11):
  - every rule has a title and what / why / how text;
  - "what" carries the measured value and the limit;
  - "how" names steps in the artist's tools (e.g. Photoshop: Image > Image Size).
  - Fire has no message catalogue, so keep every string in one module, so that a catalogue can replace
    it later.
- **Results:** written through asset-rules (shared format, AC10), with its cache used for `--staged`.
- **Tests:** generated fixtures, including:
  - opaque alpha;
  - 16-bit;
  - a Display P3 ICC profile;
  - PNG text chunks;
  - a WebP;
  - a PSD without a merged image (fire-decode's tests already build synthetic PSDs).
- **Packaging:**
  - macOS: `scripts/build-mac.sh` copies `fire-cli` into `Fire.app/Contents/MacOS/` and signs it with
    the hardened runtime, like the main binary.
  - Windows: `installer/fire.iss` and `scripts/build-installer.ps1` install `fire-cli.exe` beside
    `fire.exe`. Edit on the Mac; verify on Windows.
  - Neither installer puts it on PATH; Asset Check's setup tool records where it is (AC16).
- **Docs:** `docs/cli.md`, modelled on 3D Review's.

*Done when* Asset Check step 2's gate passes for fire's half:
- it gives the same file the same profile and stage as 3d-review-cli;
- its results validate against the schema;
- a 20-asset commit is checked in a few seconds, and almost instantly when cached.

## B. The fire-decode migration: fire's side, after A

1. **macOS baselines (step 0):** record TTFP per format, HEIC and AVIF included, with
   `scripts/ttfp.sh`. Windows baselines come later.
2. **Make the tonemap safe for negative values (step 6a).**
   - `reinhard(c) = c / (1 + c)` (`crates/fire/src/render/shader.glsl:75`, and the copy in
     `crates/fire-thumbs/src/render.rs`) returns infinity at −1 and a large positive value below it.
   - Clamp negatives before tonemapping, and add a test with negative inputs and inputs below −1.
   - This is independent of everything else, so it can be done any time.
3. **Develop fire against the new fire-decode repo** on a branch, through an untracked
   `.cargo/config.toml` `[patch]`. fire's `main` keeps its in-tree crates until migration step 1's gate
   passes on **both** platforms; that needs the Windows HEIF libraries.

## Needs Windows (later)

- Windows TTFP baselines; fire's switch to the fire-decode tag (step 1).
- The static CRT for fire and fire-shellext (FD20), checked by an import table with no `MSVCP140` /
  `VCRUNTIME140*` entries.
- Verifying the installer changes (`fire-cli.exe` installed; FD20).
- Moving the installer-association test out of fire-decode into a fire crate (FD2, FD9).

## References

- [`docs/fire-decode-plan.md`](fire-decode-plan.md): the migration design (a copy; canonical in
  psmyles/fire-decode).
- `asset-check-plan.md` in psmyles/fire-decode: the Asset Check design.
- [`docs/architecture.md`](architecture.md): fire's architecture.
- `docs/PLAN.md` in psmyles/fire-decode, psmyles/asset-rules, psmyles/3d-review and psmyles/launcher:
  the related work there.
