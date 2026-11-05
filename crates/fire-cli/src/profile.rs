//! A texture rules file: which rules apply, how serious each is, its limits, and the technical
//! artist's note (Asset Check AC8, AC12).
//!
//! The envelope is 3d-review-cli's profile format, so both checkers' rules files look alike:
//!
//! ```json
//! { "version": 1, "generator": "hand-written",
//!   "profile": { "name": "Prop texture (production)",
//!                "rules": { "texture.max_size": { "severity": "error", "params": { "max": 2048 },
//!                                                 "note": "Quest budget for props." } } } }
//! ```
//!
//! Every rule, its parameters and its defaults are in `rules-catalog.toml` ([`crate::rules`]). A
//! file is read leniently, as asset-rules' `Rule::settings` and 3d-review-cli read one, since these
//! files are written by hand:
//! - a listed rule is on unless it says `"enabled": false`;
//! - whatever a rule leaves out (severity, any parameter) is its default;
//! - a rule whose settings can't be read takes its defaults whole, and a parameter value the rule
//!   can't take (the wrong kind, out of range) keeps its default;
//! - a rule the file doesn't list takes its default, which is off for every rule that is a matter
//!   of team policy. Only rules about something Unity gets wrong whatever the policy are on by
//!   default: the format, a PSD's merged image, and the colour profile.
//!
//! Whatever is passed over (a rule fire-cli doesn't have, a value it can't take) is a warning,
//! and `rules check` an error.

use std::path::Path;

use asset_rules::catalog::RuleSettings;
use serde_json::{json, Map, Value};

use crate::messages as m;
use crate::rules::{catalog, RuleId};

/// The envelope version this tool reads.
pub const VERSION: u64 = asset_rules::catalog::RULES_FILE_VERSION;

/// Every rule's settings, read from a texture rules file or the defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    /// In [`RuleId::ALL`] order.
    rules: Vec<RuleSettings>,
}

impl Profile {
    /// Every rule at its default.
    pub fn defaults() -> Profile {
        Profile {
            name: m::defaults_name(),
            rules: RuleId::ALL
                .iter()
                .map(|rule| rule.rule().defaults(None))
                .collect(),
        }
    }

    pub fn rule(&self, rule: RuleId) -> &RuleSettings {
        &self.rules[rule as usize]
    }

    /// Reads a texture rules file. `Err` says why it can't be used; `warnings` gets the lines
    /// saying what in it was passed over.
    pub fn from_json(
        json: &str,
        file: &str,
        warnings: &mut Vec<String>,
    ) -> Result<Profile, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let version = root.get("version").and_then(Value::as_u64);
        match version {
            Some(VERSION) => {}
            Some(found) => return Err(m::profile_version(found, VERSION)),
            None => return Err(m::profile_no_version()),
        }
        let problems = catalog().check_rules_file(&root);
        if !problems.is_empty() {
            warnings.push(m::passed_over(file));
            warnings.extend(problems.iter().map(|problem| format!("  {problem}")));
        }
        let profile = root.get("profile");
        let name = profile
            .and_then(|profile| profile.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(file)
            .to_owned();
        let entries = profile
            .and_then(|profile| profile.get("rules"))
            .and_then(Value::as_object);
        let rules = RuleId::ALL
            .iter()
            .map(|rule| {
                let entry = entries.and_then(|entries| entries.get(rule.id()));
                rule.rule().settings(None, entry)
            })
            .collect();
        Ok(Profile { name, rules })
    }

    /// Reads the texture rules file at `path`, with the digest results and the cache record it by.
    pub fn load(path: &Path, warnings: &mut Vec<String>) -> Result<(Profile, String), String> {
        let shown = path.display().to_string();
        let bytes =
            std::fs::read(path).map_err(|e| m::profile_unreadable(&shown, &e.to_string()))?;
        let json = String::from_utf8_lossy(&bytes);
        let profile = Profile::from_json(&json, &shown, warnings)
            .map_err(|reason| m::profile_unreadable(&shown, &reason))?;
        Ok((profile, sha256(&bytes)))
    }

    /// The digest results record for the built-in defaults: of their settings, written out.
    pub fn defaults_digest() -> String {
        let defaults = Profile::defaults();
        let rules: Map<String, Value> = RuleId::ALL
            .iter()
            .map(|&rule| {
                let settings = defaults.rule(rule);
                (
                    rule.id().to_owned(),
                    json!({
                        "enabled": settings.enabled,
                        "severity": settings.severity,
                        "params": settings.params,
                    }),
                )
            })
            .collect();
        sha256(Value::Object(rules).to_string().as_bytes())
    }
}

/// `sha256:<hex>`, as asset-rules writes a digest.
fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

#[cfg(test)]
mod tests {
    use asset_rules::results::Severity;

    use super::*;
    use crate::rules::params;

    fn read(json: &str) -> (Result<Profile, String>, Vec<String>) {
        let mut warnings = Vec::new();
        (Profile::from_json(json, "t.json", &mut warnings), warnings)
    }

    /// The plan's sketch: a rule says what it changes, and the rest is the default.
    #[test]
    fn a_hand_written_file_needs_only_what_it_changes() {
        let (profile, warnings) = read(
            r#"{ "version": 1, "generator": "hand-written",
                 "profile": { "name": "Prop texture (production)", "rules": {
                   "texture.max_size": { "severity": "error", "params": { "max": 2048 },
                                         "note": "Quest budget for props." },
                   "texture.power_of_two": { "severity": "error" },
                   "texture.unused_alpha": {},
                   "texture.color_profile": { "enabled": false }
                 } } }"#,
        );
        let profile = profile.unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(profile.name, "Prop texture (production)");

        let max = profile.rule(RuleId::MaxSize);
        assert!(max.enabled);
        assert_eq!(max.severity, Severity::Error);
        assert_eq!(params::MaxSize::read(max).max, 2048);
        assert_eq!(max.note.as_deref(), Some("Quest budget for props."));

        let alpha = profile.rule(RuleId::UnusedAlpha);
        assert!(alpha.enabled, "listing a rule switches it on");
        assert_eq!(alpha.severity, Severity::Warning, "its default severity");

        assert!(!profile.rule(RuleId::ColorProfile).enabled);
        assert!(
            profile.rule(RuleId::Format).enabled,
            "unlisted: the default"
        );
        assert!(
            !profile.rule(RuleId::Square).enabled,
            "unlisted: the default"
        );
    }

    #[test]
    fn unknown_and_malformed_rules_are_passed_over_with_a_warning() {
        let (profile, warnings) = read(
            r#"{ "version": 1, "profile": { "rules": {
                   "texture.max_sise": { "params": { "max": 8 } },
                   "texture.square": { "severity": "catastrophic" }
                 } } }"#,
        );
        let profile = profile.unwrap();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].starts_with("t.json:"), "{warnings:?}");
        assert!(warnings[1].contains("texture.max_sise"), "{warnings:?}");
        assert!(warnings[2].contains("texture.square"), "{warnings:?}");
        assert!(
            !profile.rule(RuleId::Square).enabled,
            "malformed: its default"
        );
        assert_eq!(profile.name, "t.json", "no name: the file's");
    }

    #[test]
    fn a_newer_version_or_bad_json_is_refused() {
        assert!(read(r#"{ "version": 2, "profile": {} }"#).0.is_err());
        assert!(read(r#"{ "profile": {} }"#).0.is_err());
        assert!(read("not json").0.is_err());
    }

    #[test]
    fn a_parameter_the_rule_cant_take_keeps_the_default() {
        let (profile, warnings) = read(
            r#"{ "version": 1, "profile": { "rules": {
                   "texture.max_size": { "params": { "max": "big" } },
                   "texture.min_size": { "params": { "min": 0 } } } } }"#,
        );
        let profile = profile.unwrap();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert_eq!(
            params::MaxSize::read(profile.rule(RuleId::MaxSize)).max,
            4096
        );
        assert_eq!(
            params::MinSize::read(profile.rule(RuleId::MinSize)).min,
            16,
            "the right kind but out of range"
        );
        assert!(profile.rule(RuleId::MinSize).enabled, "listed, so on");
    }

    /// Only rules about something Unity gets wrong whatever the policy are on by default.
    #[test]
    fn the_defaults_are_unitys_own_problems() {
        let defaults = Profile::defaults();
        let on: Vec<&str> = RuleId::ALL
            .into_iter()
            .filter(|&rule| defaults.rule(rule).enabled)
            .map(RuleId::id)
            .collect();
        assert_eq!(
            on,
            [
                "texture.psd_composite",
                "texture.format",
                "texture.color_profile"
            ]
        );
    }
}
