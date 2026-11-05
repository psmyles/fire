//! The rules' checks (AC12): each one measures a fact about an image against its rule's settings.
//! The rules themselves, their settings and their texts are in `rules-catalog.toml`
//! ([`crate::rules`]); a check says whether its rule found something, and hands the rule's texts
//! what they name ([`found`]). Matching on [`RuleId`] makes a rule added to the catalog fail the
//! build until its check is here.
//!
//! A rule that needs what only a decode tells (the alpha channel, the colour profile) doesn't run
//! for an image that couldn't be decoded: a PSB, or a PSD with no merged image.

use asset_rules::catalog::RuleSettings;
use asset_rules::results::Finding;
use serde_json::{json, Map, Value};

use crate::facts::{normalize_format, Facts};
use crate::messages as m;
use crate::profile::Profile;
use crate::rules::{found, params, Found, RuleId};

/// Every finding for `facts` under `profile`: worst first, then in rule order.
pub fn evaluate(facts: &Facts, profile: &Profile) -> Vec<Finding> {
    let mut out: Vec<(RuleId, Finding)> = Vec::new();
    for rule in RuleId::ALL {
        let settings = profile.rule(rule);
        if !settings.enabled {
            continue;
        }
        for (what, measured, limit) in measure(rule, facts, settings) {
            debug_assert_eq!(what.rule(), rule, "a check hands its own rule's texts");
            let texts = what.texts(None, settings);
            let mut finding = Finding::new(
                rule.id(),
                settings.severity,
                texts.title,
                texts.what,
                texts.why,
                texts.how,
            );
            finding.note = settings.note.clone();
            finding.measured = object(measured);
            finding.limit = object(limit);
            out.push((rule, finding));
        }
    }
    out.sort_by_key(|(rule, finding)| (std::cmp::Reverse(finding.severity), *rule));
    out.into_iter().map(|(_, finding)| finding).collect()
}

/// What `rule` finds: what it hands its texts, the figures measured and the limit they were held
/// to. Most rules find one thing or nothing; `texture.bit_depth` can find two.
fn measure(rule: RuleId, facts: &Facts, settings: &RuleSettings) -> Vec<(Found, Value, Value)> {
    let (w, h) = (facts.width, facts.height);
    let size = json!({ "width": w, "height": h });
    let one = |found: Option<(Found, Value, Value)>| found.into_iter().collect();
    match rule {
        RuleId::PsdComposite => one(facts.no_merged_image.then(|| {
            (
                found::PsdComposite.into(),
                json!({ "merged_image": false }),
                Value::Null,
            )
        })),
        RuleId::Psb => one(facts.psb.then(|| {
            (
                found::Psb.into(),
                json!({ "format": "psb" }),
                json!({ "format": "psd" }),
            )
        })),
        RuleId::Format => {
            let allowed: Vec<String> = params::Format::read(settings)
                .allowed
                .iter()
                .map(|id| normalize_format(id))
                .collect();
            let id = normalize_format(&facts.format.id);
            one((!allowed.contains(&id)).then(|| {
                let upper: Vec<String> = allowed.iter().map(|f| f.to_uppercase()).collect();
                let found = found::Format {
                    case: (!unity_reads(&id)).then_some(found::FormatCase::UnityCantRead),
                    a_label: m::a_format(&facts.format.name),
                    allowed_formats: upper.join(", "),
                    target: upper.first().cloned().unwrap_or_else(|| "PNG".to_owned()),
                };
                (
                    found.into(),
                    json!({ "format": id }),
                    json!({ "allowed": allowed }),
                )
            }))
        }
        RuleId::MaxSize => {
            let max = params::MaxSize::read(settings).max;
            one((w > max || h > max).then(|| {
                let found = found::MaxSize {
                    width: w.to_string(),
                    height: h.to_string(),
                };
                (found.into(), size, json!({ "max": max }))
            }))
        }
        RuleId::MinSize => {
            let min = params::MinSize::read(settings).min;
            one((w < min || h < min).then(|| {
                let found = found::MinSize {
                    width: w.to_string(),
                    height: h.to_string(),
                };
                (found.into(), size, json!({ "min": min }))
            }))
        }
        RuleId::PowerOfTwo => one((!(w.is_power_of_two() && h.is_power_of_two())).then(|| {
            let found = found::PowerOfTwo {
                width: w.to_string(),
                height: h.to_string(),
                suggested_width: nearest_power_of_two(w).to_string(),
                suggested_height: nearest_power_of_two(h).to_string(),
            };
            (found.into(), size, json!({ "power_of_two": true }))
        })),
        RuleId::MultipleOf4 => one((w % 4 != 0 || h % 4 != 0).then(|| {
            let found = found::MultipleOf4 {
                width: w.to_string(),
                height: h.to_string(),
                suggested_width: nearest_multiple_of_4(w).to_string(),
                suggested_height: nearest_multiple_of_4(h).to_string(),
            };
            (found.into(), size, json!({ "multiple_of": 4 }))
        })),
        RuleId::Square => one((w != h).then(|| {
            let found = found::Square {
                width: w.to_string(),
                height: h.to_string(),
                side: w.max(h).to_string(),
            };
            (found.into(), size, json!({ "square": true }))
        })),
        RuleId::BitDepth => {
            let params = params::BitDepth::read(settings);
            let allowed: Vec<u8> = params
                .allowed
                .iter()
                .filter_map(|&depth| u8::try_from(depth).ok())
                .collect();
            let allowed = if allowed.is_empty() { vec![8] } else { allowed };
            let measured = json!({
                "bit_depth": facts.bit_depth,
                "float": facts.float,
                "palette": facts.palette,
            });
            // Both findings name the same values; each texts uses its own.
            let named = found::BitDepth {
                case: None,
                depth: m::depth(facts.bit_depth, facts.float),
                allowed_depths: m::depths(&allowed),
                convert_to: allowed[0].to_string(),
                a_bits: m::a_bits(facts.bit_depth),
                colours: m::palette_colours(facts.bit_depth).to_string(),
            };
            let mut out = Vec::new();
            if !allowed.contains(&facts.bit_depth) {
                out.push((
                    named.clone().into(),
                    measured.clone(),
                    json!({ "allowed": allowed }),
                ));
            }
            if facts.palette && !params.palette {
                let palette = found::BitDepth {
                    case: Some(found::BitDepthCase::Palette),
                    ..named
                };
                out.push((palette.into(), measured, json!({ "palette": false })));
            }
            out
        }
        RuleId::UnusedAlpha => one(facts.pixels.as_ref().and_then(|pixels| {
            (has_alpha(pixels.channels) && pixels.alpha_opaque).then(|| {
                (
                    found::UnusedAlpha.into(),
                    json!({ "channels": pixels.channels, "alpha_opaque": true }),
                    Value::Null,
                )
            })
        })),
        RuleId::AlphaRequired => one(facts.pixels.as_ref().and_then(|pixels| {
            let has_channel = has_alpha(pixels.channels);
            (!has_channel || pixels.alpha_opaque).then(|| {
                let found = found::AlphaRequired {
                    case: has_channel.then_some(found::AlphaRequiredCase::AlphaOpaque),
                };
                (
                    found.into(),
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
                let found = found::ColorProfile {
                    case: icc
                        .description
                        .is_none()
                        .then_some(found::ColorProfileCase::Unnamed),
                    profile: icc.description.clone().unwrap_or_default(),
                };
                (
                    found.into(),
                    json!({ "profile": icc.description }),
                    json!({ "profile": "sRGB" }),
                )
            })),
        RuleId::GenerationMetadata => one((!facts.generation.is_empty()).then(|| {
            let found = found::GenerationMetadata {
                found: facts.generation.join(", "),
            };
            (
                found.into(),
                json!({ "found": facts.generation }),
                Value::Null,
            )
        })),
        RuleId::FileSize => {
            let mb = params::FileSize::read(settings).max_mb;
            let max_bytes = (mb * 1024.0 * 1024.0) as u64;
            one((facts.file_size > max_bytes).then(|| {
                let (size, limit) = m::file_sizes(facts.file_size, max_bytes);
                (
                    found::FileSize { size, limit }.into(),
                    json!({ "bytes": facts.file_size }),
                    json!({ "max_mb": mb }),
                )
            }))
        }
    }
}

/// Whether Unity's texture importer reads the format `id`: the catalog lists the formats it reads
/// (that Fire decodes too) as `texture.format`'s options.
fn unity_reads(id: &str) -> bool {
    RuleId::Format.rule().param("allowed").is_some_and(|param| {
        param
            .options
            .iter()
            .any(|option| option.value.as_str() == Some(id))
    })
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
