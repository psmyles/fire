//! `fire-cli rules check`, `rules explain` and `rules catalog`: Asset Check's commands for the
//! technical artist who writes `rules.json` and the rules files (AC9).
//!
//! `check` and `explain` are asset-rules' own, so `3d-review-cli`'s `rules` commands say the same
//! thing about the same file. `check` lists the problems in a configuration, and in the texture
//! rules files it names, read against this tool's catalog (3d-review-cli checks the model rules
//! files); `explain` says which type, profile, stage and rules file apply to a file, and why. `check` exits 1 on any problem,
//! warnings included, as every Asset Check exit status does (AC10), so a CI step that runs it stops
//! on one. `catalog` prints this tool's rules catalog, which Rule Editor reads.

use std::path::{Path, PathBuf};

use asset_rules::git::Repo;
use asset_rules::{Context, Level, Rules, RulesError};

use crate::console::{say, warn};
use crate::messages as m;
use crate::tool;

/// Loads `path`, saying why when it can't be loaded and, for a `rules.json` too new for this
/// tool, what to update.
pub fn load(path: &Path) -> Result<Rules, String> {
    Rules::load(path).map_err(|error| load_problem(path, &error))
}

/// Why the `rules.json` at `path` couldn't be loaded, and what to update if it is too new.
pub fn load_problem(path: &Path, error: &RulesError) -> String {
    let mut problem = m::rules_unreadable(&path.display().to_string(), &error.to_string());
    if let Some(finding) = error.too_old(&tool()) {
        problem = format!("{problem}\n{}\n{}", finding.what, finding.how);
    }
    problem
}

/// `rules check`: `true` when the configuration, and the texture rules files it names, have no
/// problems.
pub fn check(path: &Path) -> bool {
    let rules = match load(path) {
        Ok(rules) => rules,
        Err(problem) => {
            warn(&problem);
            return false;
        }
    };
    let mut problems = rules.check();
    problems.extend(rules.check_rules_files(crate::rules::catalog()));
    for problem in &problems {
        say(&problem.to_string());
    }
    let too_old = rules.too_old(&tool());
    if let Some(finding) = &too_old {
        say(&finding.what);
        say(&finding.how);
    }
    let errors = problems
        .iter()
        .filter(|problem| problem.level() == Level::Error)
        .count();
    let warnings = problems.len() - errors + usize::from(too_old.is_some());
    let file = path.display().to_string();
    if errors + warnings == 0 {
        say(&m::rules_check_clean(&file));
        true
    } else {
        say(&m::rules_check_problems(&file, errors, warnings));
        false
    }
}

/// `rules catalog`: every rule this tool checks, as JSON, with this tool's version.
pub fn catalog() -> bool {
    say(crate::rules::catalog()
        .to_json(env!("FIRE_VERSION"))
        .trim_end());
    true
}

/// `rules explain`: always `true` once the rules load.
pub fn explain(path: &Path, files: &[PathBuf]) -> bool {
    let rules = match load(path) {
        Ok(rules) => rules,
        Err(problem) => {
            warn(&problem);
            return false;
        }
    };
    let ctx = context();
    for (index, file) in files.iter().enumerate() {
        if index > 0 {
            say("");
        }
        let absolute = std::path::absolute(file).unwrap_or_else(|_| file.clone());
        say(asset_rules::explain(&rules, &ctx, &absolute).trim_end());
    }
    true
}

/// The repository the current folder is in, for the paths profiles match and the branch the
/// stage can come from. Outside one there is no branch, and only profiles without `paths` match.
pub fn context() -> Context {
    Repo::discover(Path::new("."))
        .and_then(|repo| repo.context())
        .unwrap_or_else(|_| Context {
            repo_root: std::path::absolute(".").unwrap_or_default(),
            branch: None,
        })
}
