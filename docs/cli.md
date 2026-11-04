# fire-cli

`fire-cli` is Asset Check's image checker. It checks images against a technical artist's rules and
says, in plain language, what is wrong, why it matters and how to fix it. It reads images with
fire-decode, Fire's own decoder, so it sees exactly what the viewer sees. It is a console program
shipped beside the viewer, built for the Asset Check `pre-commit` hook, the Fork command, CI and
Launcher tools; `3d-review-cli` is its counterpart for models.

```text
fire-cli check <files>... [--rules <rules.json> | --profile <file>] [--results <file>]
fire-cli check --staged [--rules <rules.json> | --profile <file>] [--results <file>]
fire-cli check --files-from <list> [--rules <rules.json> | --profile <file>] [--results <file>]
fire-cli info <files>...
fire-cli rules check <rules.json>
fire-cli rules explain <files>... --rules <rules.json>
fire-cli --version | --help
```

## Where it is installed

| | |
|---|---|
| Windows | `fire-cli.exe` beside `fire.exe`, in Fire's install folder (`%LOCALAPPDATA%\Programs\Fire` for the default per-user install). |
| macOS | `Fire.app/Contents/MacOS/fire-cli`, signed with the hardened runtime like the viewer. |

Neither installer puts it on the PATH. Asset Check's setup records where it is
(`.git/asset-check/tools.env`), so the hook never depends on a PATH it doesn't control.

## The contract

`3d-review-cli`'s contract, so a tool that drives one drives the other the same way.

- **Long options only, no prompts, no standard input.** Nothing ever waits for an answer.
- **Exit status 0**: nothing at warning level or worse was found.
- **Exit status 1**: something at warning level or worse was found, a file couldn't be checked, the
  rules couldn't be read, or the results couldn't be written. Info-level findings never fail a run.
- **Exit status 2**: the command line couldn't be followed (an unknown option, an option with no
  value, no images given, `--rules` with `--profile`). Nothing ran and no results were written.
- **`--results <file>`** writes Asset Check's results (below). Results an earlier run left at that
  path are removed first, on a refused command line too, so after any run the file is this run's
  or is not there.
- **A run never stops early.** Every image is tried, and the exit status gives the verdict at the
  end.
- **Plain lines.** Results go to standard output, problems to standard error, each a whole line.
  Writes to a closed pipe are ignored.
- **An empty value is the same as leaving the option out**, so a form can always pass every option.
  A value is written `--option value` or `--option=value`. `--` ends the options. Relative paths
  resolve against the working folder.

## check

```text
fire-cli check --staged --rules rules/rules.json --results .git/asset-check/results-fire-cli.json
fire-cli check T_Crate_D.png T_Crate_N.png --profile rules/textures/prop.json
fire-cli check --files-from changed.txt --rules rules/rules.json
```

| Option | |
|---|---|
| `--rules <rules.json>` | Asset Check's rules. Each image's type, profile and stage come from its name, its folder and the branch, through the `asset-rules` crate, so `3d-review-cli` resolves every file the same way. |
| `--profile <file>` | One texture rules file (below) for every image. |
| `--staged` | The images staged for the next commit in the repository the current folder is in, checked as staged. Takes no files and no `--files-from`. |
| `--files-from <list>` | A file listing images, one per line. Windows limits a command line to about 32,000 characters, which a big commit can exceed. |
| `--results <file>` | Write Asset Check's results to this file. Its folder is created if needed. |

With neither `--rules` nor `--profile`, Fire's built-in defaults apply: every rule at its default
(below).

**How images are read.** At full size, never downscaled, and with no colour-profile conversion, so
the alpha scan sees every pixel as stored and the colour-profile test sees the profile the file
carries. Images are decoded in parallel, at most four at once and never more than there are cores,
each with its share of the cores for the decoder's own threaded passes. A 20-texture commit of 1K–4K
PNGs and TGAs (146 MB) measured 0.3 s, and 0.13 s from the cache, on a 24-core PC.

**With `--rules`:**

- A file whose type belongs to another tool (a `.fbx` is `3d-review-cli`'s), or whose extension no
  type lists, is left out: not checked and not in the results. A file named on the command line
  gets a line saying why; a staged one is passed over silently.
- A file of this tool's type whose name matches no profile gets the `file.no_profile` finding: a
  warning, or an error in enforce mode, whose "how" lists the name patterns that would match.
- Each texture rules file is read once per run, however many images use it.

**With `--staged`:**

- With `--rules`, the rules' types decide which staged files are this tool's. Without, every staged
  file with an extension Fire opens is checked.
- The index is read the way git itself reads it, `GIT_INDEX_FILE` included, so inside a
  `pre-commit` hook for `git commit <paths>` it is the commit's own index.
- An image with no unstaged changes is read in place. One changed again after staging is read from
  a temporary copy of what is staged; for a Git LFS file, from its object in `.git/lfs/objects/`.
- With `--rules`, each image's result is cached in `.git/asset-check/cache/`, keyed on the staged
  content, the texture rules file that applied, this tool's version and the stage. Checking the same
  commit again is all but instant (the git commands are most of what is left), and the Fork command
  "Check staged files" fills the cache the hook then reads. Entries unused for 30 days are removed.

Each image's lines name the profile and stage it was checked as, then each finding, worst first,
with how to fix it:

```text
Checking 3 images against rules/rules.json
[1/3] Assets/Art/T_Crate_D.png
  Checked as Prop texture (prop-texture), production stage, from the branch
  error: This image is 4096×4096. The most allowed is 2048×2048.
    How: Scale it down so neither side is more than 2048 pixels: in Photoshop, Image > Image Size. Then save it again.
[2/3] Assets/Art/T_Crate_N.png
  Checked as Normal map (normal-map), production stage, from the branch
  No problems found.
[3/3] Assets/Art/ComfyUI_00042_.png
  warning: "ComfyUI_00042_" doesn't match the name of any texture type.
    How: Rename the file so its name, without the extension, matches one of these (* stands for any text): T_*_N (Normal map), T_* (Texture).
3 images checked: 1 error, 1 warning.
```

## Texture rules files

The rules file a profile names in `rules.json` (and the file `--profile` takes) uses
`3d-review-cli`'s profile envelope:

```json
{
  "version": 1,
  "generator": "hand-written",
  "profile": {
    "name": "Prop texture (production)",
    "rules": {
      "texture.max_size":     { "severity": "error", "params": { "max": 2048 },
                                "note": "Quest budget for props." },
      "texture.power_of_two": { "severity": "error" },
      "texture.unused_alpha": {},
      "texture.format":       { "params": { "allowed": ["png", "tga", "psd"] } }
    }
  }
}
```

It is read leniently, since these files are written by hand:

- A rule listed is on, unless it says `"enabled": false`.
- Whatever a rule leaves out (`severity`, any parameter) is the tool's default for it. A parameter of
  the wrong kind (`"max": "big"`) keeps its default too.
- A rule this tool doesn't know is skipped, with a warning on standard error, so a misspelt id is
  noticed. A rule whose settings can't be read takes its defaults, also with a warning.
- A rule the file doesn't list takes the tool's default. Only the rules about something Unity gets
  wrong whatever the team's policy are on by default: `texture.format`, `texture.psd_composite` and
  `texture.color_profile`. Every other rule is off until a file lists it.
- `note` is the technical artist's note, shown under the tool's own text.
- A `version` other than 1 is refused: the file is newer than this tool.

## The rules

| Rule | Finds | Parameters (default) | Default |
|---|---|---|---|
| `texture.format` | A format not in `allowed`, judged by the file's content, not its extension. Spellings are one format: `jpg`/`jpeg`, `tif`/`tiff`. | `allowed`: the formats Unity's importer reads that Fire decodes (`png`, `jpg`, `tga`, `psd`, `tiff`, `exr`, `hdr`, `bmp`, `gif`) | on, error |
| `texture.max_size` | Either side over `max` pixels | `max` (4096) | off, error |
| `texture.min_size` | Either side under `min` pixels | `min` (16) | off, warning |
| `texture.power_of_two` | A side that isn't a power of two | - | off, warning |
| `texture.multiple_of_4` | A side that isn't a multiple of 4 | - | off, warning |
| `texture.square` | Width and height that differ | - | off, warning |
| `texture.unused_alpha` | An alpha channel in which every pixel is fully opaque | - | off, warning |
| `texture.alpha_required` | No alpha channel, or one that is fully opaque | - | off, error |
| `texture.bit_depth` | A bit depth not in `allowed`; and a palette image, unless `palette` is true | `allowed` (`[8]`), `palette` (false) | off, warning |
| `texture.color_profile` | An embedded ICC profile that isn't sRGB (Unity ignores profiles) | - | on, warning |
| `texture.generation_metadata` | An AI generator's data: PNG text chunks (`parameters`, `prompt`, `workflow`, InvokeAI's, Fooocus's, NovelAI's), or XMP marking the image as made by generative AI | - | off, warning |
| `texture.file_size` | A file over `max_mb` megabytes | `max_mb` (50) | off, warning |
| `texture.psd_composite` | A PSD saved without Maximize Compatibility, so it has no merged image for Unity to import | - | on, error |
| `texture.psb` | A PSB (Large Document Format) file | - | off, error |

- **Sizes** are a surface's: for a DDS cubemap, array or volume, one face's.
- **Bit depth** is the file's own: a 4-bit palette PNG is 4, a half-float EXR 16 (float). `allowed`
  lists bits per channel.
- **A colour profile** is sRGB when converting from it to sRGB moves no colour by more than one 8-bit
  code, whatever its name or byte layout. A greyscale profile is sRGB when its tone curve is.
- **A PSB, and a PSD with no merged image,** can't be decoded, so they are checked on what their
  header says: the size, bit depth, format and file-size rules run, and the rules that need pixels
  (alpha, colour profile) don't.

## info

```text
fire-cli info T_Crate_D.png
```

What Fire's decoder sees in each image, as the rules measure it:

```text
T_Crate_D.png
  Format        PNG
  Size          2048×2048
  Bit depth     8 bits per channel
  Channels      RGBA (the alpha is fully opaque)
  ICC profile   "Display P3" (not sRGB)
  File size     4.2 MB
  AI metadata   a "parameters" text chunk
```

Exit status 1 if a file couldn't be read.

## rules check and rules explain

For the technical artist who writes `rules.json`. Both are the `asset-rules` crate's own, so
`3d-review-cli rules ...` says the same thing about the same file.

- **`rules check <rules.json>`** lists the problems in a configuration: an unknown type, stage or
  tool, a stage with no rules file, a rules file that doesn't exist, an invalid pattern, a profile
  that can never match, duplicate ids, and a tool older than `requires` asks for. Exit status 1 on
  any problem, warnings included, so a CI step that runs it stops on one.
- **`rules explain <files>... --rules <rules.json>`** says, for each file, which type, profile,
  stage and rules file apply and why: every profile tried and why it did or didn't match, and where
  the stage came from. Files are resolved against the repository the current folder is in, and its
  branch.

## Results

`--results` writes `asset-check.results` version 1, the format both checkers write, defined and
written by the `asset-rules` crate. The Launcher's Asset Check window, the Unity tool and CI all read
it.

```json
{
  "schema": "asset-check.results",
  "version": 1,
  "tool": { "name": "fire-cli", "version": "0.6.1" },
  "rules": { "commit": "1a2b3c4", "digest": "sha256:…" },
  "context": { "branch": "main", "mode": "advise" },
  "findings": [],
  "files": [
    {
      "path": "Assets/Art/T_Crate_D.png",
      "type": "texture", "profile": "prop-texture", "profile_label": "Prop texture",
      "stage": "production", "stage_from": "branch",
      "status": "checked", "problem": null,
      "findings": [
        {
          "rule": "texture.max_size", "severity": "error", "title": "Too large",
          "what": "This image is 4096×4096. The most allowed is 2048×2048.",
          "why": "Every doubling of width and height takes four times the memory and download size in the game.",
          "how": "Scale it down so neither side is more than 2048 pixels: in Photoshop, Image > Image Size. Then save it again.",
          "note": "Quest budget for props.",
          "measured": { "width": 4096, "height": 4096 }, "limit": { "max": 2048 },
          "objects": [], "fix": null
        }
      ]
    }
  ]
}
```

- **`path`** is repository-relative with `/` separators; outside a repository, the path as given.
- **`status`** is `checked`, or `unreadable` with `problem` saying why (a file that isn't an image
  Fire can read, a texture rules file that can't be read, staged content that can't be read).
- **The top-level `findings`** are about the run rather than one file: `tool.too_old` when
  `rules.json` asks for a newer `fire-cli`.
- **Without `--rules`**, each file's `type` is `texture`, `profile` is the `--profile` path as given
  (or `default` for the built-in defaults), `profile_label` is the rules file's `name`, `stage` is
  `null`, `rules.digest` is the digest of the rules used, and `mode` is `advise`.
- **`fix`** is always `null`: automatic fixes come later.
