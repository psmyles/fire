//! `fire-cli`: Asset Check's image checker (AC12). A console program installed beside Fire's
//! viewer. It checks images against a technical artist's rules with fire-decode, Fire's own
//! decoder, and writes what it found in Asset Check's results format.
//!
//! Exit status, as `review3d-cli`'s (AC10): 0 when nothing at warning level or worse was found,
//! 1 when something was found or a file couldn't be checked, 2 when the command line couldn't be
//! followed. A refused command line runs nothing and writes no results; results an earlier run
//! left at its `--results` path are removed all the same, so after any run the file is this run's
//! or is not there.

mod args;
mod check;
mod checks;
mod facts;
mod info;
mod messages;
mod metadata;
mod profile;
mod rules_cmd;

/// The rules, generated from `rules-catalog.toml` by `build.rs`: `RuleId`, each rule's parameters
/// (`params`), and what each rule's check hands its texts (`found`, `Found`).
mod rules {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/rules.rs"));
}

use std::process::ExitCode;

use asset_rules::results::ToolInfo;

use crate::args::{Parsed, PROGRAM};

/// This tool, as results and `rules.json`'s `requires` name it.
pub fn tool() -> ToolInfo {
    ToolInfo {
        name: PROGRAM.to_owned(),
        version: env!("FIRE_VERSION").to_owned(),
    }
}

fn main() -> ExitCode {
    let raw: Vec<_> = std::env::args_os().skip(1).collect();
    let parsed = match args::parse(raw.clone()) {
        Ok(parsed) => parsed,
        Err(problem) => {
            if let Some(path) = args::results_path(&raw) {
                check::discard_stale(&path);
            }
            console::warn(&messages::usage(&problem));
            return ExitCode::from(2);
        }
    };
    let ok = match parsed {
        Parsed::Help => {
            console::say(messages::help().trim_end());
            true
        }
        Parsed::Version => {
            console::say(&messages::version(env!("FIRE_VERSION")));
            true
        }
        Parsed::Check(args) => check::run(&args),
        Parsed::Info(files) => info::run(&files),
        Parsed::RulesCheck(rules) => rules_cmd::check(&rules),
        Parsed::RulesExplain { rules, files } => rules_cmd::explain(&rules, &files),
        Parsed::RulesCatalog => rules_cmd::catalog(),
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Plain lines: results to standard output, problems to standard error, each a whole line.
/// Writes to a closed pipe are ignored, never fatal.
mod console {
    use std::io::Write;

    pub fn say(line: &str) {
        let _ = writeln!(std::io::stdout().lock(), "{line}");
    }

    pub fn warn(line: &str) {
        let _ = writeln!(std::io::stderr().lock(), "{line}");
    }
}
