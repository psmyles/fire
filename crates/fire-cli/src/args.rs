//! The command line.
//!
//! 3d-review-cli's contract, so a tool that drives one drives the other the same way: long options
//! only, no prompts, no standard input. A value is written `--name value` or `--name=value`, and an
//! empty value is the same as leaving the option out, so a form can always pass every option. `--`
//! ends the options. Relative paths resolve against the working folder.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::messages as m;

/// This tool, as results, `rules.json`'s `requires` and the messages name it.
pub const PROGRAM: &str = "fire-cli";

pub const RULES: &str = "--rules";
pub const PROFILE: &str = "--profile";
pub const RESULTS: &str = "--results";
pub const FILES_FROM: &str = "--files-from";
pub const STAGED: &str = "--staged";
const HELP: &str = "--help";
const VERSION: &str = "--version";

/// What a command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Help,
    Version,
    Check(CheckArgs),
    Info(Vec<PathBuf>),
    RulesCheck(PathBuf),
    RulesExplain { rules: PathBuf, files: Vec<PathBuf> },
}

#[derive(Debug, PartialEq, Eq)]
pub struct CheckArgs {
    /// Files named on the line.
    pub files: Vec<PathBuf>,
    /// `--files-from`: a file listing more, one per line.
    pub files_from: Option<PathBuf>,
    /// `--staged`: the images staged in the current folder's repository instead.
    pub staged: bool,
    pub judge: Judge,
    /// `--results`: where to write Asset Check's results.
    pub results: Option<PathBuf>,
}

/// What each image is checked against.
#[derive(Debug, PartialEq, Eq)]
pub enum Judge {
    /// `--rules`: Asset Check's `rules.json`, which picks each image's profile and stage.
    Rules(PathBuf),
    /// `--profile`: one texture rules file for every image.
    Profile(PathBuf),
    /// Neither: the built-in defaults.
    Defaults,
}

/// Which options a command takes, and whether each takes a value.
const CHECK_OPTIONS: &[(&str, bool)] = &[
    (RULES, true),
    (PROFILE, true),
    (RESULTS, true),
    (FILES_FROM, true),
    (STAGED, false),
    (HELP, false),
];
const INFO_OPTIONS: &[(&str, bool)] = &[(HELP, false)];
const RULES_OPTIONS: &[(&str, bool)] = &[(RULES, true), (HELP, false)];

/// The options and words given to one command.
#[derive(Default)]
struct Given {
    words: Vec<OsString>,
    /// Options with a value, the last of each kept. An empty value is dropped.
    values: Vec<(&'static str, OsString)>,
    flags: Vec<&'static str>,
}

impl Given {
    fn path(&self, option: &str) -> Option<PathBuf> {
        self.values
            .iter()
            .rev()
            .find(|(name, _)| *name == option)
            .map(|(_, value)| PathBuf::from(value))
    }

    fn flag(&self, option: &str) -> bool {
        self.flags.contains(&option)
    }

    fn paths(&self) -> Vec<PathBuf> {
        self.words.iter().map(PathBuf::from).collect()
    }
}

/// Reads a command line (without the program name). `Err` is the message for a line that can't
/// be followed.
pub fn parse(args: Vec<OsString>) -> Result<Parsed, String> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Err(m::usage_no_command());
    };
    let rest: Vec<OsString> = args.collect();
    match first.to_string_lossy().as_ref() {
        HELP => Ok(Parsed::Help),
        VERSION => Ok(Parsed::Version),
        "check" => check(scan("check", rest, CHECK_OPTIONS)?),
        "info" => {
            let given = scan("info", rest, INFO_OPTIONS)?;
            if given.flag(HELP) {
                return Ok(Parsed::Help);
            }
            if given.words.is_empty() {
                return Err(m::usage_info_no_files());
            }
            Ok(Parsed::Info(given.paths()))
        }
        "rules" => rules(scan("rules", rest, RULES_OPTIONS)?),
        other => Err(m::usage_unknown_command(other)),
    }
}

fn check(given: Given) -> Result<Parsed, String> {
    if given.flag(HELP) {
        return Ok(Parsed::Help);
    }
    let staged = given.flag(STAGED);
    let files_from = given.path(FILES_FROM);
    if staged && (!given.words.is_empty() || files_from.is_some()) {
        return Err(m::usage_staged_and_files());
    }
    // An empty --files-from list is a run with nothing to do rather than a mistake.
    if given.words.is_empty() && !staged && files_from.is_none() {
        return Err(m::usage_no_files());
    }
    let judge = match (given.path(RULES), given.path(PROFILE)) {
        (Some(_), Some(_)) => return Err(m::usage_rules_and_profile()),
        (Some(rules), None) => Judge::Rules(rules),
        (None, Some(profile)) => Judge::Profile(profile),
        (None, None) => Judge::Defaults,
    };
    Ok(Parsed::Check(CheckArgs {
        files: given.paths(),
        files_from,
        staged,
        judge,
        results: given.path(RESULTS),
    }))
}

/// `rules check <rules.json>` (or `--rules`), or `rules explain <files>... --rules <rules.json>`.
fn rules(given: Given) -> Result<Parsed, String> {
    if given.flag(HELP) {
        return Ok(Parsed::Help);
    }
    let Some((first, rest)) = given.words.split_first() else {
        return Err(m::usage_rules_no_command());
    };
    let rules = given.path(RULES);
    match first.to_string_lossy().as_ref() {
        "check" => match (rules, rest) {
            (Some(rules), []) => Ok(Parsed::RulesCheck(rules)),
            (None, [rules]) => Ok(Parsed::RulesCheck(PathBuf::from(rules))),
            _ => Err(m::usage_rules_check_one()),
        },
        "explain" => {
            let Some(rules) = rules else {
                return Err(m::usage_rules_explain_needs_rules());
            };
            if rest.is_empty() {
                return Err(m::usage_rules_explain_no_files());
            }
            Ok(Parsed::RulesExplain {
                rules,
                files: rest.iter().map(PathBuf::from).collect(),
            })
        }
        other => Err(m::usage_rules_unknown(other)),
    }
}

/// Splits `args` into words and the options `command` takes.
fn scan(
    command: &str,
    args: Vec<OsString>,
    options: &[(&'static str, bool)],
) -> Result<Given, String> {
    let mut given = Given::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        if text == "--" {
            given.words.extend(args.by_ref());
            break;
        }
        if !text.starts_with("--") {
            given.words.push(arg);
            continue;
        }
        let (name, inline) = match split_inline(&arg) {
            Some((name, value)) => (name, Some(value)),
            None => (text, None),
        };
        let Some(&(option, takes_value)) = options.iter().find(|(o, _)| *o == name) else {
            return Err(m::usage_unknown_option(&name, command));
        };
        if !takes_value {
            if inline.is_some() {
                return Err(m::usage_flag_value(option));
            }
            given.flags.push(option);
            continue;
        }
        let value = match inline {
            Some(value) => value,
            None => match args.next() {
                Some(value) if !value.to_string_lossy().starts_with("--") => value,
                _ => return Err(m::usage_missing_value(option)),
            },
        };
        given.values.retain(|(name, _)| *name != option);
        if !value.is_empty() {
            given.values.push((option, value));
        }
    }
    Ok(given)
}

/// `--name=value` split at its first `=`, or `None` when there is none. Split on the argument as
/// the OS handed it over, so a value that isn't valid Unicode survives.
fn split_inline(arg: &OsString) -> Option<(String, OsString)> {
    let text = arg.to_str()?;
    let (name, value) = text.split_once('=')?;
    Some((name.to_owned(), OsString::from(value)))
}

/// The `--results` path a command line names, read leniently: for a line [`parse`] refused, so
/// results an earlier run left there can still be removed. The last one before any `--` wins, as
/// it does when the line is accepted.
pub fn results_path(args: &[OsString]) -> Option<PathBuf> {
    let mut found = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        match split_inline(arg) {
            Some((name, value)) if name == RESULTS => found = Some(value),
            Some(_) => {}
            None if arg == RESULTS => {
                found = args
                    .next()
                    .filter(|value| !value.to_string_lossy().starts_with("--"))
                    .cloned();
            }
            None => {}
        }
    }
    found.filter(|value| !value.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &str) -> Result<Parsed, String> {
        parse(line.split_whitespace().map(OsString::from).collect())
    }

    fn check_line(line: &str) -> CheckArgs {
        match parse_line(line) {
            Ok(Parsed::Check(args)) => args,
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn check_takes_files_rules_and_results() {
        let args = check_line("check a.png b.tga --rules rules/rules.json --results=r.json");
        assert_eq!(args.files, [PathBuf::from("a.png"), PathBuf::from("b.tga")]);
        assert_eq!(args.judge, Judge::Rules(PathBuf::from("rules/rules.json")));
        assert_eq!(args.results, Some(PathBuf::from("r.json")));
        assert!(!args.staged);
    }

    #[test]
    fn staged_and_files_from_name_the_images_themselves() {
        assert!(check_line("check --staged --profile p.json").staged);
        assert_eq!(
            check_line("check --files-from list.txt").files_from,
            Some(PathBuf::from("list.txt"))
        );
        assert_eq!(check_line("check a.png").judge, Judge::Defaults);
    }

    #[test]
    fn contradictions_and_gaps_are_refused() {
        for line in [
            "",
            "frobnicate",
            "check",
            "check --staged a.png",
            "check --staged --files-from l.txt",
            "check a.png --rules r.json --profile p.json",
            "check a.png --bogus",
            "check a.png --rules",
            "check a.png --staged=yes",
            "info",
            "rules",
            "rules lint r.json",
            "rules check",
            "rules check a.json b.json",
            "rules explain a.png",
            "rules explain --rules r.json",
        ] {
            assert!(parse_line(line).is_err(), "{line:?} should be refused");
        }
    }

    /// An empty value is the same as leaving the option out, so a form can pass every option.
    #[test]
    fn an_empty_value_is_no_value() {
        let args = parse(
            ["check", "a.png", "--rules=", "--results", ""]
                .map(OsString::from)
                .to_vec(),
        );
        let Ok(Parsed::Check(args)) = args else {
            panic!("{args:?}");
        };
        assert_eq!(args.judge, Judge::Defaults);
        assert_eq!(args.results, None);
    }

    #[test]
    fn double_dash_ends_the_options() {
        let args = check_line("check -- --staged");
        assert_eq!(args.files, [PathBuf::from("--staged")]);
        assert!(!args.staged);
    }

    #[test]
    fn rules_commands() {
        assert_eq!(
            parse_line("rules check r.json"),
            Ok(Parsed::RulesCheck(PathBuf::from("r.json")))
        );
        assert_eq!(
            parse_line("rules check --rules r.json"),
            Ok(Parsed::RulesCheck(PathBuf::from("r.json")))
        );
        assert_eq!(
            parse_line("rules explain a.png b.png --rules r.json"),
            Ok(Parsed::RulesExplain {
                rules: PathBuf::from("r.json"),
                files: vec![PathBuf::from("a.png"), PathBuf::from("b.png")],
            })
        );
    }

    #[test]
    fn the_results_path_is_found_on_a_refused_line() {
        let line =
            |s: &str| -> Vec<OsString> { s.split_whitespace().map(OsString::from).collect() };
        assert_eq!(
            results_path(&line("check --bogus --results r.json")),
            Some(PathBuf::from("r.json"))
        );
        assert_eq!(
            results_path(&line("check --results=a.json --results b.json")),
            Some(PathBuf::from("b.json"))
        );
        assert_eq!(results_path(&line("check -- --results r.json")), None);
        assert_eq!(results_path(&line("check --results --staged")), None);
    }
}
