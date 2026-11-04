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
//! Loaded leniently, as 3d-review-cli loads a profile, since these files are written by hand:
//! - a rule this tool doesn't know is skipped (with a warning, so a misspelt id is noticed);
//! - a listed rule is on unless it says `"enabled": false`;
//! - whatever a rule leaves out (severity, any parameter) is the tool's default for it;
//! - a rule whose settings can't be read takes its defaults whole;
//! - a rule the file doesn't list takes the tool's default, which is off for every rule that is
//!   a matter of team policy. Only rules about something Unity gets wrong whatever the policy
//!   are on by default: the format, a PSD's merged image, and the colour profile.

use std::collections::BTreeMap;
use std::path::Path;

use asset_rules::results::Severity;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::messages as m;

/// The envelope version this tool reads.
pub const VERSION: u64 = 1;

/// Every rule `fire-cli` knows, in the order findings of equal severity are listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuleId {
    PsdComposite,
    Psb,
    Format,
    MaxSize,
    MinSize,
    PowerOfTwo,
    MultipleOf4,
    Square,
    BitDepth,
    UnusedAlpha,
    AlphaRequired,
    ColorProfile,
    GenerationMetadata,
    FileSize,
}

impl RuleId {
    pub const ALL: [RuleId; 14] = [
        RuleId::PsdComposite,
        RuleId::Psb,
        RuleId::Format,
        RuleId::MaxSize,
        RuleId::MinSize,
        RuleId::PowerOfTwo,
        RuleId::MultipleOf4,
        RuleId::Square,
        RuleId::BitDepth,
        RuleId::UnusedAlpha,
        RuleId::AlphaRequired,
        RuleId::ColorProfile,
        RuleId::GenerationMetadata,
        RuleId::FileSize,
    ];

    /// The id rules files and results use.
    pub fn id(self) -> &'static str {
        match self {
            RuleId::PsdComposite => "texture.psd_composite",
            RuleId::Psb => "texture.psb",
            RuleId::Format => "texture.format",
            RuleId::MaxSize => "texture.max_size",
            RuleId::MinSize => "texture.min_size",
            RuleId::PowerOfTwo => "texture.power_of_two",
            RuleId::MultipleOf4 => "texture.multiple_of_4",
            RuleId::Square => "texture.square",
            RuleId::BitDepth => "texture.bit_depth",
            RuleId::UnusedAlpha => "texture.unused_alpha",
            RuleId::AlphaRequired => "texture.alpha_required",
            RuleId::ColorProfile => "texture.color_profile",
            RuleId::GenerationMetadata => "texture.generation_metadata",
            RuleId::FileSize => "texture.file_size",
        }
    }

    pub fn from_id(id: &str) -> Option<RuleId> {
        RuleId::ALL.into_iter().find(|rule| rule.id() == id)
    }

    /// The tool's default: on or off, severity, and every parameter.
    fn default_config(self) -> RuleConfig {
        let (enabled, severity, params) = match self {
            RuleId::PsdComposite => (true, Severity::Error, json!({})),
            RuleId::Psb => (false, Severity::Error, json!({})),
            // The formats Unity's texture importer reads that Fire also decodes.
            RuleId::Format => (true, Severity::Error, json!({ "allowed": UNITY_FORMATS })),
            RuleId::MaxSize => (false, Severity::Error, json!({ "max": 4096 })),
            RuleId::MinSize => (false, Severity::Warning, json!({ "min": 16 })),
            RuleId::PowerOfTwo => (false, Severity::Warning, json!({})),
            RuleId::MultipleOf4 => (false, Severity::Warning, json!({})),
            RuleId::Square => (false, Severity::Warning, json!({})),
            RuleId::BitDepth => (
                false,
                Severity::Warning,
                json!({ "allowed": [8], "palette": false }),
            ),
            RuleId::UnusedAlpha => (false, Severity::Warning, json!({})),
            RuleId::AlphaRequired => (false, Severity::Error, json!({})),
            RuleId::ColorProfile => (true, Severity::Warning, json!({})),
            RuleId::GenerationMetadata => (false, Severity::Warning, json!({})),
            RuleId::FileSize => (false, Severity::Warning, json!({ "max_mb": 50 })),
        };
        RuleConfig {
            enabled,
            severity,
            params: match params {
                Value::Object(map) => map,
                _ => Map::new(),
            },
            note: None,
        }
    }
}

/// The formats Unity's texture importer lists that Fire decodes, as `texture.format` names them.
pub const UNITY_FORMATS: [&str; 9] = [
    "png", "jpg", "tga", "psd", "tiff", "exr", "hdr", "bmp", "gif",
];

/// One rule's settings in a profile.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleConfig {
    pub enabled: bool,
    pub severity: Severity,
    /// The tool's defaults, with the file's values over them.
    pub params: Map<String, Value>,
    pub note: Option<String>,
}

impl RuleConfig {
    /// A whole-number parameter of at least 1, or `None` if it isn't one.
    pub fn count(&self, name: &str) -> Option<u32> {
        let value = self.params.get(name)?.as_f64()?;
        (value >= 1.0 && value <= f64::from(u32::MAX)).then_some(value.round() as u32)
    }

    /// A number parameter, if it is one.
    pub fn number(&self, name: &str) -> Option<f64> {
        self.params.get(name)?.as_f64().filter(|n| n.is_finite())
    }

    pub fn flag(&self, name: &str) -> Option<bool> {
        self.params.get(name)?.as_bool()
    }

    /// A list parameter's strings, lowercased; non-strings are dropped.
    pub fn words(&self, name: &str) -> Option<Vec<String>> {
        let items = self.params.get(name)?.as_array()?;
        Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect(),
        )
    }

    /// A list parameter's whole numbers from 1 to 255; anything else is dropped.
    pub fn depths(&self, name: &str) -> Option<Vec<u8>> {
        let items = self.params.get(name)?.as_array()?;
        Some(
            items
                .iter()
                .filter_map(Value::as_u64)
                .filter_map(|n| u8::try_from(n).ok())
                .filter(|&n| n > 0)
                .collect(),
        )
    }
}

/// Every rule's settings, read from a texture rules file or the defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    rules: BTreeMap<RuleId, RuleConfig>,
}

/// One rule as written: every field may be left out.
#[derive(Deserialize)]
struct RuleIn {
    enabled: Option<bool>,
    severity: Option<Severity>,
    #[serde(default)]
    params: Map<String, Value>,
    note: Option<String>,
}

impl Profile {
    /// Every rule at the tool's default.
    pub fn defaults() -> Profile {
        Profile {
            name: m::defaults_name(),
            rules: RuleId::ALL
                .into_iter()
                .map(|rule| (rule, rule.default_config()))
                .collect(),
        }
    }

    pub fn rule(&self, rule: RuleId) -> &RuleConfig {
        &self.rules[&rule]
    }

    /// Reads a texture rules file. `Err` says why it can't be used; `warnings` gets one line per
    /// rule it skipped or defaulted.
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
        let profile = root.get("profile").cloned().unwrap_or(Value::Null);
        let mut out = Profile::defaults();
        out.name = profile
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(file)
            .to_owned();
        let rules = profile.get("rules").and_then(Value::as_object);
        for (id, value) in rules.into_iter().flatten() {
            let Some(rule) = RuleId::from_id(id) else {
                warnings.push(m::unknown_rule(file, id));
                continue;
            };
            let Ok(read) = serde_json::from_value::<RuleIn>(value.clone()) else {
                warnings.push(m::malformed_rule(file, id));
                continue;
            };
            let config = out.rules.get_mut(&rule).expect("every rule has a config");
            config.enabled = read.enabled.unwrap_or(true);
            if let Some(severity) = read.severity {
                config.severity = severity;
            }
            // A value of the wrong kind (`"max": "big"`) leaves the default in place.
            for (name, value) in read.params {
                let fits = config
                    .params
                    .get(&name)
                    .is_none_or(|default| same_kind(default, &value));
                if fits {
                    config.params.insert(name, value);
                }
            }
            config.note = read.note.filter(|note| !note.trim().is_empty());
        }
        Ok(out)
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
        let rules: Map<String, Value> = Profile::defaults()
            .rules
            .iter()
            .map(|(rule, config)| {
                (
                    rule.id().to_owned(),
                    json!({
                        "enabled": config.enabled,
                        "severity": config.severity,
                        "params": config.params,
                    }),
                )
            })
            .collect();
        sha256(Value::Object(rules).to_string().as_bytes())
    }
}

fn same_kind(a: &Value, b: &Value) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
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
    use super::*;

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
        assert_eq!(max.count("max"), Some(2048));
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
    fn unknown_and_malformed_rules_are_skipped_with_a_warning() {
        let (profile, warnings) = read(
            r#"{ "version": 1, "profile": { "rules": {
                   "texture.max_sise": { "params": { "max": 8 } },
                   "texture.square": { "severity": "catastrophic" }
                 } } }"#,
        );
        let profile = profile.unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("texture.max_sise"));
        assert!(warnings[1].contains("texture.square"));
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
    fn a_parameter_of_the_wrong_kind_keeps_the_default() {
        let (profile, _) = read(
            r#"{ "version": 1, "profile": { "rules": {
                   "texture.max_size": { "params": { "max": "big" } },
                   "texture.min_size": { "params": { "min": 0 } } } } }"#,
        );
        let profile = profile.unwrap();
        assert_eq!(profile.rule(RuleId::MaxSize).count("max"), Some(4096));
        assert_eq!(
            profile.rule(RuleId::MinSize).count("min"),
            None,
            "the right kind but out of range: the rule can't run"
        );
    }

    #[test]
    fn every_rule_id_round_trips() {
        for rule in RuleId::ALL {
            assert_eq!(RuleId::from_id(rule.id()), Some(rule));
        }
    }
}
