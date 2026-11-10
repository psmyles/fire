//! The installer is the one copy of fire-decode's extension list that cannot `use` it:
//! `installer/fire.iss` is an Inno Setup script, and its per-format
//! `Capabilities\FileAssociations` entries are what put fire in Explorer's "Open with" and Default
//! Apps. If the two disagree, an installed fire either claims a format it cannot decode or fails
//! to offer one it can — so read the script and compare the sets outright.
//!
//! It lives here, not in fire-decode, because it `include_str!`s fire's installer (fire-decode
//! plan, FD2), and it compares against `all_extensions()`, which holds whatever features this
//! build has, so it passes in CI's no-features job too (FD9).

/// Extensions live in lines of the form:
///
/// ```text
/// ...Capabilities\FileAssociations"; ValueType: string; ValueName: ".png"; ValueData: "Fire.png"...
/// ```
#[test]
fn installer_associations_match_the_extension_table() {
    const ISS: &str = include_str!("../../../installer/fire.iss");

    let mut registered: Vec<String> = Vec::new();
    for line in ISS.lines() {
        if !line.contains(r"Capabilities\FileAssociations") {
            continue;
        }
        // Pull the `ValueName: ".ext"` field out of the line.
        let Some(rest) = line.split(r#"ValueName: "."#).nth(1) else {
            continue;
        };
        let Some(ext) = rest.split('"').next() else {
            continue;
        };
        registered.push(ext.to_ascii_lowercase());
    }
    assert!(
        !registered.is_empty(),
        "parsed no associations out of fire.iss — the script's format changed, and this test \
         is now silently vacuous"
    );

    let installer: std::collections::BTreeSet<&str> =
        registered.iter().map(|s| s.as_str()).collect();
    let decoder: std::collections::BTreeSet<&str> =
        fire_decode::all_extensions().iter().copied().collect();

    let missing: Vec<_> = decoder.difference(&installer).collect();
    let extra: Vec<_> = installer.difference(&decoder).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "installer/fire.iss and fire_decode::all_extensions() disagree.\n  \
         decodable but not associated: {missing:?}\n  \
         associated but not decodable: {extra:?}"
    );
}
