//! `fire-cli` end to end: the built binary, run on generated images.
//!
//! Every fixture is written in code rather than committed: an image with an unused alpha channel,
//! a 16-bit one, one with a Display P3 profile, one with a generator's text chunk, a WebP, and a
//! PSD saved without a merged image. `--staged` runs in a temporary git repository with the
//! rules inside it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use asset_rules::results::{Results, Severity, Status};
use asset_rules::StageFrom;

// --- running it ---------------------------------------------------------------------------------

/// Runs `fire-cli` in `dir`, with git's system and global settings kept out.
fn fire_cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fire-cli"))
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", dir.join("no-global-config"))
        .output()
        .expect("run fire-cli")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("an exit code")
}

/// A folder under the system's temporary folder, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fire-cli-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The rule ids found in the results entry for `path`.
fn rules_of(results: &Results, path: &str) -> Vec<String> {
    entry(results, path)
        .findings
        .iter()
        .map(|f| f.rule.clone())
        .collect()
}

fn entry<'a>(results: &'a Results, path: &str) -> &'a asset_rules::results::FileResult {
    results
        .files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("no entry for {path}: {results:#?}"))
}

// --- fixtures -----------------------------------------------------------------------------------

fn png(width: u32, height: u32, color: image::ExtendedColorType, pixels: &[u8]) -> Vec<u8> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(pixels, width, height, color)
        .unwrap();
    out
}

fn rgb8(width: u32, height: u32) -> Vec<u8> {
    let pixels: Vec<u8> = (0..width * height * 3).map(|i| (i % 251) as u8).collect();
    png(width, height, image::ExtendedColorType::Rgb8, &pixels)
}

/// An RGBA image whose alpha is 255 everywhere.
fn opaque_rgba8(width: u32, height: u32) -> Vec<u8> {
    let pixels: Vec<u8> = (0..width * height)
        .flat_map(|i| [(i % 200) as u8, 40, 90, 255])
        .collect();
    png(width, height, image::ExtendedColorType::Rgba8, &pixels)
}

fn rgb16(width: u32, height: u32) -> Vec<u8> {
    let pixels: Vec<u8> = (0..width * height * 3)
        .flat_map(|i| ((i * 257) as u16).to_be_bytes())
        .collect();
    png(width, height, image::ExtendedColorType::Rgb16, &pixels)
}

/// A Display P3 profile: P3's primaries, D65 white and sRGB's tone curve, named as Apple's is.
fn display_p3() -> Vec<u8> {
    use lcms2::{CIExyY, CIExyYTRIPLE, Locale, Profile, Tag, TagSignature, ToneCurve, MLU};
    let xy = |x, y| CIExyY { x, y, Y: 1.0 };
    let trc =
        ToneCurve::new_parametric(4, &[2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045])
            .unwrap();
    let primaries = CIExyYTRIPLE {
        Red: xy(0.680, 0.320),
        Green: xy(0.265, 0.690),
        Blue: xy(0.150, 0.060),
    };
    let mut profile =
        Profile::new_rgb(&xy(0.3127, 0.3290), &primaries, &[&trc, &trc, &trc]).unwrap();
    let mut name = MLU::new(1);
    name.set_text("Display P3", Locale::none());
    profile.write_tag(TagSignature::ProfileDescriptionTag, Tag::MLU(&name));
    profile.icc().unwrap()
}

fn with_icc(icc: Vec<u8>) -> Vec<u8> {
    use image::ImageEncoder;
    let pixels: Vec<u8> = (0..16 * 16 * 3).map(|i| (i % 256) as u8).collect();
    let mut out = Vec::new();
    let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder.set_icc_profile(icc).unwrap();
    encoder
        .write_image(&pixels, 16, 16, image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// `png` with a `tEXt` chunk added after its header, as a generator writes one.
fn with_text(png: &[u8], key: &str, text: &str) -> Vec<u8> {
    let mut data = key.as_bytes().to_vec();
    data.push(0);
    data.extend_from_slice(text.as_bytes());
    let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
    let start = chunk.len();
    chunk.extend_from_slice(b"tEXt");
    chunk.extend_from_slice(&data);
    let crc = crc32(&chunk[start..]);
    chunk.extend_from_slice(&crc.to_be_bytes());
    let after_header = 8 + 8 + 13 + 4; // signature, IHDR's length and type, its data, its CRC
    [&png[..after_header], &chunk, &png[after_header..]].concat()
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

fn webp(width: u32, height: u32) -> Vec<u8> {
    let pixels: Vec<u8> = (0..width * height * 4).map(|i| (i % 256) as u8).collect();
    let mut out = Vec::new();
    image_webp::WebPEncoder::new(&mut out)
        .encode(&pixels, width, height, image_webp::ColorType::Rgba8)
        .unwrap();
    out
}

/// A 1×1 RGB PSD as Photoshop writes one without Maximize Compatibility: a white merged image,
/// and `hasRealMergedData` cleared in the Version Info resource.
#[cfg(feature = "psd")]
fn psd_without_merged_image() -> Vec<u8> {
    let mut info = 1u32.to_be_bytes().to_vec();
    info.push(0); // hasRealMergedData
    for name in ["Adobe Photoshop", "Adobe Photoshop 2026"] {
        let units: Vec<u16> = name.encode_utf16().collect();
        info.extend((units.len() as u32).to_be_bytes());
        info.extend(units.iter().flat_map(|u| u.to_be_bytes()));
    }
    info.extend(1u32.to_be_bytes());
    let mut resource = b"8BIM".to_vec();
    resource.extend(1057u16.to_be_bytes());
    resource.extend([0, 0]);
    resource.extend((info.len() as u32).to_be_bytes());
    resource.extend(&info);
    if info.len() % 2 == 1 {
        resource.push(0);
    }

    let mut b = b"8BPS".to_vec();
    b.extend(1u16.to_be_bytes());
    b.extend([0; 6]);
    b.extend(3u16.to_be_bytes()); // channels
    b.extend(1u32.to_be_bytes()); // height
    b.extend(1u32.to_be_bytes()); // width
    b.extend(8u16.to_be_bytes()); // depth
    b.extend(3u16.to_be_bytes()); // RGB
    b.extend(0u32.to_be_bytes()); // colour mode data
    b.extend((resource.len() as u32).to_be_bytes());
    b.extend(&resource);
    b.extend(0u32.to_be_bytes()); // layer and mask info
    b.extend(0u16.to_be_bytes()); // raw
    b.extend([255, 255, 255]);
    b
}

// --- the tests ----------------------------------------------------------------------------------

/// With no rules given, only what Unity gets wrong whatever the team's policy is reported: a
/// format it can't import, a colour profile it ignores, a PSD with no merged image.
#[test]
fn the_defaults_report_what_unity_gets_wrong() {
    let dir = TempDir::new();
    dir.write("clean.png", &rgb8(64, 64));
    dir.write("p3.png", &with_icc(display_p3()));
    dir.write("generated.webp", &webp(8, 8));
    #[cfg(feature = "psd")]
    dir.write("flat.psd", &psd_without_merged_image());
    let files: Vec<&str> = ["clean.png", "p3.png", "generated.webp"]
        .into_iter()
        .chain(cfg!(feature = "psd").then_some("flat.psd"))
        .collect();
    let mut args = vec!["check"];
    args.extend(&files);
    args.extend(["--results", "out/results.json"]);
    let output = fire_cli(dir.path(), &args);
    assert_eq!(code(&output), 1, "{}", stdout(&output));

    // Read back as Asset Check's results, which checks the schema and version.
    let results = Results::read(&dir.path().join("out/results.json")).unwrap();
    assert_eq!(results.tool.name, "fire-cli");
    assert_eq!(results.files.len(), files.len());
    let clean = entry(&results, "clean.png");
    assert_eq!(clean.kind.as_deref(), Some("texture"));
    assert_eq!(clean.profile.as_deref(), Some("default"));
    assert_eq!(clean.status, Status::Checked);
    assert!(clean.findings.is_empty());

    let p3 = &entry(&results, "p3.png").findings;
    assert_eq!(p3.len(), 1);
    assert_eq!(p3[0].rule, "texture.color_profile");
    assert_eq!(p3[0].severity, Severity::Warning);
    assert!(p3[0].what.contains("\"Display P3\""), "{}", p3[0].what);

    let webp = &entry(&results, "generated.webp").findings;
    assert_eq!(webp.len(), 1);
    assert_eq!(webp[0].rule, "texture.format");
    assert_eq!(webp[0].severity, Severity::Error);
    assert!(webp[0].why.contains("can't read"), "{}", webp[0].why);

    #[cfg(feature = "psd")]
    assert_eq!(rules_of(&results, "flat.psd"), ["texture.psd_composite"]);
}

/// A texture rules file turns rules on, sets their limits, and carries the technical artist's note.
#[test]
fn a_profile_turns_rules_on() {
    let dir = TempDir::new();
    dir.write("opaque.png", &opaque_rgba8(64, 64));
    dir.write("deep.png", &rgb16(16, 16));
    dir.write(
        "ai.png",
        &with_text(&rgb8(16, 16), "parameters", "a crate, steps: 20"),
    );
    dir.write(
        "prop.json",
        br#"{ "version": 1, "generator": "hand-written",
              "profile": { "name": "Prop texture", "rules": {
                "texture.max_size": { "severity": "error", "params": { "max": 32 },
                                      "note": "Quest budget for props." },
                "texture.unused_alpha": {},
                "texture.bit_depth": {},
                "texture.generation_metadata": { "severity": "error" }
              } } }"#,
    );
    let output = fire_cli(
        dir.path(),
        &[
            "check",
            "opaque.png",
            "deep.png",
            "ai.png",
            "--profile",
            "prop.json",
            "--results",
            "r.json",
        ],
    );
    assert_eq!(code(&output), 1);
    let results = Results::read(&dir.path().join("r.json")).unwrap();
    assert_eq!(
        rules_of(&results, "opaque.png"),
        ["texture.max_size", "texture.unused_alpha"]
    );
    assert_eq!(rules_of(&results, "deep.png"), ["texture.bit_depth"]);
    assert_eq!(
        rules_of(&results, "ai.png"),
        ["texture.generation_metadata"]
    );

    let opaque = entry(&results, "opaque.png");
    assert_eq!(opaque.profile_label.as_deref(), Some("Prop texture"));
    let max_size = &opaque.findings[0];
    assert_eq!(max_size.note.as_deref(), Some("Quest budget for props."));
    assert_eq!(
        max_size.what,
        "This image is 64×64. The most allowed is 32×32."
    );
    assert!(entry(&results, "ai.png").findings[0]
        .what
        .contains("\"parameters\""));

    // What the console says: the finding, then how to fix it.
    let out = stdout(&output);
    assert!(out.contains("error: This image is 64×64."), "{out}");
    assert!(out.contains("How: Scale it down"), "{out}");
}

/// 0 when nothing at warning level or worse was found, 1 when something was, 2 for a command line
/// that can't be followed, which also removes results an earlier run left.
#[test]
fn exit_codes_and_stale_results() {
    let dir = TempDir::new();
    dir.write("odd.png", &rgb8(30, 30));
    dir.write(
        "info.json",
        br#"{ "version": 1, "profile": { "rules": {
              "texture.power_of_two": { "severity": "info" } } } }"#,
    );
    let output = fire_cli(
        dir.path(),
        &[
            "check",
            "odd.png",
            "--profile",
            "info.json",
            "--results",
            "r.json",
        ],
    );
    assert_eq!(
        code(&output),
        0,
        "an info-level finding doesn't fail the run"
    );
    let results = Results::read(&dir.path().join("r.json")).unwrap();
    assert_eq!(rules_of(&results, "odd.png"), ["texture.power_of_two"]);

    let refused = fire_cli(
        dir.path(),
        &["check", "odd.png", "--bogus", "--results", "r.json"],
    );
    assert_eq!(code(&refused), 2);
    assert!(!dir.path().join("r.json").exists(), "stale results removed");

    let missing = fire_cli(dir.path(), &["check", "missing.png", "--results", "r.json"]);
    assert_eq!(code(&missing), 1, "a file that can't be read fails the run");
    let results = Results::read(&dir.path().join("r.json")).unwrap();
    assert_eq!(entry(&results, "missing.png").status, Status::Unreadable);

    assert_eq!(code(&fire_cli(dir.path(), &[])), 2);
    assert_eq!(code(&fire_cli(dir.path(), &["--version"])), 0);
    assert_eq!(code(&fire_cli(dir.path(), &["--help"])), 0);
}

#[test]
fn files_from_lists_the_images() {
    let dir = TempDir::new();
    dir.write("a.png", &rgb8(8, 8));
    dir.write("sub/b.png", &rgb8(8, 8));
    dir.write("list.txt", b"a.png\n\nsub/b.png\n");
    let output = fire_cli(
        dir.path(),
        &["check", "--files-from", "list.txt", "--results", "r.json"],
    );
    assert_eq!(code(&output), 0, "{}", stdout(&output));
    let results = Results::read(&dir.path().join("r.json")).unwrap();
    let paths: Vec<&str> = results.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a.png", "sub/b.png"]);
}

#[test]
fn info_says_what_the_decoder_sees() {
    let dir = TempDir::new();
    dir.write("p3.png", &with_icc(display_p3()));
    let output = fire_cli(dir.path(), &["info", "p3.png"]);
    assert_eq!(code(&output), 0);
    let out = stdout(&output);
    assert!(out.contains("PNG"), "{out}");
    assert!(out.contains("16×16"), "{out}");
    assert!(out.contains("\"Display P3\" (not sRGB)"), "{out}");
    assert_eq!(code(&fire_cli(dir.path(), &["info", "missing.png"])), 1);
}

// --- --staged, with rules.json --------------------------------------------------------------------

/// Runs git in `dir`, failing the test if it fails.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", dir.join("no-global-config"))
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A project on `main` with its rules in `rules/`: textures go to fire-cli, models to
/// 3d-review-cli; `T_*_N` is a normal map, `T_*` any other texture; `main` is production.
fn project() -> TempDir {
    let dir = TempDir::new();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    dir.write(
        "rules/rules.json",
        br#"{
          "version": 1, "mode": "advise",
          "stages": ["prototype", "production"], "stage": "prototype",
          "branches": [{ "pattern": "main", "stage": "production" }],
          "requires": { "fire-cli": "0.1.0" },
          "types": {
            "texture": { "tool": "fire-cli", "extensions": ["png", "tga", "psd"] },
            "model": { "tool": "3d-review-cli", "extensions": ["fbx"] }
          },
          "profiles": [
            { "id": "normal-map", "type": "texture", "label": "Normal map",
              "names": ["T_*_N"], "rules": { "*": "textures/normal.json" } },
            { "id": "texture", "type": "texture", "label": "Texture",
              "names": ["T_*"], "rules": { "*": "textures/texture.json" } }
          ]
        }"#,
    );
    dir.write(
        "rules/textures/normal.json",
        br#"{ "version": 1, "profile": { "name": "Normal map", "rules": {
              "texture.unused_alpha": { "severity": "error" } } } }"#,
    );
    dir.write(
        "rules/textures/texture.json",
        br#"{ "version": 1, "profile": { "name": "Texture", "rules": {
              "texture.max_size": { "params": { "max": 100 } } } } }"#,
    );
    dir.write("README.md", b"a project");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "--no-verify", "-m", "rules"]);
    dir
}

/// The staged images are resolved by asset-rules, checked as staged, and cached: the second run
/// reads every file from the cache and gives the same results.
#[test]
fn staged_images_are_checked_as_staged_and_cached() {
    let dir = project();
    dir.write("Assets/T_Crate_N.png", &opaque_rgba8(32, 32));
    dir.write("Assets/T_Crate_D.png", &rgb8(64, 64));
    dir.write("Assets/ComfyUI_00042_.png", &rgb8(8, 8));
    dir.write("Assets/SM_Rock.fbx", b"a model");
    dir.write("Assets/notes.txt", b"not an asset");
    git(dir.path(), &["add", "Assets"]);
    // Changed after staging, past texture.json's limit: the staged 64×64 is what must be checked.
    dir.write("Assets/T_Crate_D.png", &rgb8(400, 400));

    let args = [
        "check",
        "--staged",
        "--rules",
        "rules/rules.json",
        "--results",
        ".git/asset-check/results-fire-cli.json",
    ];
    let first = fire_cli(dir.path(), &args);
    assert_eq!(code(&first), 1, "{}", stdout(&first));
    let results_path = dir.path().join(".git/asset-check/results-fire-cli.json");
    let results = Results::read(&results_path).unwrap();

    let paths: Vec<&str> = results.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "Assets/ComfyUI_00042_.png",
            "Assets/T_Crate_D.png",
            "Assets/T_Crate_N.png"
        ],
        "only this tool's files: not the model, not the text file"
    );
    let normal = entry(&results, "Assets/T_Crate_N.png");
    assert_eq!(normal.profile.as_deref(), Some("normal-map"));
    assert_eq!(normal.stage.as_deref(), Some("production"));
    assert_eq!(normal.stage_from, Some(StageFrom::Branch));
    assert_eq!(
        rules_of(&results, "Assets/T_Crate_N.png"),
        ["texture.unused_alpha"]
    );
    assert_eq!(
        entry(&results, "Assets/T_Crate_D.png").profile.as_deref(),
        Some("texture")
    );
    assert!(
        rules_of(&results, "Assets/T_Crate_D.png").is_empty(),
        "the staged content was checked, not the working file"
    );
    assert_eq!(
        rules_of(&results, "Assets/ComfyUI_00042_.png"),
        ["file.no_profile"]
    );
    assert_eq!(results.context.branch.as_deref(), Some("main"));

    let second = fire_cli(dir.path(), &args);
    assert_eq!(code(&second), 1);
    let cached = stdout(&second).matches("from the cache").count();
    assert_eq!(cached, 2, "{}", stdout(&second));
    let again = Results::read(&results_path).unwrap();
    assert_eq!(again.files, results.files);
}

#[test]
fn rules_check_and_explain() {
    let dir = project();
    let check = fire_cli(dir.path(), &["rules", "check", "rules/rules.json"]);
    assert_eq!(code(&check), 0, "{}", stdout(&check));
    assert!(stdout(&check).contains("no problems"));

    let explain = fire_cli(
        dir.path(),
        &[
            "rules",
            "explain",
            "Assets/T_Wall_N.png",
            "--rules",
            "rules/rules.json",
        ],
    );
    assert_eq!(code(&explain), 0);
    assert!(
        stdout(&explain).contains("normal-map"),
        "{}",
        stdout(&explain)
    );

    dir.write("bad.json", b"{ \"version\": 1 }");
    assert_eq!(
        code(&fire_cli(dir.path(), &["rules", "check", "bad.json"])),
        1
    );
}
