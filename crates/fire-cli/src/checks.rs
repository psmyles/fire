//! The v1 rules (AC12): each one measures a fact about an image against its settings, and says
//! what it found in the artist's words.
//!
//! A rule that needs what only a decode tells (the alpha channel, the colour profile) doesn't run
//! for an image that couldn't be decoded: a PSB, or a PSD with no merged image. A rule whose
//! limit is set to something it can't use (`"max": 0`) doesn't run either.

use asset_rules::results::Finding;
use serde_json::{json, Map, Value};

use crate::facts::{normalize_format, Facts};
use crate::messages::{self as m, Text};
use crate::profile::{Profile, RuleId, UNITY_FORMATS};

/// Every finding for `facts` under `profile`: worst first, then in rule order.
pub fn evaluate(facts: &Facts, profile: &Profile) -> Vec<Finding> {
    let mut found: Vec<(RuleId, Finding)> = Vec::new();
    for rule in RuleId::ALL {
        let config = profile.rule(rule);
        if !config.enabled {
            continue;
        }
        for (text, measured, limit) in measure(rule, facts, profile) {
            let mut finding = Finding::new(
                rule.id(),
                config.severity,
                text.title,
                text.what,
                text.why,
                text.how,
            );
            finding.note = config.note.clone();
            finding.measured = object(measured);
            finding.limit = object(limit);
            found.push((rule, finding));
        }
    }
    found.sort_by_key(|(rule, finding)| (std::cmp::Reverse(finding.severity), *rule));
    found.into_iter().map(|(_, finding)| finding).collect()
}

/// What `rule` finds: the text, the figures measured and the limit they were held to. Most
/// rules find one thing or nothing; `texture.bit_depth` can find two.
fn measure(rule: RuleId, facts: &Facts, profile: &Profile) -> Vec<(Text, Value, Value)> {
    let config = profile.rule(rule);
    let (w, h) = (facts.width, facts.height);
    let size = json!({ "width": w, "height": h });
    let one = |found: Option<(Text, Value, Value)>| found.into_iter().collect();
    match rule {
        RuleId::PsdComposite => one(facts.no_merged_image.then(|| {
            (
                m::psd_composite(),
                json!({ "merged_image": false }),
                Value::Null,
            )
        })),
        RuleId::Psb => one(facts.psb.then(|| {
            (
                m::psb(),
                json!({ "format": "psb" }),
                json!({ "format": "psd" }),
            )
        })),
        RuleId::Format => {
            let allowed: Vec<String> = config
                .words("allowed")
                .unwrap_or_default()
                .iter()
                .map(|id| normalize_format(id))
                .collect();
            let id = normalize_format(&facts.format.id);
            one((!allowed.contains(&id)).then(|| {
                let unity_reads = UNITY_FORMATS.contains(&id.as_str());
                (
                    m::format(&facts.format.name, &allowed, unity_reads),
                    json!({ "format": id }),
                    json!({ "allowed": allowed }),
                )
            }))
        }
        RuleId::MaxSize => one(config.count("max").and_then(|max| {
            (w > max || h > max).then(|| (m::max_size(w, h, max), size, json!({ "max": max })))
        })),
        RuleId::MinSize => one(config.count("min").and_then(|min| {
            (w < min || h < min).then(|| (m::min_size(w, h, min), size, json!({ "min": min })))
        })),
        RuleId::PowerOfTwo => one((!(w.is_power_of_two() && h.is_power_of_two())).then(|| {
            let suggested = (nearest_power_of_two(w), nearest_power_of_two(h));
            (
                m::power_of_two(w, h, suggested),
                size,
                json!({ "power_of_two": true }),
            )
        })),
        RuleId::MultipleOf4 => one((w % 4 != 0 || h % 4 != 0).then(|| {
            let suggested = (nearest_multiple_of_4(w), nearest_multiple_of_4(h));
            (
                m::multiple_of_4(w, h, suggested),
                size,
                json!({ "multiple_of": 4 }),
            )
        })),
        RuleId::Square => one((w != h).then(|| (m::square(w, h), size, json!({ "square": true })))),
        RuleId::BitDepth => {
            let allowed = config
                .depths("allowed")
                .filter(|depths| !depths.is_empty())
                .unwrap_or_else(|| vec![8]);
            let palette_ok = config.flag("palette").unwrap_or(false);
            let measured = json!({
                "bit_depth": facts.bit_depth,
                "float": facts.float,
                "palette": facts.palette,
            });
            let mut found = Vec::new();
            if !allowed.contains(&facts.bit_depth) {
                found.push((
                    m::bit_depth(facts.bit_depth, facts.float, &allowed),
                    measured.clone(),
                    json!({ "allowed": allowed }),
                ));
            }
            if facts.palette && !palette_ok {
                found.push((
                    m::palette(facts.bit_depth),
                    measured,
                    json!({ "palette": false }),
                ));
            }
            found
        }
        RuleId::UnusedAlpha => one(facts.pixels.as_ref().and_then(|pixels| {
            (has_alpha(pixels.channels) && pixels.alpha_opaque).then(|| {
                (
                    m::unused_alpha(),
                    json!({ "channels": pixels.channels, "alpha_opaque": true }),
                    Value::Null,
                )
            })
        })),
        RuleId::AlphaRequired => one(facts.pixels.as_ref().and_then(|pixels| {
            let has_channel = has_alpha(pixels.channels);
            (!has_channel || pixels.alpha_opaque).then(|| {
                (
                    m::alpha_required(has_channel),
                    json!({
                        "channels": pixels.channels,
                        "alpha_opaque": has_channel && pixels.alpha_opaque,
                    }),
                    json!({ "alpha": true }),
                )
            })
        })),
        RuleId::ColorProfile => one(facts
            .pixels
            .as_ref()
            .and_then(|pixels| pixels.icc.as_ref())
            .filter(|icc| !icc.srgb)
            .map(|icc| {
                (
                    m::color_profile(icc.description.as_deref()),
                    json!({ "profile": icc.description }),
                    json!({ "profile": "sRGB" }),
                )
            })),
        RuleId::GenerationMetadata => one((!facts.generation.is_empty()).then(|| {
            (
                m::generation_metadata(&facts.generation),
                json!({ "found": facts.generation }),
                Value::Null,
            )
        })),
        RuleId::FileSize => one(config
            .number("max_mb")
            .filter(|mb| *mb > 0.0)
            .and_then(|mb| {
                let max_bytes = (mb * 1024.0 * 1024.0) as u64;
                (facts.file_size > max_bytes).then(|| {
                    (
                        m::file_size_too_big(facts.file_size, max_bytes),
                        json!({ "bytes": facts.file_size }),
                        json!({ "max_mb": mb }),
                    )
                })
            })),
    }
}

fn has_alpha(channels: u8) -> bool {
    matches!(channels, 2 | 4)
}

/// The power of two closest to `n` (the smaller on a tie), at least 1.
fn nearest_power_of_two(n: u32) -> u32 {
    let up = n.max(1).checked_next_power_of_two().unwrap_or(1 << 31);
    let down = if up > n { up / 2 } else { up };
    if down >= 1 && n - down <= up - n {
        down.max(1)
    } else {
        up
    }
}

/// The multiple of 4 closest to `n`, at least 4.
fn nearest_multiple_of_4(n: u32) -> u32 {
    (n.saturating_add(2) / 4 * 4).max(4)
}

fn object(value: Value) -> Option<Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use asset_rules::results::Severity;

    use super::*;
    use crate::facts::{Format, Icc, Pixels};

    fn png(width: u32, height: u32) -> Facts {
        Facts {
            file_size: 1000,
            format: Format {
                id: "png".into(),
                name: "PNG".into(),
            },
            width,
            height,
            surfaces: None,
            bit_depth: 8,
            float: false,
            palette: false,
            pixels: Some(Pixels {
                channels: 3,
                alpha_opaque: false,
                icc: None,
            }),
            no_merged_image: false,
            psb: false,
            generation: Vec::new(),
        }
    }

    fn profile(rules: &str) -> Profile {
        let json = format!(r#"{{ "version": 1, "profile": {{ "rules": {rules} }} }}"#);
        Profile::from_json(&json, "test", &mut Vec::new()).unwrap()
    }

    fn rules_found(facts: &Facts, rules: &str) -> Vec<String> {
        evaluate(facts, &profile(rules))
            .into_iter()
            .map(|f| f.rule)
            .collect()
    }

    #[test]
    fn a_clean_image_under_the_defaults_has_no_findings() {
        assert!(evaluate(&png(512, 512), &Profile::defaults()).is_empty());
    }

    #[test]
    fn sizes() {
        let rules = r#"{ "texture.max_size": { "params": { "max": 2048 } },
                         "texture.min_size": { "params": { "min": 64 } },
                         "texture.power_of_two": {}, "texture.multiple_of_4": {},
                         "texture.square": {} }"#;
        assert!(rules_found(&png(1024, 1024), rules).is_empty());
        assert_eq!(rules_found(&png(4096, 4096), rules), ["texture.max_size"]);
        assert_eq!(
            rules_found(&png(30, 64), rules),
            [
                "texture.min_size",
                "texture.power_of_two",
                "texture.multiple_of_4",
                "texture.square"
            ]
        );
    }

    /// The plan's example finding, figures and all.
    #[test]
    fn a_finding_carries_its_figures_note_and_severity() {
        let found = evaluate(
            &png(4096, 4096),
            &profile(
                r#"{ "texture.max_size": { "severity": "error", "params": { "max": 2048 },
                                           "note": "Quest budget for props." } }"#,
            ),
        );
        let [finding] = found.as_slice() else {
            panic!("{found:?}");
        };
        assert_eq!(finding.severity, Severity::Error);
        assert_eq!(finding.title, "Too large");
        assert_eq!(
            finding.what,
            "This image is 4096×4096. The most allowed is 2048×2048."
        );
        assert_eq!(finding.note.as_deref(), Some("Quest budget for props."));
        assert_eq!(
            Value::Object(finding.measured.clone().unwrap()),
            json!({ "width": 4096, "height": 4096 })
        );
        assert_eq!(
            Value::Object(finding.limit.clone().unwrap()),
            json!({ "max": 2048 })
        );
    }

    #[test]
    fn worst_first_then_rule_order() {
        let found = evaluate(
            &png(30, 30),
            &profile(
                r#"{ "texture.square": {}, "texture.power_of_two": { "severity": "info" },
                     "texture.min_size": { "severity": "error", "params": { "min": 64 } } }"#,
            ),
        );
        let order: Vec<(&str, Severity)> = found
            .iter()
            .map(|f| (f.rule.as_str(), f.severity))
            .collect();
        assert_eq!(
            order,
            [
                ("texture.min_size", Severity::Error),
                ("texture.power_of_two", Severity::Info)
            ]
        );
    }

    #[test]
    fn format_goes_by_content_and_spelling_doesnt_matter() {
        let mut webp = png(64, 64);
        webp.format = Format {
            id: "webp".into(),
            name: "WebP".into(),
        };
        let found = evaluate(&webp, &Profile::defaults());
        assert_eq!(found.len(), 1);
        assert!(found[0].why.contains("can't read"), "{}", found[0].why);

        let mut jpeg = png(64, 64);
        jpeg.format = Format {
            id: "jpg".into(),
            name: "JPEG".into(),
        };
        let rules = r#"{ "texture.format": { "params": { "allowed": ["PNG", "jpeg"] } } }"#;
        assert!(rules_found(&jpeg, rules).is_empty());
        let rules = r#"{ "texture.format": { "params": { "allowed": ["png", "tga"] } } }"#;
        let found = evaluate(&jpeg, &profile(rules));
        assert!(found[0].why.contains("agreed formats"), "Unity reads JPEG");
    }

    #[test]
    fn alpha() {
        let rules = r#"{ "texture.unused_alpha": {}, "texture.alpha_required": {} }"#;
        let with = |channels, alpha_opaque| {
            let mut facts = png(64, 64);
            facts.pixels = Some(Pixels {
                channels,
                alpha_opaque,
                icc: None,
            });
            rules_found(&facts, rules)
        };
        assert_eq!(
            with(4, true),
            ["texture.alpha_required", "texture.unused_alpha"],
            "the error first"
        );
        assert!(with(4, false).is_empty());
        assert_eq!(with(3, false), ["texture.alpha_required"]);
    }

    #[test]
    fn bit_depth_and_palette() {
        let rules = r#"{ "texture.bit_depth": {} }"#;
        let mut deep = png(64, 64);
        deep.bit_depth = 16;
        assert_eq!(rules_found(&deep, rules), ["texture.bit_depth"]);
        let allowed = r#"{ "texture.bit_depth": { "params": { "allowed": [8, 16] } } }"#;
        assert!(rules_found(&deep, allowed).is_empty());

        let mut palette = png(64, 64);
        palette.palette = true;
        palette.bit_depth = 4;
        let found = evaluate(&palette, &profile(rules));
        let titles: Vec<&str> = found.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(titles, ["Bit depth not allowed", "Palette image"]);
        let palette_ok =
            r#"{ "texture.bit_depth": { "params": { "allowed": [4, 8], "palette": true } } }"#;
        assert!(rules_found(&palette, palette_ok).is_empty());
    }

    #[test]
    fn colour_profile() {
        let mut p3 = png(64, 64);
        p3.pixels = Some(Pixels {
            channels: 3,
            alpha_opaque: false,
            icc: Some(Icc {
                description: Some("Display P3".into()),
                srgb: false,
            }),
        });
        let found = evaluate(&p3, &Profile::defaults());
        assert_eq!(found.len(), 1);
        assert!(
            found[0].what.contains("\"Display P3\""),
            "{}",
            found[0].what
        );
    }

    #[test]
    fn photoshop_files_without_pixels_get_the_header_rules() {
        let mut facts = png(8000, 8000);
        facts.format = Format {
            id: "psd".into(),
            name: "PSD".into(),
        };
        facts.pixels = None;
        facts.no_merged_image = true;
        let rules = r#"{ "texture.max_size": { "params": { "max": 4096 } },
                         "texture.unused_alpha": {}, "texture.color_profile": {} }"#;
        assert_eq!(
            rules_found(&facts, rules),
            ["texture.psd_composite", "texture.max_size"]
        );
    }

    #[test]
    fn file_size_and_generation_metadata() {
        let mut facts = png(64, 64);
        facts.file_size = 3 * 1024 * 1024;
        facts.generation = vec!["a \"parameters\" text chunk".into()];
        let rules = r#"{ "texture.file_size": { "params": { "max_mb": 2 } },
                         "texture.generation_metadata": {} }"#;
        let found = evaluate(&facts, &profile(rules));
        let rules: Vec<&str> = found.iter().map(|f| f.rule.as_str()).collect();
        assert_eq!(rules, ["texture.generation_metadata", "texture.file_size"]);
        assert!(found[1].what.contains("3.0 MB"), "{}", found[1].what);
    }

    #[test]
    fn suggestions() {
        assert_eq!(nearest_power_of_two(1000), 1024);
        assert_eq!(nearest_power_of_two(700), 512);
        assert_eq!(nearest_power_of_two(768), 512, "a tie goes down");
        assert_eq!(nearest_power_of_two(1), 1);
        assert_eq!(nearest_multiple_of_4(1001), 1000);
        assert_eq!(nearest_multiple_of_4(1002), 1004);
        assert_eq!(nearest_multiple_of_4(1), 4);
    }
}
