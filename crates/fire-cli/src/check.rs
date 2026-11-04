//! `fire-cli check`: every image checked against its rules (AC12).
//!
//! With `--rules`, Asset Check's `rules.json` picks each image's profile and stage from its name,
//! its folder and the branch, through asset-rules, so `3d-review-cli` resolves a file the same
//! way. With `--profile`, one texture rules file applies to every image; with neither, the
//! built-in defaults do. `--staged` checks the images staged in the current folder's repository,
//! as staged; there, with `--rules`, each result is cached against the staged content, the rules
//! file that applied, this tool's version and the stage, so checking an unchanged commit again is
//! all but instant (AC15). `--results` writes what was found in Asset Check's format.
//!
//! Images are decoded in parallel, at most four at once and never more than there are cores, each
//! with its share of the cores for the decode's own threaded passes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use asset_rules::cache::{Cache, CacheKey};
use asset_rules::git::{Repo, StagedFile};
use asset_rules::results::{
    ContextInfo, FileResult, Finding, Results, RulesInfo, Severity, Status,
};
use asset_rules::{resolve, Context, Mode, Resolution, Resolved, Rules};

use crate::args::{CheckArgs, Judge, PROGRAM};
use crate::console::{say, warn};
use crate::facts::{self, Facts};
use crate::messages as m;
use crate::profile::Profile;
use crate::{checks, rules_cmd, tool};

/// The type an image has in results written without `--rules`, where no `rules.json` names one:
/// what Asset Check's rules call it.
const TEXTURE_TYPE: &str = "texture";

/// The profile id results give the built-in defaults.
const DEFAULTS_ID: &str = "default";

/// The most images decoded at once.
const MAX_WORKERS: usize = 4;

/// One image to check: how lines and results name it, and where its content is.
struct Subject {
    /// The path as given, or the repository path of a staged image.
    label: String,
    source: Source,
}

enum Source {
    File(PathBuf),
    /// Staged: read through the repository, and cached by its blob.
    Staged(StagedFile),
}

/// What every image is judged against.
enum Judging {
    Rules {
        rules: Box<Rules>,
        /// Each texture rules file a profile named, loaded once: the profile and its digest.
        files: HashMap<PathBuf, Result<Arc<(Profile, String)>, String>>,
    },
    One {
        profile: Arc<(Profile, String)>,
        /// How results name it: the `--profile` path as given, or `default`.
        id: String,
    },
}

/// What happens to one image, once its rules are worked out.
enum Plan {
    /// Check it against this profile; `entry` is its results entry so far.
    Check {
        profile: Arc<(Profile, String)>,
        entry: FileResult,
        key: Option<CacheKey>,
    },
    /// Already decided: no profile matched, or the cache has it.
    Decided { entry: FileResult, cached: bool },
    /// It can't be checked: its results entry, and why.
    Unreadable { entry: FileResult, problem: String },
}

/// `true` when the run exits 0: nothing at warning level or worse, every image checked, and the
/// results (if asked for) written.
pub fn run(args: &CheckArgs) -> bool {
    if let Some(path) = &args.results {
        discard_stale(path);
    }
    // Asking git costs two processes, so only a run that resolves images by their repository
    // path, or records the branch, asks.
    let needs_repo = args.staged || args.results.is_some() || matches!(args.judge, Judge::Rules(_));
    let mut ctx = if needs_repo {
        rules_cmd::context()
    } else {
        Context::default()
    };

    // What the images are checked against, and the results header.
    let (mut judging, mut results, opening) = match &args.judge {
        Judge::Rules(path) => {
            let rules = match Rules::load(path) {
                Ok(rules) => rules,
                Err(error) => {
                    warn(&rules_cmd::load_problem(path, &error));
                    // A rules.json too new to read can still say what to update.
                    if let (Some(finding), Some(results_path)) =
                        (error.too_old(&tool()), &args.results)
                    {
                        let digest = asset_rules::cache::digest_file(path).unwrap_or_default();
                        let mut results = bare_results(digest, ctx.branch.clone());
                        results.findings.push(finding);
                        write_results(&results, results_path);
                    }
                    return false;
                }
            };
            let commit = Repo::discover(rules.dir())
                .and_then(|repo| repo.head())
                .ok()
                .flatten();
            let mut results = Results::new(tool(), &rules, commit, &ctx);
            results.findings.extend(rules.too_old(&tool()));
            let opening = path.display().to_string();
            let judging = Judging::Rules {
                rules: Box::new(rules),
                files: HashMap::new(),
            };
            (judging, results, Opening::Rules(opening))
        }
        Judge::Profile(path) => {
            let mut warnings = Vec::new();
            let loaded = Profile::load(path, &mut warnings);
            warnings.iter().for_each(|line| warn(line));
            let (profile, digest) = match loaded {
                Ok(loaded) => loaded,
                Err(problem) => {
                    warn(&problem);
                    return false;
                }
            };
            let results = bare_results(digest.clone(), ctx.branch.clone());
            let opening = Opening::Profile(profile.name.clone());
            let judging = Judging::One {
                profile: Arc::new((profile, digest)),
                id: path.display().to_string(),
            };
            (judging, results, opening)
        }
        Judge::Defaults => {
            let digest = Profile::defaults_digest();
            let results = bare_results(digest.clone(), ctx.branch.clone());
            let judging = Judging::One {
                profile: Arc::new((Profile::defaults(), digest)),
                id: DEFAULTS_ID.to_owned(),
            };
            (judging, results, Opening::Defaults)
        }
    };

    // The images.
    let mut staged: Option<(Repo, Cache)> = None;
    let subjects: Vec<Subject> = if args.staged {
        let repo = match Repo::discover(Path::new(".")) {
            Ok(repo) => repo,
            Err(error) => return fail_run(m::staged_failed(&error.to_string()), &results, args),
        };
        // Copies a killed run never removed, and entries unused for a month. Nothing depends on
        // either succeeding.
        let _ = repo.prune_copies();
        let files = match repo.staged() {
            Ok(files) => files,
            Err(error) => return fail_run(m::staged_failed(&error.to_string()), &results, args),
        };
        ctx = repo.context().unwrap_or(ctx);
        results.context.branch = ctx.branch.clone();
        let cache = Cache::open(&repo);
        let _ = cache.prune();
        staged = Some((repo, cache));
        files
            .into_iter()
            .filter(|file| {
                // Without rules, the staged images are the files fire can open; with them, the
                // rules' types decide, below.
                matches!(judging, Judging::Rules { .. })
                    || Path::new(&file.path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(fire_decode::is_supported_extension)
            })
            .map(|file| Subject {
                label: file.path.clone(),
                source: Source::Staged(file),
            })
            .collect()
    } else {
        let mut paths = args.files.clone();
        if let Some(list) = &args.files_from {
            match std::fs::read_to_string(list) {
                Ok(text) => paths.extend(
                    text.lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(PathBuf::from),
                ),
                Err(error) => {
                    let problem =
                        m::files_from_unreadable(&list.display().to_string(), &error.to_string());
                    return fail_run(problem, &results, args);
                }
            }
        }
        paths
            .into_iter()
            .map(|path| Subject {
                label: path.display().to_string(),
                source: Source::File(path),
            })
            .collect()
    };

    // What each is checked against. Files the rules leave to another tool, or to none, drop out
    // here: a staged one silently, a named one with a line saying why.
    let mut plans: Vec<(Subject, Plan)> = Vec::new();
    for subject in subjects {
        match plan(&mut judging, &ctx, staged.as_ref(), &subject) {
            Ok(plan) => plans.push((subject, plan)),
            Err(reason) => {
                if matches!(subject.source, Source::File(_)) {
                    say(&format!("{}: {reason}", subject.label));
                }
            }
        }
    }

    let count = plans.len();
    if count == 0 && args.staged {
        say(&m::staged_none());
    } else {
        say(&match &opening {
            Opening::Rules(file) => m::check_start_rules(count, file),
            Opening::Profile(name) => m::check_start_profile(count, name),
            Opening::Defaults => m::check_start_defaults(count),
        });
    }
    for finding in &results.findings {
        say_finding("", finding);
    }

    // Decode and measure, in parallel; then say and record each, in order.
    let repo = staged.as_ref().map(|(repo, _)| repo);
    let jobs: Vec<&Subject> = plans
        .iter()
        .filter(|(_, plan)| matches!(plan, Plan::Check { .. }))
        .map(|(subject, _)| subject)
        .collect();
    let mut gathered = gather_all(&jobs, repo).into_iter();

    let mut counts = [0usize; 3];
    let mut checked = 0;
    let mut unreadable = 0;
    for (index, (subject, plan)) in plans.into_iter().enumerate() {
        say(&m::heading(index + 1, count, &subject.label));
        let entry = match plan {
            Plan::Decided { entry, cached } => {
                say_resolution(&entry);
                if cached {
                    say(&format!("  {}", m::from_cache()));
                }
                entry
            }
            Plan::Unreadable { entry, problem } => unreadable_entry(entry, problem),
            Plan::Check {
                profile,
                mut entry,
                key,
            } => {
                say_resolution(&entry);
                match gathered.next().flatten() {
                    Some(Ok(facts)) => {
                        entry.findings = checks::evaluate(&facts, &profile.0);
                        if let (Some(key), Some((_, cache))) = (&key, &staged) {
                            // A cache that can't be written costs the next run time, not this
                            // one its answer.
                            let _ = cache.put(key, &entry);
                        }
                        entry
                    }
                    Some(Err(problem)) => unreadable_entry(entry, problem),
                    None => unreadable_entry(entry, m::check_panicked()),
                }
            }
        };
        if entry.status == Status::Unreadable {
            unreadable += 1;
            let problem = entry.problem.as_deref().unwrap_or_default();
            warn(&format!("  {}", m::not_checked(problem)));
        } else {
            checked += 1;
            if entry.findings.is_empty() {
                say(&format!("  {}", m::no_problems()));
            }
            for finding in &entry.findings {
                say_finding("  ", finding);
            }
        }
        for finding in &entry.findings {
            counts[severity_index(finding.severity)] += 1;
        }
        results.files.push(entry);
    }
    for finding in &results.findings {
        counts[severity_index(finding.severity)] += 1;
    }
    if count > 0 {
        say(&m::summary(checked, unreadable, counts));
    }

    let written = args
        .results
        .as_ref()
        .is_none_or(|path| write_results(&results, path));
    written && results.exit_code() == 0
}

/// What the run's first line names.
enum Opening {
    Rules(String),
    Profile(String),
    Defaults,
}

/// Works out what `subject` is checked against, or why it isn't this tool's to check.
fn plan(
    judging: &mut Judging,
    ctx: &Context,
    staged: Option<&(Repo, Cache)>,
    subject: &Subject,
) -> Result<Plan, String> {
    let path = results_path(ctx, subject);
    let (rules, files) = match judging {
        Judging::One { profile, id } => {
            let entry = FileResult {
                path,
                kind: Some(TEXTURE_TYPE.to_owned()),
                profile: Some(id.clone()),
                profile_label: Some(profile.0.name.clone()),
                stage: None,
                stage_from: None,
                status: Status::Checked,
                problem: None,
                findings: Vec::new(),
            };
            return Ok(Plan::Check {
                profile: profile.clone(),
                entry,
                key: None,
            });
        }
        Judging::Rules { rules, files } => (&**rules, files),
    };

    // A staged image resolves by its repository path; a named one by where it is, which
    // `resolve` makes repository-relative itself.
    let resolution = match &subject.source {
        Source::Staged(file) => resolve(rules, ctx, Path::new(&file.path)),
        Source::File(file) => {
            let absolute = std::path::absolute(file).unwrap_or_else(|_| file.clone());
            resolve(rules, ctx, &absolute)
        }
    };
    let ours = |kind: &str| rules.types()[kind].tool == PROGRAM;
    let resolved: Resolved = match resolution {
        Resolution::Ignored => return Err(m::left_out_ignored()),
        Resolution::Unmatched { kind } | Resolution::Resolved(Resolved { kind, .. })
            if !ours(&kind) =>
        {
            return Err(m::left_out_other_tool(&kind, &rules.types()[&kind].tool));
        }
        Resolution::Unmatched { kind } => {
            return Ok(Plan::Decided {
                entry: FileResult::unmatched(path, &kind, rules),
                cached: false,
            });
        }
        Resolution::Resolved(resolved) => resolved,
    };
    let entry = FileResult::resolved(path, &resolved);
    let Some(rules_file) = resolved.rules_file.clone() else {
        let problem = m::no_rules_file(&resolved.profile_id, &resolved.stage);
        return Ok(Plan::Unreadable { entry, problem });
    };
    let loaded = files.entry(rules_file.clone()).or_insert_with(|| {
        let mut warnings = Vec::new();
        let loaded = Profile::load(&rules_file, &mut warnings).map(Arc::new);
        warnings.iter().for_each(|line| warn(line));
        loaded
    });
    let profile = match loaded {
        Ok(profile) => profile.clone(),
        Err(problem) => {
            return Ok(Plan::Unreadable {
                entry,
                problem: problem.clone(),
            });
        }
    };

    let key = match (&subject.source, staged) {
        (Source::Staged(file), Some((_, cache))) => {
            let key = CacheKey::new(&file.path, &file.blob, &resolved, &profile.1, &tool());
            if let Some(cached) = cache.get(&key) {
                return Ok(Plan::Decided {
                    entry: cached,
                    cached: true,
                });
            }
            Some(key)
        }
        _ => None,
    };
    Ok(Plan::Check {
        profile,
        entry,
        key,
    })
}

/// The facts of every job, in order, decoded on a small pool. An entry is `None` only if its
/// worker died, which `gather_one` doesn't let a panic do.
fn gather_all(jobs: &[&Subject], repo: Option<&Repo>) -> Vec<Option<Result<Facts, String>>> {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let workers = jobs.len().min(cores).clamp(1, MAX_WORKERS);
    let threads_each = (cores / workers).max(1);
    let next = AtomicUsize::new(0);
    let mut out: Vec<Option<Result<Facts, String>>> = (0..jobs.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(subject) = jobs.get(index) else {
                            break;
                        };
                        let facts = fire_decode::with_max_threads(threads_each, || {
                            gather_one(subject, repo)
                        });
                        mine.push((index, facts));
                    }
                    mine
                })
            })
            .collect();
        for handle in handles {
            for (index, facts) in handle.join().unwrap_or_default() {
                out[index] = Some(facts);
            }
        }
    });
    out
}

/// The facts of one image: its content read from the file, or from what is staged of it.
fn gather_one(subject: &Subject, repo: Option<&Repo>) -> Result<Facts, String> {
    let (path, _copy) = match (&subject.source, repo) {
        (Source::File(path), _) => (path.clone(), None),
        (Source::Staged(file), Some(repo)) => {
            let checked = repo
                .content(file)
                .map_err(|e| m::staged_unreadable(&e.to_string()))?;
            (checked.path().to_path_buf(), Some(checked))
        }
        (Source::Staged(_), None) => unreachable!("a staged image comes with its repository"),
    };
    if path.is_dir() {
        return Err(m::not_a_file());
    }
    std::panic::catch_unwind(|| facts::gather(&path)).unwrap_or_else(|_| Err(m::check_panicked()))
}

/// How results name an image: by its repository path, with `/` separators, or, outside the
/// repository, by the path it was given as.
fn results_path(ctx: &Context, subject: &Subject) -> String {
    match &subject.source {
        Source::Staged(file) => file.path.clone(),
        Source::File(path) => {
            let absolute = std::path::absolute(path).unwrap_or_else(|_| path.clone());
            ctx.repo_path(&absolute)
                .unwrap_or_else(|| subject.label.replace('\\', "/"))
        }
    }
}

/// Results for a run with no `rules.json` to record: one against a `--profile` or the defaults,
/// whose digest it is. Advise mode, since there are no rules to say otherwise.
fn bare_results(digest: String, branch: Option<String>) -> Results {
    Results {
        tool: tool(),
        rules: RulesInfo {
            commit: None,
            digest,
        },
        context: ContextInfo {
            branch,
            mode: Mode::Advise,
        },
        findings: Vec::new(),
        files: Vec::new(),
    }
}

/// A run that stopped before checking anything: said, and recorded in the results if asked for.
fn fail_run(problem: String, results: &Results, args: &CheckArgs) -> bool {
    warn(&problem);
    if let Some(path) = &args.results {
        write_results(results, path);
    }
    false
}

fn unreadable_entry(mut entry: FileResult, problem: String) -> FileResult {
    entry.status = Status::Unreadable;
    entry.problem = Some(problem);
    entry.findings.clear();
    entry
}

/// Says which profile and stage the rules gave an image.
fn say_resolution(entry: &FileResult) {
    if let (Some(label), Some(id), Some(stage), Some(from)) = (
        &entry.profile_label,
        &entry.profile,
        &entry.stage,
        entry.stage_from,
    ) {
        say(&format!("  {}", m::checked_as(label, id, stage, from)));
    }
}

/// A finding said in two lines: its severity and what, then how.
fn say_finding(indent: &str, finding: &Finding) {
    say(&format!(
        "{indent}{}",
        m::finding_line(finding.severity, &finding.what)
    ));
    say(&format!("{indent}  {}", m::how_line(&finding.how)));
}

fn severity_index(severity: Severity) -> usize {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    }
}

/// Writes `results`, saying so when it can't: `false` then, which fails the run, since a hook
/// waiting on the file would otherwise read nothing and be told everything worked.
fn write_results(results: &Results, path: &Path) -> bool {
    match results.write(path) {
        Ok(()) => true,
        Err(error) => {
            warn(&m::results_failed(
                &path.display().to_string(),
                &error.to_string(),
            ));
            false
        }
    }
}

/// Removes results an earlier run left at `path`, so they can never be read as this run's.
pub fn discard_stale(path: &Path) {
    let _ = std::fs::remove_file(path);
}
