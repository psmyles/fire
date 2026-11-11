# fire - Work Plan

**Status, 10 November 2026:** A1 and A2 are done and merged into `main`, and A2's end-to-end check
against review3d-cli has passed on Windows. A3, the rules catalog, is done and merged. Asset Check
is done across its repositories, so B is next. B.1's macOS baselines are recorded (fire-decode's
`docs/baselines.md`), and B.2 to B.6 are done on the `fire-decode` branch (10 November 2026),
against fire-decode's steps 3, 6 and 7. fire-cli depends on asset-rules
`v0.2.1`.

Open items, in order:
1. **A2 on the Mac:** run `build-mac.sh` and `dev-app.sh`, and `fire-cli` inside the bundle (A2's
   last section).
2. **B, the migration.** The Launcher's results table and Asset Check's rules repo,
   SandboxVR/exp-content-rules, are done. What fire's side still needs is Windows: the merge onto
   fire-decode's first tag, and the checks in "Needs Windows".

`docs/fire-decode-plan.md` is kept in step with the canonical copy; on 10 November 2026 it gained
steps 3-7 as built.

The work is ordered for a stretch on a Mac; items that need Windows are collected at the end.

## Context

Two designs touch this repository:

- **Asset Check** checks art assets committed to a Unity project against a technical artist's rules,
  and shows artists what is wrong, why, and how to fix it. Fire's part is a new console tool,
  **`fire-cli`**, that checks images using fire-decode.
  - Design: `docs/asset-check-plan.md` in the rules repo,
    [SandboxVR/exp-content-rules](https://github.com/SandboxVR/exp-content-rules) (private).
    Decisions are numbered AC1-AC19.
- **The fire-decode migration** moves `crates/fire-decode`, `crates/psd-sdk-sys` and
  `crates/heif-sys` into their own repository, as the decoder shared by fire, 3D Review and bite.
  - Design: [`docs/fire-decode-plan.md`](fire-decode-plan.md). This is a copy of the canonical plan in
    psmyles/fire-decode; when either changes, update the other. Decisions are numbered FD1-FD20.
  - Most migration work happens in the fire-decode repo; see its `docs/PLAN.md`.

**Priority: Asset Check first.** Its pieces are plain Rust that work fully on a Mac, and they unblock
the artist pilot. The migration comes after, starting with what a Mac can do.

**Order across repositories:**
1. `asset-rules` - its `docs/PLAN.md`;
2. this repo's A1-A2 (done), alongside 3D Review's `review3d-cli` changes (done);
3. the Launcher's results table (done), then Asset Check's rules repo (done);
4. the migration.

**Do A1 before the fire-decode extraction** (`git filter-repo`), so the additions travel with the
crate's history. A1 is done and in `main`, so the extraction can start from `main`.

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

### A2. `fire-cli` - done, except the Mac checks below

`crates/fire-cli`, a console binary installed beside the viewer (AC12). [`docs/cli.md`](cli.md)
documents it in full.

- **Dependencies:** `fire-decode` (with `psd`/`heif` pass-through features, on by default, so CI's
  decoder-free job builds it), and `asset-rules` by tag (`v0.2.0` since A3, `v0.2.1` since 3D Review's checker became `review3d-cli`). asset-rules is public,
  so CI fetches it without credentials.
- **Commands:** `check <files>... | --staged | --files-from <list>` with `--rules`, `--profile` or
  neither, and `--results`; `info`; `rules check` / `rules explain`; `--version` / `--help`.
  - The contract, exit codes and option rules are review3d-cli's, matched against its in-progress
    AC13 work.
  - With `--rules`, other tools' files are left out of the results. `--staged` reads the staged
    content and, with `--rules`, uses the cache.
- **Decoding:** full size, no ICC conversion, at most 4 files at once (never more than the cores),
  each decode limited to its share of the cores (`with_max_threads`).
- **Rules:** all 14 v1 rules.
  - A PSB, or a PSD with no merged image, can't be decoded. It is checked on its header: size, depth,
    format and file size.
- **Rules files:** the review3d-cli envelope, read leniently. A listed rule is on; omitted fields
  take the defaults; unknown or malformed rules are skipped with a warning.
- **Decision: rules a file doesn't list take the tool's default.** The defaults are on only for what
  Unity gets wrong whatever the team's policy is:
  - `texture.format`: Unity's importable formats that Fire decodes;
  - `texture.psd_composite`;
  - `texture.color_profile`.
  - Everything else is off until listed. The same defaults apply with neither `--rules` nor
    `--profile`.
- **Messages:** all of them in `src/messages.rs` (AC11); since A3, the findings' texts are in
  `rules-catalog.toml`, with the rules.
- **Tests:**
  - 33 unit tests, and 7 end-to-end tests of the binary on generated fixtures: opaque alpha, 16-bit,
    Display P3, a `parameters` text chunk, WebP, and a PSD without a merged image;
  - `--staged` in a scratch git repository, with a partially staged file read as staged, and the
    cache;
  - all of them in CI's decoder-free job too, except the PSD fixture.
- **Packaging:**
  - Windows: `fire.iss` and `build-installer.ps1` install `fire-cli.exe` beside `fire.exe`. The
    "Add fire-cli to PATH" task, on by default, puts that folder on the user's PATH; Asset Check
    doesn't rely on it. ISCC compiles the installer with it included (verified on Windows).
  - macOS: `build-mac.sh` builds `fire-cli`, copies it to `Fire.app/Contents/MacOS/`, and signs it on
    its own with the hardened runtime before signing the bundle. `dev-app.sh` copies it too.
  - CI lints and tests it in both jobs, and builds it in the release step.
- **Notices:** the 17 crates it adds are in `THIRD-PARTY-NOTICES.md` (184 crates now) and
  `CREDITS.md`. `serde_json` is now linked, no longer build-time only.

*The gate* (Asset Check step 2, fire's half):
- **Results validate:** every test reads its results back through asset-rules' `Results::read`,
  which checks the schema and version.
- **Speed:** 20 real 1K-4K textures (146 MB) staged in a scratch repository took 0.3 s cold and
  0.13 s cached, on a 24-core PC with a release build.
- **The same profile and stage as review3d-cli:** true by construction (both call asset-rules'
  `resolve`), covered by asset-rules' own step-1 test, and checked end to end on Windows on
  5 November 2026 with both release builds:
  - **The setup:** a scratch repository with asset-rules' fixture `rules.json`, four models (environment
    prop by path, prop, character, and a name matching no profile) and three textures (normal map,
    texture, no match).
  - **`rules explain`:** both tools printed identical output for every file, on `main`, `proto/x`, a
    feature branch and a detached HEAD. That covers each stage source (branch, profile, project) and
    `file.no_profile`.
  - **`check --staged` / `audit --staged`:** on new staged files, both gave the same profile, stage and
    `stage_from`; both filled the cache. Each run took about 140 ms.

**Still to do on the Mac:**
- Run `scripts/build-mac.sh`: the nested `fire-cli` signature, notarization, and `codesign --verify`.
- Run `scripts/dev-app.sh`.
- Run `fire-cli` from inside the bundle.

### A3. The rules catalog - done

The rules repo's `docs/rule-editor-plan.md`, step 2: every rule is described once, in
`rules-catalog.toml` at the repository root, so adding or changing one is easy and Rule Editor
learns the rules from `fire-cli rules catalog`. Fire 0.7.0 ships with it.

- **The catalog:** 14 rules in 6 categories, their parameters, defaults and texts (title, what,
  why, how, and the cases a check picks).
- **`build.rs`** has asset-rules check the catalog (no rule newer than `product.json`'s version) and
  generate the rule code: `RuleId`, typed parameters (`params::MaxSize { max }`), and what each
  check hands its texts (`found::MaxSize { width, height }`). The checks in `checks.rs` stay
  hand-written; a rule added to the catalog fails the build until its check exists.
- **Rules files** are read as asset-rules' `Rule::settings` reads them. One change: a parameter value
  out of range keeps its default, where it used to switch the rule off.
- **`rules catalog`** prints the catalog; **`rules check`** reads each texture rules file against it.
- **`rules.json` version 2:** files `check` and `skip` leave out are left out.
- **Two wording fixes:** "an 8-bit palette image" and "an OpenEXR file" (also AVIF and ICO).

*The gate:* CrimeCrew's 496 textures, checked before and after with release builds: the same
results and output against its `rules.json` and against the defaults (digest included); with every
rule on, only the two wording fixes differ (6 findings).

It depends on asset-rules `v0.2.0`, tagged 5 November 2026; `v0.2.1` since 6 November, which names 3D Review's checker `review3d-cli`.

## B. The fire-decode migration: fire's side, after A

1. **macOS baselines (step 0):** record TTFP per format, HEIC and AVIF included, with
   `scripts/ttfp.sh`. Windows baselines come later.
2. **Make the tonemap safe for negative values (step 6a). Done on the `fire-decode` branch**
   (`1a88bbb`, 10 November 2026).
   - `reinhard(c) = c / (1 + c)` (`crates/fire/src/render/shader.glsl:75`, and the copy in
     `crates/fire-thumbs/src/render.rs`) returned infinity at −1 and a large positive value below
     it.
   - Both tone curves, Reinhard and ACES, now clamp negatives first, in the shader and in
     fire-thumbs, with a test of negative inputs and inputs below −1. ACES needed it too: a large
     negative came out white.
3. **Develop fire against the new fire-decode repo** on a branch, through a `[patch]` in a
   `.cargo/config.toml` in the folder that holds both checkouts. fire's own `.cargo/config.toml` is
   committed, so it isn't the place (3D Review's `docs/cli.md`, "Building against unreleased
   asset-rules", shows the pattern). fire's `main` keeps its in-tree crates until migration step 1's gate
   passes on **both** platforms; that needs the Windows HEIF libraries.
   - **Done (10 November 2026): the `fire-decode` branch.** It removes `crates/fire-decode`,
     `psd-sdk-sys` and `heif-sys`, and depends on
     `git = "https://github.com/psmyles/fire-decode", branch = "main"`. An untracked
     `~/Dev/.cargo/config.toml` patches that to the checkout beside this one.
   - fire, fire-cli and fire-shellext name the crate's `icc` and `raw` features, since the crate
     has none on by default.
   - The installer-association test moved into `crates/fire/tests/installer.rs`, against
     `fire_decode::all_extensions()`.
   - The workspace dependencies only the in-tree crates used are gone, `bindgen` among them.
   - **Builds need Rust 1.99**, the crate's floor; this Mac's `stable` is 1.99.1.
   - **CI isn't updated on the branch.** Its jobs still name `-p fire-decode` and the vendored
     artifacts. They move when the branch merges for the tag (fire-decode's `docs/PLAN.md`, "Needs
     Windows", item 7).
**Behaviour changes fire picks up with fire-decode's tag** (step 2a, 10 November 2026): a PSD with
a malformed RLE run is refused rather than shown with that plane blacked out, and an RGB / CMYK PSD
declaring fewer channels than its colour mode needs is refused rather than filled from plane 0.
Both are malformed files Photoshop doesn't write.

4. **Move each consumer to its setup (step 3). Done on the `fire-decode` branch** (10 November
   2026); the macOS gate passed (fire-decode's `docs/baselines.md`). Details:
   - **fire-shellext's setup:** it is `fire_thumbs::decode_options()`, so a test on every OS checks
     that a cubemap's thumbnail draws all six faces. fire-shellext itself builds only on Windows,
     so its changed call waits for a Windows build.
   - **The viewer's setup:** one `app::decode_options()`, `viewer()` with the RAM-guard fit at
     `MAX_CPU_DIM`.
   - **The loading bar** gets the crate's progress through a `DecodeCtl`, whose cancel never fires
     until step 6d.
   - **Without `heif`, `.avif` and `.heic` aren't listed:** the extension lists follow the build's
     features. That matters only to CI's decoder-free job.

   The plan for each consumer:
   - **The viewer** to `viewer()`.
   - **fire-shellext** to `viewer()` with `frames: First`, `stored_mips: false` and `hdr: Sdr`. It
     keeps `surfaces: Sheet`, so a cubemap's thumbnail still shows every face.
     - From fire-decode's step 6b, `Sdr` converts a PQ / HLG HEIF to SDR (203 cd/m² at white,
       sRGB-encoded). Today its thumbnail shows the raw HDR signal, flat and dark (FD17, settled
       10 November 2026). The viewer opens those files as HDR (`Linear`) from the same step.
   - **fire-cli** to `inspect()`: full size, no ICC conversion, EXIF orientation off.
     - A rotated JPEG then reports its stored size, which is what Unity imports. Today it reports
       the upright size. Add a test for it.
     - Drop the per-worker `with_max_threads` share; the crate's budget divides the cores.
     - The facts it reads (`bit_depth`, `channels`, `palette`, `source_format`) move to
       `DecodedImage::source`.

5. **HDR, gain maps and sequences (steps 6b-6d). Done on the `fire-decode` branch** (10 November
   2026); fire-decode's `docs/baselines.md` has the gates.
   - **6b** (`28a892d`): a PQ / HLG HEIC or AVIF opens as HDR. fire asks for `Hdr::Linear` from
     what `probe()` reports, before the decode.
   - **6c** (`1ccce64`): a gain-map photo opens as its SDR base. `H` (`toggle-hdr`) switches the
     window to HDR mode and decodes it again with its gain map applied, keeping the view.
   - **6d** (`94bc48e`): animated AVIF / HEIF play as GIFs do, each for its stated loop count. A
     decode the window has moved on from is cancelled.

6. **The mip builder moves to fire-decode (step 7). Done on the `fire-decode` branch**
   (`c66ac58`, 10 November 2026).
   - `render/mips.rs`'s builder is fire-decode's `mips` feature (FD11), which 3D Review now
     shares. `render/mips.rs` keeps the upload side: the level count, the texel width and the
     half-float conversions.
   - 8-bit chains are byte-identical to before. 16-bit chains round exactly now, up to 2 in
     65535 apart. Half-float chains are now right at subnormals, which the old table halved.
   - The builder is faster on every format: 8-bit by 20%, half float 4×. TTFP is within noise.

## Needs Windows (later)

- **Done 11 November 2026** (fire-decode's `docs/baselines.md`, "Windows"):
  - the Windows TTFP baselines, and every fire-decode step's TTFP gate on this branch: every
    format within 2% of `710b3f6` or faster;
  - the static CRT for fire, fire-shellext and fire-cli (FD20, `.cargo/config.toml`): none of the
    three imports `MSVCP140`, `VCRUNTIME140*` or `api-ms-win-crt-*`;
  - fire-shellext's decode path, driven directly: HEIC and AVIF thumbnails upright,
    `pq_lossless.avif` as SDR.
- fire's switch to the fire-decode tag (step 1): done on this branch, 11 November, onto
  `v0.1.0`; it lands on `main` with the branch's merge. The installer script runs plain `cargo`,
  so the machine building it needs a `stable` of at least 1.99 (`rustup update stable`).
- Hands-on: Explorer thumbnails through the new `fire_shellext.dll` (HEIC, AVIF, PQ as SDR, a
  cubemap DDS's every face); the tone curves on an EXR and a PQ file; `H` on an iPhone HEIC;
  sequences' loop counts and cancel.
- Verifying the installer changes:
  - run the installer and check that `fire-cli.exe` lands beside `fire.exe` and runs. A2 only
    compiled the installer with ISCC.
  - FD20's: no runtime DLL needed on a clean machine.
- Moving the installer-association test out of fire-decode into a fire crate (FD2, FD9): done on
  this branch in step 3.
- Steps 6 and 7 on Direct3D 11 (fire-decode's `docs/PLAN.md`, "Needs Windows", items 13 and 14):
  the regenerated HLSL compiles and TTFP with the new mip builder is within 2%, done 11 November.
  HDR mode and sequences are among the hands-on checks above.

## References

- [`docs/fire-decode-plan.md`](fire-decode-plan.md): the migration design (a copy; canonical in
  psmyles/fire-decode).
- `docs/asset-check-plan.md` in SandboxVR/exp-content-rules: the Asset Check design.
- [`docs/architecture.md`](architecture.md): fire's architecture.
- `docs/PLAN.md` in psmyles/fire-decode, psmyles/asset-rules, psmyles/3d-review, psmyles/launcher
  and SandboxVR/exp-content-rules: the related work there.
