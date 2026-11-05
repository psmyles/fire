//! Every line `fire-cli` says, and every finding's text.
//!
//! All of it is here, and only here, so a message catalogue can replace this module later
//! without a hunt through the code (Asset Check AC11). Fire has no catalogue yet, and the text is
//! English only for now.
//!
//! The findings follow AC11: **what** gives the measured value and the limit; **why** gives the
//! consequence in the game or the pipeline; **how** names the steps in the artist's own tool.
//! Plain language and short sentences.

use asset_rules::results::Severity;
use asset_rules::StageFrom;

use crate::args::PROGRAM;

// --- command line ---------------------------------------------------------------------------

/// A refused command line: what was wrong, then where to look.
pub fn usage(problem: &str) -> String {
    format!("{PROGRAM}: {problem}\nRun `{PROGRAM} --help` for how to use it.")
}

pub fn usage_no_command() -> String {
    "no command given".to_owned()
}

pub fn usage_unknown_command(word: &str) -> String {
    format!("\"{word}\" isn't a command; the commands are check, info and rules")
}

pub fn usage_unknown_option(option: &str, command: &str) -> String {
    format!("{command} doesn't take {option}")
}

pub fn usage_missing_value(option: &str) -> String {
    format!("{option} needs a value")
}

pub fn usage_flag_value(option: &str) -> String {
    format!("{option} doesn't take a value")
}

pub fn usage_no_files() -> String {
    "no images given: name the files, or use --staged or --files-from".to_owned()
}

pub fn usage_staged_and_files() -> String {
    "--staged checks the staged images, so it takes no files and no --files-from".to_owned()
}

pub fn usage_rules_and_profile() -> String {
    "--rules and --profile both say what to check against; give one of them".to_owned()
}

pub fn usage_info_no_files() -> String {
    "info needs at least one file".to_owned()
}

pub fn usage_rules_no_command() -> String {
    "rules needs a command: check or explain".to_owned()
}

pub fn usage_rules_unknown(word: &str) -> String {
    format!("\"{word}\" isn't a rules command; they are check and explain")
}

pub fn usage_rules_check_one() -> String {
    "rules check takes one rules.json".to_owned()
}

pub fn usage_rules_explain_needs_rules() -> String {
    "rules explain needs --rules <rules.json>".to_owned()
}

pub fn usage_rules_explain_no_files() -> String {
    "rules explain needs at least one file".to_owned()
}

pub fn version(version: &str) -> String {
    format!("{PROGRAM} {version}")
}

pub fn help() -> String {
    format!(
        "\
{PROGRAM}: checks images against Asset Check's rules, with Fire's decoder.

Usage:
  {PROGRAM} check <files>... [--rules <rules.json> | --profile <file>] [--results <file>]
  {PROGRAM} check --staged [--rules <rules.json> | --profile <file>] [--results <file>]
  {PROGRAM} check --files-from <list> [--rules <rules.json> | --profile <file>] [--results <file>]
  {PROGRAM} info <files>...
  {PROGRAM} rules check <rules.json>
  {PROGRAM} rules explain <files>... --rules <rules.json>
  {PROGRAM} --version | --help

check
  Checks each image and prints what it found.
  --rules <rules.json>   Asset Check's rules: each image's profile and stage come from its name,
                         its folder and the branch.
  --profile <file>       One texture rules file for every image.
                         With neither, Fire's built-in defaults apply.
  --staged               The images staged for the next commit, as staged, in the repository the
                         current folder is in. Results are cached.
  --files-from <list>    A file listing images, one per line.
  --results <file>       Writes Asset Check's results (asset-check.results v1) to this file.

info
  Prints what Fire's decoder sees in each image.

rules
  check     Lists the problems in a rules.json.
  explain   Says which type, profile, stage and rules file apply to each file, and why.

Exit status: 0 when nothing at warning level or worse was found, 1 when something was found or a
file couldn't be checked, 2 when the command line couldn't be followed.
"
    )
}

// --- check ------------------------------------------------------------------------------------

pub fn check_start_rules(count: usize, rules: &str) -> String {
    format!("Checking {} against {rules}", images(count))
}

pub fn check_start_profile(count: usize, profile: &str) -> String {
    format!("Checking {} against {profile}", images(count))
}

pub fn check_start_defaults(count: usize) -> String {
    format!("Checking {} against Fire's defaults", images(count))
}

pub fn staged_none() -> String {
    "No images are staged.".to_owned()
}

pub fn heading(index: usize, count: usize, label: &str) -> String {
    format!("[{index}/{count}] {label}")
}

/// How a file was resolved: `Checked as Prop texture (prop-texture), production stage, from the branch`.
pub fn checked_as(label: &str, id: &str, stage: &str, from: StageFrom) -> String {
    let from = match from {
        StageFrom::Branch => "from the branch",
        StageFrom::Profile => "the profile's own",
        StageFrom::Project => "the project's",
        StageFrom::Asset => "the asset's own",
    };
    format!("Checked as {label} ({id}), {stage} stage, {from}")
}

pub fn from_cache() -> String {
    "(already checked; from the cache)".to_owned()
}

pub fn no_problems() -> String {
    "No problems found.".to_owned()
}

pub fn severity(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

pub fn finding_line(severity: Severity, what: &str) -> String {
    format!("{}: {what}", self::severity(severity))
}

pub fn how_line(how: &str) -> String {
    format!("How: {how}")
}

pub fn not_checked(problem: &str) -> String {
    format!("Couldn't check it: {problem}")
}

pub fn summary(checked: usize, unreadable: usize, counts: [usize; 3]) -> String {
    let [errors, warnings, infos] = counts;
    let mut line = format!("{} checked", images(checked));
    if unreadable > 0 {
        line.push_str(&format!(", {unreadable} couldn't be checked"));
    }
    let found: Vec<String> = [
        (errors, "error", "errors"),
        (warnings, "warning", "warnings"),
        (infos, "note", "notes"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| format!("{n} {}", if n == 1 { one } else { many }))
    .collect();
    if found.is_empty() {
        line.push_str(": no problems.");
    } else {
        line.push_str(&format!(": {}.", found.join(", ")));
    }
    line
}

fn images(count: usize) -> String {
    if count == 1 {
        "1 image".to_owned()
    } else {
        format!("{count} images")
    }
}

pub fn decode_failed(error: &str) -> String {
    format!("Fire can't read this image ({error})")
}

pub fn file_unreadable(error: &str) -> String {
    format!("the file can't be read ({error})")
}

pub fn check_panicked() -> String {
    "fire-cli failed while checking it".to_owned()
}

pub fn rules_unreadable(path: &str, error: &str) -> String {
    format!("Can't use the rules {path}: {error}")
}

pub fn profile_unreadable(path: &str, reason: &str) -> String {
    format!("Can't read the texture rules {path}: {reason}")
}

pub fn profile_version(found: u64, supported: u64) -> String {
    format!(
        "it is version {found}, but this {PROGRAM} reads version {supported}; it may need updating"
    )
}

pub fn profile_no_version() -> String {
    "it has no \"version\"; texture rules files say \"version\": 1".to_owned()
}

pub fn unknown_rule(file: &str, rule: &str) -> String {
    format!("{file}: \"{rule}\" isn't a rule {PROGRAM} knows; it is ignored")
}

pub fn malformed_rule(file: &str, rule: &str) -> String {
    format!("{file}: the settings of \"{rule}\" can't be read; it takes its defaults")
}

pub fn no_rules_file(profile: &str, stage: &str) -> String {
    format!("the profile \"{profile}\" has no rules file for the {stage} stage, and no \"*\" file")
}

pub fn staged_failed(error: &str) -> String {
    format!("Can't list the staged files: {error}")
}

pub fn staged_unreadable(error: &str) -> String {
    format!("the staged content can't be read ({error})")
}

pub fn files_from_unreadable(path: &str, error: &str) -> String {
    format!("Can't read the list {path}: {error}")
}

pub fn results_failed(path: &str, error: &str) -> String {
    format!("Can't write the results to {path}: {error}")
}

pub fn left_out_ignored() -> String {
    "left out: no type in the rules has its extension".to_owned()
}

pub fn left_out_other_tool(kind: &str, tool: &str) -> String {
    format!("left out: it is a {kind} file, which {tool} checks")
}

pub fn left_out_not_in_check() -> String {
    "left out: it isn't in the paths the rules check (\"check\")".to_owned()
}

pub fn left_out_skipped(label: Option<&str>) -> String {
    match label {
        Some(label) => format!("left out: the rules skip it ({label})"),
        None => "left out: the rules skip it".to_owned(),
    }
}

pub fn not_a_file() -> String {
    "it is a folder, not a file".to_owned()
}

pub fn defaults_name() -> String {
    "Fire's defaults".to_owned()
}

// --- rules check ------------------------------------------------------------------------------

pub fn rules_check_clean(file: &str) -> String {
    format!("{file}: no problems.")
}

pub fn rules_check_problems(file: &str, errors: usize, warnings: usize) -> String {
    let count =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    format!(
        "{file}: {}, {}.",
        count(errors, "error", "errors"),
        count(warnings, "warning", "warnings")
    )
}

// --- info -------------------------------------------------------------------------------------

pub fn info_row(name: &str, value: &str) -> String {
    format!("  {name:<14}{value}")
}

pub const INFO_FORMAT: &str = "Format";
pub const INFO_SIZE: &str = "Size";
pub const INFO_DEPTH: &str = "Bit depth";
pub const INFO_CHANNELS: &str = "Channels";
pub const INFO_PROFILE: &str = "ICC profile";
pub const INFO_FILE_SIZE: &str = "File size";
pub const INFO_MERGED: &str = "Merged image";
pub const INFO_GENERATION: &str = "AI metadata";

pub fn info_size(width: u32, height: u32, surfaces: Option<(u32, &str)>) -> String {
    match surfaces {
        Some((count, kind)) => format!("{width}×{height} ({count} {kind} surfaces)"),
        None => format!("{width}×{height}"),
    }
}

pub fn info_depth(bits: u8, float: bool, palette: bool) -> String {
    match (float, palette) {
        (true, _) => format!("{bits}-bit float"),
        (false, true) => format!(
            "{bits}-bit palette ({} colours at most)",
            palette_colours(bits)
        ),
        (false, false) => format!("{bits} bits per channel"),
    }
}

pub fn info_channels(channels: u8, alpha_opaque: Option<bool>) -> String {
    let name = match channels {
        1 => "grey",
        2 => "grey + alpha",
        3 => "RGB",
        4 => "RGBA",
        _ => "?",
    };
    match (channels, alpha_opaque) {
        (2 | 4, Some(true)) => format!("{name} (the alpha is fully opaque)"),
        (2 | 4, Some(false)) => format!("{name} (with transparency)"),
        _ => name.to_owned(),
    }
}

pub fn info_profile(description: Option<&str>, srgb: bool) -> String {
    let name = description.map_or("unnamed".to_owned(), |d| format!("\"{d}\""));
    if srgb {
        format!("{name} (sRGB)")
    } else {
        format!("{name} (not sRGB)")
    }
}

pub fn info_no_profile() -> String {
    "none (sRGB is assumed)".to_owned()
}

pub fn info_merged_missing() -> String {
    "missing (saved without Maximize Compatibility)".to_owned()
}

pub fn info_no_pixels() -> String {
    "(not read: Fire can't read this file's pixels)".to_owned()
}

pub fn info_none() -> String {
    "none".to_owned()
}

// --- shared figures -----------------------------------------------------------------------------

/// A file size as Explorer and Finder show one: `63.2 MB`, `850 KB`.
pub fn file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} bytes")
    }
}

fn palette_colours(bits: u8) -> u32 {
    1u32 << bits.min(8)
}

fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

// --- findings -----------------------------------------------------------------------------------

/// The text of one finding: title, what, why, how.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    pub title: String,
    pub what: String,
    pub why: String,
    pub how: String,
}

fn text(title: &str, what: String, why: &str, how: String) -> Text {
    Text {
        title: title.to_owned(),
        what,
        why: why.to_owned(),
        how,
    }
}

/// `texture.format`. `unity_reads` is whether Unity's texture importer reads the format at all.
pub fn format(label: &str, allowed: &[String], unity_reads: bool) -> Text {
    let allowed_upper: Vec<String> = allowed.iter().map(|f| f.to_uppercase()).collect();
    let target = allowed_upper
        .first()
        .cloned()
        .unwrap_or_else(|| "PNG".to_owned());
    let why = if unity_reads {
        "The project keeps its textures in a few agreed formats, so every texture goes through \
         the same import settings."
    } else {
        "Unity's texture importer can't read this format, so the texture would be missing in \
         the game."
    };
    text(
        "Format not allowed",
        format!(
            "This is a {label} file. Allowed formats: {}.",
            allowed_upper.join(", ")
        ),
        why,
        format!(
            "Save it as {target} instead: in Photoshop, File > Save a Copy, and pick {target} as \
             the format. Then stage the new file in place of this one."
        ),
    )
}

pub fn max_size(width: u32, height: u32, max: u32) -> Text {
    text(
        "Too large",
        format!("This image is {width}×{height}. The most allowed is {max}×{max}."),
        "Every doubling of width and height takes four times the memory and download size in \
         the game.",
        format!(
            "Scale it down so neither side is more than {max} pixels: in Photoshop, \
             Image > Image Size. Then save it again."
        ),
    )
}

pub fn min_size(width: u32, height: u32, min: u32) -> Text {
    text(
        "Too small",
        format!("This image is {width}×{height}. The least allowed is {min}×{min}."),
        "A texture this small looks blurred or blocky on screen.",
        format!(
            "Make it at least {min} pixels on each side, ideally by exporting it again from the \
             source at a larger size rather than scaling it up."
        ),
    )
}

pub fn power_of_two(width: u32, height: u32, suggested: (u32, u32)) -> Text {
    let (w, h) = suggested;
    text(
        "Not a power of two",
        format!(
            "This image is {width}×{height}. Both sides must be a power of two \
             (… 256, 512, 1024, 2048 …)."
        ),
        "Unity rescales a texture that isn't a power of two, or can't compress it or make \
         mipmaps for it on every platform, so it ends up blurrier or takes more memory.",
        format!(
            "Resize it to powers of two, for example {w}×{h}: in Photoshop, Image > Image Size \
             (or Image > Canvas Size to add or trim space instead of scaling). Then save it again."
        ),
    )
}

pub fn multiple_of_4(width: u32, height: u32, suggested: (u32, u32)) -> Text {
    let (w, h) = suggested;
    text(
        "Not a multiple of 4",
        format!("This image is {width}×{height}. Both sides must be a multiple of 4."),
        "Texture compression works on 4×4 blocks. A texture whose sides aren't multiples of 4 \
         can't be compressed on every platform, so it stays uncompressed and takes far more \
         memory.",
        format!(
            "Resize it to multiples of 4, for example {w}×{h}: in Photoshop, Image > Canvas Size. \
             Then save it again."
        ),
    )
}

pub fn square(width: u32, height: u32) -> Text {
    let side = width.max(height);
    text(
        "Not square",
        format!("This image is {width}×{height}. It must be square."),
        "Textures of this type are expected to be square, and some platforms compress only \
         square textures.",
        format!(
            "Make it square, for example {side}×{side}: in Photoshop, Image > Canvas Size. Then \
             save it again."
        ),
    )
}

pub fn unused_alpha() -> Text {
    text(
        "Alpha channel not used",
        "It has an alpha channel, but every pixel is fully opaque.".to_owned(),
        "An alpha channel makes Unity pick a format that stores it, which takes up to twice the \
         memory, for no visible difference.",
        "Remove the alpha channel: in Photoshop, delete it in the Channels panel (or flatten the \
         image with Layer > Flatten Image), then save it again. In Export As, untick \
         Transparency."
            .to_owned(),
    )
}

pub fn alpha_required(has_channel: bool) -> Text {
    let what = if has_channel {
        "Its alpha channel is fully opaque, but this type of texture needs transparency or a \
         mask in alpha."
    } else {
        "It has no alpha channel, but this type of texture needs one."
    };
    text(
        "Alpha channel needed",
        what.to_owned(),
        "The material reads transparency or a mask from the alpha channel. Without it the whole \
         surface shows as solid.",
        "Add the alpha: in Photoshop, keep the transparent areas on a layer, or add an alpha \
         channel in the Channels panel, and save as PNG, or as 32-bit TGA."
            .to_owned(),
    )
}

pub fn bit_depth(bits: u8, float: bool, allowed: &[u8]) -> Text {
    let allowed: Vec<String> = allowed.iter().map(|b| format!("{b}")).collect();
    let kind = if float {
        format!("{bits}-bit float")
    } else {
        format!("{bits} bits per channel")
    };
    text(
        "Bit depth not allowed",
        format!(
            "This image is {kind}. Allowed: {} bits per channel.",
            list(&allowed)
        ),
        "A deeper image takes more memory and download size, and for most textures the extra \
         precision is lost when Unity compresses it anyway.",
        format!(
            "Convert it: in Photoshop, Image > Mode > {} Bits/Channel. Then save it again.",
            allowed.first().map_or("8", String::as_str)
        ),
    )
}

pub fn palette(bits: u8) -> Text {
    text(
        "Palette image",
        format!(
            "This is a {bits}-bit palette image, so it has {} colours at most.",
            palette_colours(bits)
        ),
        "A palette keeps only a few colours, so gradients band. Unity expands it to full colour \
         when it imports it, but the banding stays.",
        "Export it again from the source as full colour, not as a palette (\"indexed\" or \
         \"PNG-8\") image. In Photoshop, Image > Mode > RGB Color converts it, but it can't bring \
         back colours that were lost."
            .to_owned(),
    )
}

pub fn color_profile(description: Option<&str>) -> Text {
    let what = match description {
        Some(name) => format!("It has the colour profile \"{name}\" embedded, not sRGB."),
        None => "It has a colour profile embedded that isn't sRGB.".to_owned(),
    };
    text(
        "Not sRGB",
        what,
        "Unity ignores embedded colour profiles and treats every texture as sRGB, so its colours \
         in the game won't match what you saw in Photoshop.",
        "Convert it to sRGB: in Photoshop, Edit > Convert to Profile, with the destination \
         sRGB IEC61966-2.1. Then save it again."
            .to_owned(),
    )
}

pub fn generation_metadata(found: &[String]) -> Text {
    text(
        "AI generation data",
        format!(
            "It carries data from the tool that generated it: {}.",
            found.join(", ")
        ),
        "Prompts, settings and workflows stay inside the file, so they travel into the \
         repository and the game with it. The project's policy is to strip them.",
        "Remove it: in Photoshop, File > Export > Export As, with Metadata set to None. Or turn \
         off saving metadata in the generator and export it again."
            .to_owned(),
    )
}

pub fn file_size_too_big(bytes: u64, max_bytes: u64) -> Text {
    // Just over the limit, both round to the same figure; the exact bytes then show the difference.
    let (size, limit) = if file_size(bytes) == file_size(max_bytes) {
        (format!("{bytes} bytes"), format!("{max_bytes} bytes"))
    } else {
        (file_size(bytes), file_size(max_bytes))
    };
    text(
        "File too large",
        format!("The file is {size}. The most allowed is {limit}."),
        "Large files make the repository slower to clone and pull for everyone, and they stay \
         in its history even after they're replaced.",
        "Make it smaller: delete hidden or unused layers, crop away unused canvas, or save it \
         with compression (PNG, or RLE-compressed TGA)."
            .to_owned(),
    )
}

pub fn psd_composite() -> Text {
    text(
        "PSD has no merged image",
        "This PSD has no merged image: it was saved without Maximize Compatibility.".to_owned(),
        "Unity reads only a PSD's merged image. Without it the texture imports blank.",
        "In Photoshop, set Preferences > File Handling > Maximize PSD and PSB File Compatibility \
         to Always. Then open the file and save it again."
            .to_owned(),
    )
}

pub fn psb() -> Text {
    text(
        "PSB file",
        "This is a PSB (Large Document Format) file.".to_owned(),
        "Unity's texture importer doesn't read PSB files, and a PSB is usually far larger than \
         the texture needs.",
        "Save it as a PSD instead: in Photoshop, File > Save a Copy, with the format Photoshop. \
         If Photoshop refuses because the image is too large, scale it down first \
         (Image > Image Size)."
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_just_over_its_limit_reads_as_over_it() {
        let mb = 1024 * 1024;
        assert_eq!(
            file_size_too_big(3 * mb, 2 * mb).what,
            "The file is 3.0 MB. The most allowed is 2.0 MB."
        );
        assert_eq!(
            file_size_too_big(16 * mb + 18, 16 * mb).what,
            "The file is 16777234 bytes. The most allowed is 16777216 bytes."
        );
    }

    #[test]
    fn summary_counts() {
        assert_eq!(summary(1, 0, [0, 0, 0]), "1 image checked: no problems.");
        assert_eq!(
            summary(3, 1, [1, 2, 0]),
            "3 images checked, 1 couldn't be checked: 1 error, 2 warnings."
        );
    }
}
