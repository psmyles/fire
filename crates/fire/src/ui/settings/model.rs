//! The settings dialog's model — pure logic, no Win32 (unit-tested).
//!
//! Two halves:
//!
//! * **Field accessors.** A widget knows which [`Config`] field it edits ([`BoolField`],
//!   [`ChoiceField`], [`NumField`], [`TextField`]) and reads/writes it through here, so the dialog's
//!   paint/hit code never touches config internals and adding a setting is one enum variant plus one
//!   `get`/`set` arm.
//! * **Open-with tree edits.** [`flatten`] turns the nested `[[open-with]]` tree into the flat,
//!   indented row list the Context Menu tab paints, and the edit operations (add / remove / move /
//!   indent / outdent) work on the `Vec<MenuEntry>` by *path* (the index chain from the root).

use crate::config::{BackgroundCfg, Config, FitCfg, InstanceMode, MenuEntry, TonemapCfg};
use crate::flipbook::{FPS_MAX, FPS_MIN};

// ---------------------------------------------------------------------------------------------
// Field accessors
// ---------------------------------------------------------------------------------------------

/// A checkbox's target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoolField {
    HotReload,
    FitUpscale,
    FlipbookBlend,
    FlipbookAutoplay,
    FlipbookAutoDetect,
    OctagonRemember,
    CtxShowInExplorer,
    CtxCopyFile,
    CtxCopyPath,
    CtxCopyFileName,
}

impl BoolField {
    pub(crate) fn get(self, c: &Config) -> bool {
        match self {
            BoolField::HotReload => c.hot_reload,
            BoolField::FitUpscale => c.fit_upscale,
            BoolField::FlipbookBlend => c.flipbook.blend,
            BoolField::FlipbookAutoplay => c.flipbook.autoplay,
            BoolField::FlipbookAutoDetect => c.flipbook.auto_detect,
            BoolField::OctagonRemember => c.octagon.remember,
            BoolField::CtxShowInExplorer => c.context_menu.show_in_explorer,
            BoolField::CtxCopyFile => c.context_menu.copy_file,
            BoolField::CtxCopyPath => c.context_menu.copy_path,
            BoolField::CtxCopyFileName => c.context_menu.copy_file_name,
        }
    }

    pub(crate) fn set(self, c: &mut Config, v: bool) {
        match self {
            BoolField::HotReload => c.hot_reload = v,
            BoolField::FitUpscale => c.fit_upscale = v,
            BoolField::FlipbookBlend => c.flipbook.blend = v,
            BoolField::FlipbookAutoplay => c.flipbook.autoplay = v,
            BoolField::FlipbookAutoDetect => c.flipbook.auto_detect = v,
            BoolField::OctagonRemember => c.octagon.remember = v,
            BoolField::CtxShowInExplorer => c.context_menu.show_in_explorer = v,
            BoolField::CtxCopyFile => c.context_menu.copy_file = v,
            BoolField::CtxCopyPath => c.context_menu.copy_path = v,
            BoolField::CtxCopyFileName => c.context_menu.copy_file_name = v,
        }
    }
}

/// A dropdown's target. The options are positional: index N of [`Self::options`] is the value
/// [`Self::set`] writes for N, so the two must be kept in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChoiceField {
    InstanceMode,
    Background,
    DefaultTonemap,
    DefaultFit,
}

impl ChoiceField {
    pub(crate) fn options(self) -> &'static [&'static str] {
        match self {
            ChoiceField::InstanceMode => &["New window", "Single instance"],
            ChoiceField::Background => &["Automatic", "Black", "White", "Grey", "Checkerboard"],
            ChoiceField::DefaultTonemap => &["Reinhard", "ACES"],
            ChoiceField::DefaultFit => &["Fit to window", "Actual size (1:1)"],
        }
    }

    pub(crate) fn get(self, c: &Config) -> usize {
        match self {
            ChoiceField::InstanceMode => match c.instance_mode {
                InstanceMode::NewWindow => 0,
                InstanceMode::SingleInstance => 1,
            },
            ChoiceField::Background => match c.background {
                BackgroundCfg::Auto => 0,
                BackgroundCfg::Black => 1,
                BackgroundCfg::White => 2,
                BackgroundCfg::Grey => 3,
                BackgroundCfg::Checker => 4,
            },
            ChoiceField::DefaultTonemap => match c.default_tonemap {
                TonemapCfg::Reinhard => 0,
                TonemapCfg::Aces => 1,
            },
            ChoiceField::DefaultFit => match c.default_fit {
                FitCfg::Fit => 0,
                FitCfg::ActualSize => 1,
            },
        }
    }

    /// Out-of-range indices are ignored rather than clamped — they can only come from a bug.
    pub(crate) fn set(self, c: &mut Config, i: usize) {
        match (self, i) {
            (ChoiceField::InstanceMode, 0) => c.instance_mode = InstanceMode::NewWindow,
            (ChoiceField::InstanceMode, 1) => c.instance_mode = InstanceMode::SingleInstance,
            (ChoiceField::Background, 0) => c.background = BackgroundCfg::Auto,
            (ChoiceField::Background, 1) => c.background = BackgroundCfg::Black,
            (ChoiceField::Background, 2) => c.background = BackgroundCfg::White,
            (ChoiceField::Background, 3) => c.background = BackgroundCfg::Grey,
            (ChoiceField::Background, 4) => c.background = BackgroundCfg::Checker,
            (ChoiceField::DefaultTonemap, 0) => c.default_tonemap = TonemapCfg::Reinhard,
            (ChoiceField::DefaultTonemap, 1) => c.default_tonemap = TonemapCfg::Aces,
            (ChoiceField::DefaultFit, 0) => c.default_fit = FitCfg::Fit,
            (ChoiceField::DefaultFit, 1) => c.default_fit = FitCfg::ActualSize,
            _ => {}
        }
    }
}

/// A numeric stepper's target, with its range/step/precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumField {
    ZoomStep,
    ExposureStep,
    ZoomSnap,
    FlipbookFps,
}

impl NumField {
    /// `(min, max, step, decimals)`. The range mirrors [`Config::sanitize`], which is still the
    /// authority — the stepper just can't leave it in the first place.
    pub(crate) fn spec(self) -> (f32, f32, f32, usize) {
        match self {
            NumField::ZoomStep => (1.01, 4.0, 0.05, 2),
            NumField::ExposureStep => (0.01, 4.0, 0.05, 2),
            // Whole drag pixels — a tenth of one is not a feel anybody can dial in.
            NumField::ZoomSnap => (0.0, 100.0, 1.0, 0),
            NumField::FlipbookFps => (FPS_MIN, FPS_MAX, 1.0, 1),
        }
    }

    pub(crate) fn get(self, c: &Config) -> f32 {
        match self {
            NumField::ZoomStep => c.zoom_step,
            NumField::ExposureStep => c.exposure_step,
            NumField::ZoomSnap => c.zoom_snap,
            NumField::FlipbookFps => c.flipbook.fps,
        }
    }

    /// Clamped to the field's range on the way in.
    pub(crate) fn set(self, c: &mut Config, v: f32) {
        let (min, max, _, _) = self.spec();
        let v = if v.is_finite() {
            v.clamp(min, max)
        } else {
            min
        };
        match self {
            NumField::ZoomStep => c.zoom_step = v,
            NumField::ExposureStep => c.exposure_step = v,
            NumField::ZoomSnap => c.zoom_snap = v,
            NumField::FlipbookFps => c.flipbook.fps = v,
        }
    }
}

/// A text box's target: one field of the *selected* open-with entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextField {
    Name,
    Program,
    /// The `args` vector, edited as one command-line-ish string (see [`split_args`]).
    Args,
}

impl TextField {
    pub(crate) fn get(self, entry: &MenuEntry) -> String {
        match self {
            TextField::Name => entry.name.clone(),
            TextField::Program => entry.path.clone().unwrap_or_default(),
            TextField::Args => join_args(&entry.args),
        }
    }

    pub(crate) fn set(self, entry: &mut MenuEntry, v: &str) {
        match self {
            TextField::Name => entry.name = v.to_string(),
            // An empty path is "not set" — an entry with neither a path nor children is skipped when
            // the menu is built, so a half-filled row simply doesn't appear yet.
            TextField::Program => {
                entry.path = (!v.trim().is_empty()).then(|| v.to_string());
            }
            TextField::Args => entry.args = split_args(v),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            TextField::Name => "Name",
            TextField::Program => "Program",
            TextField::Args => "Arguments",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Open-with tree
// ---------------------------------------------------------------------------------------------

/// One row of the flattened open-with tree, as the Context Menu tab lists it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TreeRow {
    /// Index chain from the root (`[1, 0]` = second top-level entry's first child).
    pub(crate) path: Vec<usize>,
    pub(crate) depth: usize,
    pub(crate) name: String,
    pub(crate) submenu: bool,
}

/// Pre-order walk of the tree into indented rows.
pub(crate) fn flatten(entries: &[MenuEntry]) -> Vec<TreeRow> {
    fn walk(entries: &[MenuEntry], prefix: &mut Vec<usize>, out: &mut Vec<TreeRow>) {
        for (i, e) in entries.iter().enumerate() {
            prefix.push(i);
            out.push(TreeRow {
                path: prefix.clone(),
                depth: prefix.len() - 1,
                name: e.name.clone(),
                submenu: e.is_submenu(),
            });
            walk(&e.items, prefix, out);
            prefix.pop();
        }
    }
    let mut out = Vec::new();
    walk(entries, &mut Vec::new(), &mut out);
    out
}

/// The `Vec` that *holds* the entry at `path` (its parent's `items`, or the root list). `None` for
/// an empty or dangling path.
fn container<'a>(root: &'a mut Vec<MenuEntry>, path: &[usize]) -> Option<&'a mut Vec<MenuEntry>> {
    let (_, parents) = path.split_last()?;
    let mut cur = root;
    for &i in parents {
        cur = &mut cur.get_mut(i)?.items;
    }
    Some(cur)
}

/// The entry at `path`.
pub(crate) fn entry_at<'a>(
    root: &'a mut Vec<MenuEntry>,
    path: &[usize],
) -> Option<&'a mut MenuEntry> {
    let last = *path.last()?;
    container(root, path)?.get_mut(last)
}

/// A fresh leaf entry (the "Add item" button's payload). Its empty path keeps it out of the real
/// menu until the user points it at a program.
pub(crate) fn new_item() -> MenuEntry {
    MenuEntry {
        name: "New item".into(),
        path: None,
        args: Vec::new(),
        items: Vec::new(),
    }
}

/// A fresh submenu holding one new leaf — a submenu with no children is not a submenu at all (see
/// [`MenuEntry`]), so it ships with one.
pub(crate) fn new_submenu() -> MenuEntry {
    MenuEntry {
        name: "New submenu".into(),
        path: None,
        args: Vec::new(),
        items: vec![new_item()],
    }
}

/// Insert `entry` directly after `path` (or at the end of the root list when `path` is `None`).
/// Returns the new entry's path.
pub(crate) fn insert_after(
    root: &mut Vec<MenuEntry>,
    path: Option<&[usize]>,
    entry: MenuEntry,
) -> Vec<usize> {
    match path {
        Some(p) if !p.is_empty() => {
            let idx = *p.last().unwrap();
            if let Some(c) = container(root, p) {
                let at = (idx + 1).min(c.len());
                c.insert(at, entry);
                let mut new = p.to_vec();
                *new.last_mut().unwrap() = at;
                return new;
            }
            root.push(entry);
            vec![root.len() - 1]
        }
        _ => {
            root.push(entry);
            vec![root.len() - 1]
        }
    }
}

/// Remove the entry at `path` (and its children). Returns the path that should be selected
/// afterwards: the next sibling, else the previous, else the parent, else nothing.
pub(crate) fn remove_at(root: &mut Vec<MenuEntry>, path: &[usize]) -> Option<Vec<usize>> {
    let idx = *path.last()?;
    let c = container(root, path)?;
    if idx >= c.len() {
        return None;
    }
    c.remove(idx);
    let remaining = c.len();
    if remaining == 0 {
        // The container is now empty: fall back to the parent (which just stopped being a submenu),
        // or to no selection at the root.
        let parent = &path[..path.len() - 1];
        return (!parent.is_empty()).then(|| parent.to_vec());
    }
    let mut sel = path.to_vec();
    *sel.last_mut().unwrap() = idx.min(remaining - 1);
    Some(sel)
}

/// Move the entry at `path` up (`-1`) or down (`+1`) among its siblings. Returns its new path, or
/// `None` if it is already at that end.
pub(crate) fn move_sibling(
    root: &mut Vec<MenuEntry>,
    path: &[usize],
    delta: isize,
) -> Option<Vec<usize>> {
    let idx = *path.last()?;
    let c = container(root, path)?;
    let target = idx.checked_add_signed(delta)?;
    if target >= c.len() {
        return None;
    }
    c.swap(idx, target);
    let mut new = path.to_vec();
    *new.last_mut().unwrap() = target;
    Some(new)
}

/// Nest the entry at `path` into its previous sibling, which becomes (or stays) a submenu. Returns
/// the moved entry's new path, or `None` when it is the first among its siblings.
///
/// A *leaf* previous-sibling turns into a submenu, which means its own `path`/`args` stop being used
/// (submenu wins — see [`MenuEntry`]). They are kept in the struct, so outdenting the child again
/// restores it to a working leaf: no data is destroyed by the round trip.
pub(crate) fn indent(root: &mut Vec<MenuEntry>, path: &[usize]) -> Option<Vec<usize>> {
    let idx = *path.last()?;
    if idx == 0 {
        return None;
    }
    let c = container(root, path)?;
    if idx >= c.len() {
        return None;
    }
    let entry = c.remove(idx);
    let prev = c.get_mut(idx - 1)?;
    prev.items.push(entry);
    let child = prev.items.len() - 1;
    let mut new = path.to_vec();
    *new.last_mut().unwrap() = idx - 1;
    new.push(child);
    Some(new)
}

/// Lift the entry at `path` out of its submenu, placing it directly after its former parent.
/// Returns its new path, or `None` when it is already at the top level.
pub(crate) fn outdent(root: &mut Vec<MenuEntry>, path: &[usize]) -> Option<Vec<usize>> {
    if path.len() < 2 {
        return None;
    }
    let idx = *path.last()?;
    let c = container(root, path)?;
    if idx >= c.len() {
        return None;
    }
    let entry = c.remove(idx);
    let parent_path = &path[..path.len() - 1];
    Some(insert_after(root, Some(parent_path), entry))
}

/// Split a typed argument string into argv elements, honoring double quotes so a path with spaces
/// survives (`"{path}" -flag` → `["{path}", "-flag"]`). Quotes are removed; there is no escaping.
pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true; // `""` is a deliberate empty argument
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Read a typed zoom-snap list ("50, 100, 150" — commas and/or spaces, a trailing `%` tolerated)
/// into percentages. Junk tokens are dropped rather than rejected: the box is edited a keystroke at
/// a time, so a half-typed entry must cost the user that entry and nothing else.
///
/// Range and ordering are [`Config::sanitize`]'s job, as they are for every other field here.
pub(crate) fn parse_snap_levels(s: &str) -> Vec<f32> {
    s.split([',', ';', ' ', '\t'])
        .filter_map(|t| t.trim().trim_end_matches('%').parse::<f32>().ok())
        .collect()
}

/// The inverse of [`parse_snap_levels`]. Whole percentages print without a decimal point — the
/// levels are round numbers, and "50.0, 100.0" reads like a precision nobody asked for.
pub(crate) fn format_snap_levels(levels: &[f32]) -> String {
    levels
        .iter()
        .map(|p| {
            if p.fract() == 0.0 {
                format!("{p:.0}")
            } else {
                format!("{p}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The inverse of [`split_args`]: re-quote any element containing whitespace.
pub(crate) fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.chars().any(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(name: &str) -> MenuEntry {
        MenuEntry {
            name: name.into(),
            path: Some(format!(r"C:\{name}.exe")),
            args: Vec::new(),
            items: Vec::new(),
        }
    }

    /// `a`, `b` (with children `b1`, `b2`), `c`.
    fn tree() -> Vec<MenuEntry> {
        vec![
            leaf("a"),
            MenuEntry {
                name: "b".into(),
                path: None,
                args: Vec::new(),
                items: vec![leaf("b1"), leaf("b2")],
            },
            leaf("c"),
        ]
    }

    fn names(root: &[MenuEntry]) -> Vec<(usize, String)> {
        flatten(root)
            .into_iter()
            .map(|r| (r.depth, r.name))
            .collect()
    }

    #[test]
    fn flatten_is_pre_order_with_depth() {
        let rows = flatten(&tree());
        assert_eq!(
            rows.iter()
                .map(|r| (r.path.clone(), r.depth, r.name.as_str(), r.submenu))
                .collect::<Vec<_>>(),
            vec![
                (vec![0], 0, "a", false),
                (vec![1], 0, "b", true),
                (vec![1, 0], 1, "b1", false),
                (vec![1, 1], 1, "b2", false),
                (vec![2], 0, "c", false),
            ]
        );
    }

    #[test]
    fn insert_after_lands_next_to_the_selection() {
        let mut t = tree();
        let p = insert_after(&mut t, Some(&[1, 0]), new_item());
        assert_eq!(p, vec![1, 1]);
        assert_eq!(
            names(&t),
            [
                (0, "a"),
                (0, "b"),
                (1, "b1"),
                (1, "New item"),
                (1, "b2"),
                (0, "c")
            ]
            .map(|(d, n)| (d, n.to_string()))
        );
        // No selection → appended at the root.
        let mut t = tree();
        let p = insert_after(&mut t, None, new_item());
        assert_eq!(p, vec![3]);
    }

    #[test]
    fn remove_picks_a_sensible_next_selection() {
        // Next sibling.
        let mut t = tree();
        assert_eq!(remove_at(&mut t, &[1, 0]), Some(vec![1, 0]));
        assert_eq!(entry_at(&mut t, &[1, 0]).unwrap().name, "b2");
        // Last of its siblings → the previous one.
        let mut t = tree();
        assert_eq!(remove_at(&mut t, &[2]), Some(vec![1]));
        // Only child → the (now childless) parent.
        let mut t = tree();
        remove_at(&mut t, &[1, 0]);
        assert_eq!(remove_at(&mut t, &[1, 0]), Some(vec![1]));
        assert!(!entry_at(&mut t, &[1]).unwrap().is_submenu());
        // Last entry at the root → nothing selected.
        let mut t = vec![leaf("only")];
        assert_eq!(remove_at(&mut t, &[0]), None);
        assert!(t.is_empty());
    }

    #[test]
    fn move_swaps_within_siblings_and_stops_at_the_ends() {
        let mut t = tree();
        assert_eq!(move_sibling(&mut t, &[2], -1), Some(vec![1]));
        assert_eq!(names(&t)[0], (0, "a".to_string()));
        assert_eq!(names(&t)[1], (0, "c".to_string()));
        // Already first / already last.
        assert_eq!(move_sibling(&mut t, &[0], -1), None);
        assert_eq!(move_sibling(&mut t, &[2], 1), None);
    }

    /// Indenting a leaf onto a leaf turns the target into a submenu; outdenting again restores the
    /// original tree exactly — no field is destroyed by the round trip.
    #[test]
    fn indent_outdent_round_trips() {
        let original = tree();
        let mut t = original.clone();
        // "c" (index 2) nests into "b" (index 1), which is already a submenu.
        let p = indent(&mut t, &[2]).unwrap();
        assert_eq!(p, vec![1, 2]);
        assert_eq!(outdent(&mut t, &p), Some(vec![2]));
        assert_eq!(t, original);

        // Now onto a *leaf*: "b" nests into "a", which becomes a submenu.
        let mut t = original.clone();
        let p = indent(&mut t, &[1]).unwrap();
        assert_eq!(p, vec![0, 0]);
        assert!(entry_at(&mut t, &[0]).unwrap().is_submenu());
        // "a" kept its own path, so lifting "b" back out restores it as a working leaf.
        assert_eq!(outdent(&mut t, &p), Some(vec![1]));
        assert_eq!(t, original);

        // The first entry among its siblings has nothing to nest into; the top level has nothing to
        // rise to.
        let mut t = original.clone();
        assert_eq!(indent(&mut t, &[0]), None);
        assert_eq!(outdent(&mut t, &[0]), None);
        assert_eq!(t, original);
    }

    #[test]
    fn args_round_trip_through_the_text_box() {
        let args = vec![
            "{path}".to_string(),
            r"C:\out dir\x.jpg".to_string(),
            "-q".to_string(),
        ];
        let text = join_args(&args);
        assert_eq!(text, r#"{path} "C:\out dir\x.jpg" -q"#);
        assert_eq!(split_args(&text), args);
        // Extra whitespace collapses; an empty string is no arguments at all.
        assert_eq!(split_args("  a   b  "), vec!["a", "b"]);
        assert!(split_args("   ").is_empty());
    }

    /// The snap-level box is typed into a keystroke at a time, so parsing has to survive every
    /// half-finished state — and round-trip whatever survives.
    #[test]
    fn snap_levels_round_trip_and_ignore_junk() {
        assert_eq!(parse_snap_levels("50, 100, 150"), vec![50.0, 100.0, 150.0]);
        // Separators are interchangeable, a trailing % is tolerated, and fractions survive.
        assert_eq!(parse_snap_levels("50% 100;12.5"), vec![50.0, 100.0, 12.5]);
        // A word, a lone separator or a bare "%" is dropped; the rest of the list still parses.
        assert_eq!(parse_snap_levels("50, , x, %, 100"), vec![50.0, 100.0]);
        assert!(parse_snap_levels("").is_empty());

        let levels = vec![12.5, 50.0, 100.0, 6400.0];
        let text = format_snap_levels(&levels);
        assert_eq!(text, "12.5, 50, 100, 6400");
        assert_eq!(parse_snap_levels(&text), levels);
    }

    /// A numeric field's range **is** `Config::sanitize`'s clamp. The settings window's sliders are
    /// bounded by [`NumField::spec`], so if the two ever drift, a value the user set would be
    /// silently moved on the way to disk — the setting would just not take.
    #[test]
    fn num_fields_cannot_leave_the_sanitized_range() {
        for f in [
            NumField::ZoomStep,
            NumField::ExposureStep,
            NumField::ZoomSnap,
            NumField::FlipbookFps,
        ] {
            let (min, max, _, _) = f.spec();
            // Ctrl+click on an ImGui slider lets you *type* a value, so `set` has to survive
            // nonsense, not just the ends of the track.
            for probe in [min - 1.0, min, max, max + 1.0, f32::MIN, f32::MAX, f32::NAN] {
                let mut c = Config::default();
                f.set(&mut c, probe);
                let got = f.get(&c);
                assert!((min..=max).contains(&got), "{f:?}: set({probe}) → {got}");

                let mut sanitized = c.clone();
                sanitized.sanitize();
                assert_eq!(sanitized, c, "{f:?}: the range must match Config::sanitize");
            }
        }
    }

    /// Every dropdown's option list and its `set` arms agree, and `get` inverts `set`.
    #[test]
    fn dropdown_indices_round_trip() {
        let fields = [
            ChoiceField::InstanceMode,
            ChoiceField::Background,
            ChoiceField::DefaultTonemap,
            ChoiceField::DefaultFit,
        ];
        for f in fields {
            for i in 0..f.options().len() {
                let mut c = Config::default();
                f.set(&mut c, i);
                assert_eq!(f.get(&c), i, "{f:?} option {i}");
            }
        }
    }
}
