//! Every line `fire-cli` says, and the measured values its findings name, worded.
//!
//! All of it is here, and only here, so a message catalogue can replace this module later
//! without a hunt through the code (Asset Check AC11). Fire has no catalogue yet, and the text is
//! English only for now.
//!
//! The findings' own texts are in `rules-catalog.toml`, with the rules: **what** gives the
//! measured value and the limit; **why** gives the consequence in the game or the pipeline;
//! **how** names the steps in the artist's own tool (AC11). A value the catalog's texts name that
//! needs words of its own ("an AVIF", "8 or 16", "3.0 MB") is worded here, under *Measures*.

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
    "rules needs a command: check, explain or catalog".to_owned()
}

pub fn usage_rules_unknown(word: &str) -> String {
    format!("\"{word}\" isn't a rules command; they are check, explain and catalog")
}

pub fn usage_rules_catalog_alone() -> String {
    "rules catalog takes no files and no --rules".to_owned()
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
  {PROGRAM} rules catalog
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
  catalog   Prints every rule {PROGRAM} checks, with its settings, defaults and texts, as JSON
            (asset-rules' catalog format), for Rule Editor.

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

/// Heads the lines saying what a texture rules file sets that this tool can't use.
pub fn passed_over(file: &str) -> String {
    format!(
        "{file}: {PROGRAM} passes over what it can't use here, and goes by the defaults instead:"
    )
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

/// How many colours a palette of `bits` bits per pixel holds.
pub fn palette_colours(bits: u8) -> u32 {
    1u32 << bits.min(8)
}

fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

// --- measures --------------------------------------------------------------------------------

/// A format's name with its article: "a PNG", "an AVIF", "an OpenEXR". Of the names Fire gives
/// formats, those starting with a vowel letter are said with a vowel (AVIF, ICO, OpenEXR) and the
/// rest with a consonant (HEIF, PNG, TGA).
pub fn a_format(name: &str) -> String {
    let an = name
        .chars()
        .next()
        .is_some_and(|c| "AEIOUaeiou".contains(c));
    format!("{} {name}", if an { "an" } else { "a" })
}

/// A bit depth with its article: "a 4-bit", "an 8-bit".
pub fn a_bits(bits: u8) -> String {
    let an = matches!(bits, 8 | 11 | 18 | 80..=89);
    format!("{} {bits}-bit", if an { "an" } else { "a" })
}

/// An image's depth: "8 bits per channel", or "16-bit float".
pub fn depth(bits: u8, float: bool) -> String {
    if float {
        format!("{bits}-bit float")
    } else {
        format!("{bits} bits per channel")
    }
}

/// Bit depths as a choice: "8", "8 or 16", "8, 16 or 32".
pub fn depths(allowed: &[u8]) -> String {
    let allowed: Vec<String> = allowed.iter().map(u8::to_string).collect();
    list(&allowed)
}

/// A file's size and its limit, as `texture.file_size` names them. Just over the limit, both
/// round to the same figure; the exact bytes then show the difference.
pub fn file_sizes(bytes: u64, max_bytes: u64) -> (String, String) {
    if file_size(bytes) == file_size(max_bytes) {
        (format!("{bytes} bytes"), format!("{max_bytes} bytes"))
    } else {
        (file_size(bytes), file_size(max_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_just_over_its_limit_reads_as_over_it() {
        let mb = 1024 * 1024;
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(file_sizes(3 * mb, 2 * mb), pair("3.0 MB", "2.0 MB"));
        assert_eq!(
            file_sizes(16 * mb + 18, 16 * mb),
            pair("16777234 bytes", "16777216 bytes")
        );
    }

    #[test]
    fn articles() {
        let formats: Vec<String> = ["PNG", "AVIF", "OpenEXR", "HEIF", "ICO", "image", "JPEG"]
            .into_iter()
            .map(a_format)
            .collect();
        assert_eq!(
            formats,
            [
                "a PNG",
                "an AVIF",
                "an OpenEXR",
                "a HEIF",
                "an ICO",
                "an image",
                "a JPEG"
            ]
        );
        let bits: Vec<String> = [1, 2, 4, 8, 16].into_iter().map(a_bits).collect();
        assert_eq!(
            bits,
            ["a 1-bit", "a 2-bit", "a 4-bit", "an 8-bit", "a 16-bit"]
        );
        assert_eq!(depths(&[8, 16, 32]), "8, 16 or 32");
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
