//! Keyboard bindings — pure logic, no Win32 (unit-tested like [`crate::folder`] and
//! [`crate::render::view`]).
//!
//! Every keyboard command the viewer has is a [`KeyAction`]; a [`Keybinds`] table maps chords to
//! them. The win shell builds a [`KeyChord`] from the `WM_KEYDOWN` virtual-key plus the live
//! modifier state, looks it up here, and dispatches — so *what* a key does lives in one table
//! instead of a `match` on hex VK codes, which is what makes the settings dialog's rebind editor
//! possible. It is also where the toolbar's tooltips get their "(F)" suffixes ([`Keybinds::labels`]),
//! so a rebound key relabels the button it belongs to.
//!
//! **Chords match exactly.** `Left` and `Ctrl+Left` are different bindings, so a modifier held by
//! accident no longer triggers the plain command (and, conversely, `Ctrl+…` chords are bindable).
//! The one concession is `Shift+=`, bound alongside `=` by default, because that is how you type
//! `+` on most layouts.
//!
//! Chords round-trip through `config.toml` as strings (`"F"`, `"Ctrl+Shift+K"`, `"Num+"`) — see
//! [`Keybinds::from_config`] / [`Keybinds::to_config`]. Only bindings that *differ* from the
//! defaults are written, so a user who never rebinds anything keeps an empty `[keybinds]` table and
//! inherits future default changes.

use crate::config::{KeyValue, KeybindsCfg};

/// A rebindable keyboard command. The order here is the order the settings dialog lists them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    // File
    OpenFile,
    CloseImage,
    // View
    Fit,
    ActualSize,
    ZoomIn,
    ZoomOut,
    // Channel isolation
    ChannelRgb,
    ChannelR,
    ChannelG,
    ChannelB,
    ChannelA,
    // HDR
    ToggleTonemap,
    ExposureUp,
    ExposureDown,
    ExposureReset,
    // Appearance
    ToggleOutline,
    /// Walk the four backdrops (black → white → grey → checker → black).
    CycleBackdrop,
    // Navigation
    PrevImage,
    NextImage,
    // Window
    ToggleFullscreen,
    /// Esc: leave full-screen if in it, otherwise close the window — the closing half only when
    /// the `esc-closes-window` config key is on (Settings ▸ General).
    CloseOrExitFullscreen,
    // Flipbook
    ToggleFlipbook,
    FlipbookPlayPause,
    FlipbookPrevFrame,
    FlipbookNextFrame,
}

/// Every action, in dialog/list order. The single source of truth for "what is bindable".
pub const ALL_ACTIONS: &[KeyAction] = &[
    KeyAction::OpenFile,
    KeyAction::CloseImage,
    KeyAction::Fit,
    KeyAction::ActualSize,
    KeyAction::ZoomIn,
    KeyAction::ZoomOut,
    KeyAction::ChannelRgb,
    KeyAction::ChannelR,
    KeyAction::ChannelG,
    KeyAction::ChannelB,
    KeyAction::ChannelA,
    KeyAction::ToggleTonemap,
    KeyAction::ExposureUp,
    KeyAction::ExposureDown,
    KeyAction::ExposureReset,
    KeyAction::ToggleOutline,
    KeyAction::CycleBackdrop,
    KeyAction::PrevImage,
    KeyAction::NextImage,
    KeyAction::ToggleFullscreen,
    KeyAction::CloseOrExitFullscreen,
    KeyAction::ToggleFlipbook,
    KeyAction::FlipbookPlayPause,
    KeyAction::FlipbookPrevFrame,
    KeyAction::FlipbookNextFrame,
];

impl KeyAction {
    /// The `[keybinds]` TOML key for this action (stable — renaming one silently unbinds it for
    /// every user, so don't).
    pub fn name(self) -> &'static str {
        match self {
            KeyAction::OpenFile => "open-file",
            KeyAction::CloseImage => "close-image",
            KeyAction::Fit => "fit",
            KeyAction::ActualSize => "actual-size",
            KeyAction::ZoomIn => "zoom-in",
            KeyAction::ZoomOut => "zoom-out",
            KeyAction::ChannelRgb => "all-channels",
            KeyAction::ChannelR => "red-channel",
            KeyAction::ChannelG => "green-channel",
            KeyAction::ChannelB => "blue-channel",
            KeyAction::ChannelA => "alpha-channel",
            KeyAction::ToggleTonemap => "toggle-tonemap",
            KeyAction::ExposureUp => "exposure-up",
            KeyAction::ExposureDown => "exposure-down",
            KeyAction::ExposureReset => "exposure-reset",
            KeyAction::ToggleOutline => "toggle-outline",
            KeyAction::CycleBackdrop => "cycle-backdrop",
            KeyAction::PrevImage => "previous-image",
            KeyAction::NextImage => "next-image",
            KeyAction::ToggleFullscreen => "toggle-fullscreen",
            KeyAction::CloseOrExitFullscreen => "close-or-exit-fullscreen",
            KeyAction::ToggleFlipbook => "toggle-flipbook",
            KeyAction::FlipbookPlayPause => "flipbook-play-pause",
            KeyAction::FlipbookPrevFrame => "flipbook-previous-frame",
            KeyAction::FlipbookNextFrame => "flipbook-next-frame",
        }
    }

    /// Human label for the settings list.
    pub fn label(self) -> &'static str {
        match self {
            KeyAction::OpenFile => "Open image\u{2026}",
            KeyAction::CloseImage => "Close image",
            KeyAction::Fit => "Fit to window",
            KeyAction::ActualSize => "Actual size (1:1)",
            KeyAction::ZoomIn => "Zoom in",
            KeyAction::ZoomOut => "Zoom out",
            KeyAction::ChannelRgb => "All channels",
            KeyAction::ChannelR => "Red channel",
            KeyAction::ChannelG => "Green channel",
            KeyAction::ChannelB => "Blue channel",
            KeyAction::ChannelA => "Alpha channel",
            KeyAction::ToggleTonemap => "Tone map: Reinhard \u{2194} ACES",
            KeyAction::ExposureUp => "Increase exposure",
            KeyAction::ExposureDown => "Decrease exposure",
            KeyAction::ExposureReset => "Reset exposure",
            KeyAction::ToggleOutline => "Image boundary outline",
            KeyAction::CycleBackdrop => "Next backdrop",
            KeyAction::PrevImage => "Previous image",
            KeyAction::NextImage => "Next image",
            KeyAction::ToggleFullscreen => "Full screen",
            KeyAction::CloseOrExitFullscreen => "Close window / leave full screen",
            KeyAction::ToggleFlipbook => "Flipbook mode",
            KeyAction::FlipbookPlayPause => "Play / pause",
            KeyAction::FlipbookPrevFrame => "Previous frame",
            KeyAction::FlipbookNextFrame => "Next frame",
        }
    }

    /// The settings list's group heading for this action.
    pub fn group(self) -> &'static str {
        match self {
            KeyAction::OpenFile | KeyAction::CloseImage => "File",
            KeyAction::Fit | KeyAction::ActualSize | KeyAction::ZoomIn | KeyAction::ZoomOut => {
                "View"
            }
            KeyAction::ChannelRgb
            | KeyAction::ChannelR
            | KeyAction::ChannelG
            | KeyAction::ChannelB
            | KeyAction::ChannelA => "Channels",
            KeyAction::ToggleTonemap
            | KeyAction::ExposureUp
            | KeyAction::ExposureDown
            | KeyAction::ExposureReset => "HDR",
            KeyAction::ToggleOutline | KeyAction::CycleBackdrop => "Appearance",
            KeyAction::PrevImage | KeyAction::NextImage => "Navigation",
            KeyAction::ToggleFullscreen | KeyAction::CloseOrExitFullscreen => "Window",
            KeyAction::ToggleFlipbook
            | KeyAction::FlipbookPlayPause
            | KeyAction::FlipbookPrevFrame
            | KeyAction::FlipbookNextFrame => "Flipbook",
        }
    }

    /// Whether this action only fires while flipbook mode is active. Such bindings are *inert*
    /// outside the mode (so `Space` does nothing over a still image), but they still take part in
    /// conflict detection — one flat namespace is far easier to reason about than a modal one.
    pub fn is_flipbook_context(self) -> bool {
        matches!(
            self,
            KeyAction::FlipbookPlayPause
                | KeyAction::FlipbookPrevFrame
                | KeyAction::FlipbookNextFrame
        )
    }

    fn from_name(s: &str) -> Option<Self> {
        ALL_ACTIONS.iter().copied().find(|a| a.name() == s)
    }
}

/// One key press: a virtual-key code plus the modifiers held with it. Matched exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub vk: u32,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyChord {
    /// A chord with no modifiers.
    pub fn plain(vk: u32) -> Self {
        Self {
            vk,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// The canonical config/UI string: `"Ctrl+Alt+Shift+K"`. An unnamed VK falls back to `"0x2F"`
    /// hex so even an exotic capture round-trips rather than being silently dropped.
    pub fn format(self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(&vk_name(self.vk));
        s
    }

    /// Like [`Self::format`] but with arrow glyphs — nicer in a tooltip or the keybind list, while
    /// the config file keeps the typeable `Left`/`Right` names.
    pub fn display(self) -> String {
        let arrow = match self.vk {
            VK_LEFT => Some('\u{2190}'),
            VK_UP => Some('\u{2191}'),
            VK_RIGHT => Some('\u{2192}'),
            VK_DOWN => Some('\u{2193}'),
            _ => None,
        };
        match arrow {
            Some(g) => {
                let mut s = self.format();
                let cut = s.len() - vk_name(self.vk).len();
                s.truncate(cut);
                s.push(g);
                s
            }
            None => self.format(),
        }
    }

    /// Parse a config/UI string. Modifier names are case-insensitive; the key name matches the
    /// canonical table (also case-insensitively) or a `0x..` hex VK. Returns `None` for an empty or
    /// unrecognized string — the caller treats that as "leave the default in place".
    ///
    /// Modifier *prefixes* are stripped one at a time rather than splitting on `+`, because the key
    /// name itself can be `+` or `Num+` ("Ctrl++" is a legitimate chord).
    pub fn parse(s: &str) -> Option<Self> {
        const MODS: &[(&str, u8)] = &[("ctrl+", 0), ("control+", 0), ("alt+", 1), ("shift+", 2)];
        let mut rest = s.trim();
        let mut chord = KeyChord::plain(0);
        'strip: loop {
            for (prefix, which) in MODS {
                // `len() > prefix.len()` keeps a bare "Ctrl+" (a modifier with no key) unparseable,
                // and guarantees the split lands on a char boundary (the prefix is all ASCII).
                if rest.len() > prefix.len()
                    && rest.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
                {
                    match which {
                        0 => chord.ctrl = true,
                        1 => chord.alt = true,
                        _ => chord.shift = true,
                    }
                    rest = rest[prefix.len()..].trim_start();
                    continue 'strip;
                }
            }
            break;
        }
        chord.vk = parse_vk(rest)?;
        Some(chord)
    }

    /// Whether this chord is one the dialog must not let the user bind (see
    /// [`crate::settings`]): the keys the dialog itself needs to stay usable.
    pub fn is_reserved(self) -> bool {
        // Alt alone (or with modifiers) opens the system menu / is the accelerator prefix, and a
        // bare modifier key is not a chord at all.
        matches!(self.vk, VK_MENU | VK_CONTROL | VK_SHIFT | VK_LWIN | VK_RWIN)
    }
}

// Virtual-key codes we name directly (windows-sys exposes these, but keeping them local keeps this
// module Win32-free and unit-testable).
const VK_BACK: u32 = 0x08;
const VK_TAB: u32 = 0x09;
const VK_RETURN: u32 = 0x0D;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;

/// Virtual keys with a fixed printable name. Letters, digits, numpad digits and function keys are
/// computed instead (see [`vk_name`] / [`parse_vk`]).
const NAMED_VKS: &[(u32, &str)] = &[
    (VK_BACK, "Backspace"),
    (VK_TAB, "Tab"),
    (VK_RETURN, "Enter"),
    (VK_ESCAPE, "Esc"),
    (VK_SPACE, "Space"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x23, "End"),
    (0x24, "Home"),
    (VK_LEFT, "Left"),
    (VK_UP, "Up"),
    (VK_RIGHT, "Right"),
    (VK_DOWN, "Down"),
    (0x2D, "Insert"),
    (0x2E, "Delete"),
    (0x6A, "Num*"),
    (0x6B, "Num+"),
    (0x6C, "NumEnter"),
    (0x6D, "Num-"),
    (0x6E, "Num."),
    (0x6F, "Num/"),
    (0xBA, ";"),
    (0xBB, "="),
    (0xBC, ","),
    (0xBD, "-"),
    (0xBE, "."),
    (0xBF, "/"),
    (0xC0, "`"),
    (0xDB, "["),
    (0xDC, "\\"),
    (0xDD, "]"),
    (0xDE, "'"),
];

/// A virtual key's canonical name (`"F"`, `"F11"`, `"Num+"`, `"["`), or `"0x2F"` hex for one we
/// have no name for.
fn vk_name(vk: u32) -> String {
    if let Some((_, n)) = NAMED_VKS.iter().find(|(v, _)| *v == vk) {
        return (*n).to_string();
    }
    match vk {
        0x30..=0x39 => char::from(b'0' + (vk - 0x30) as u8).to_string(), // 0-9
        0x41..=0x5A => char::from(b'A' + (vk - 0x41) as u8).to_string(), // A-Z
        0x60..=0x69 => format!("Num{}", vk - 0x60),                      // numpad 0-9
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),                    // F1-F24
        _ => format!("{vk:#04X}"),
    }
}

/// Inverse of [`vk_name`] (case-insensitive), including the `0x..` hex fallback.
fn parse_vk(name: &str) -> Option<u32> {
    if let Some((v, _)) = NAMED_VKS.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)) {
        return Some(*v);
    }
    // "+" is how `Num+`-less layouts spell VK_OEM_PLUS; accept it as an alias for "=".
    if name == "+" {
        return Some(0xBB);
    }
    let upper = name.to_ascii_uppercase();
    let b = upper.as_bytes();
    match b {
        [c @ b'0'..=b'9'] => Some(0x30 + (c - b'0') as u32),
        [c @ b'A'..=b'Z'] => Some(0x41 + (c - b'A') as u32),
        _ => {
            if let Some(hex) = upper.strip_prefix("0X") {
                return u32::from_str_radix(hex, 16).ok().filter(|v| *v <= 0xFF);
            }
            if let Some(n) = upper.strip_prefix("NUM") {
                let d: u32 = n.parse().ok()?;
                return (d <= 9).then_some(0x60 + d);
            }
            if let Some(n) = upper.strip_prefix('F') {
                let d: u32 = n.parse().ok()?;
                return (1..=24).contains(&d).then_some(0x70 + d - 1);
            }
            None
        }
    }
}

/// The chord table: every [`KeyAction`] with the chords bound to it (possibly none). Ordered by
/// [`ALL_ACTIONS`], so the settings list can walk it directly.
#[derive(Debug, Clone, PartialEq)]
pub struct Keybinds {
    bindings: Vec<(KeyAction, Vec<KeyChord>)>,
}

impl Default for Keybinds {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Keybinds {
    /// The shipped bindings — Fire's keyboard as it has always been, plus `Shift+=` (how `+` is
    /// typed) and two actions that ship unbound (`ExposureReset`, `ToggleOutline`).
    pub fn defaults() -> Self {
        let c = |s: &str| KeyChord::parse(s).expect("default chord parses");
        let bind = |a: KeyAction, keys: &[&str]| (a, keys.iter().map(|s| c(s)).collect::<Vec<_>>());
        Self {
            bindings: vec![
                bind(KeyAction::OpenFile, &["Ctrl+O"]),
                bind(KeyAction::CloseImage, &["Ctrl+W"]),
                bind(KeyAction::Fit, &["F"]),
                bind(KeyAction::ActualSize, &["1"]),
                bind(KeyAction::ZoomIn, &["=", "Shift+=", "Num+"]),
                bind(KeyAction::ZoomOut, &["-", "Num-"]),
                bind(KeyAction::ChannelRgb, &["C"]),
                bind(KeyAction::ChannelR, &["R"]),
                bind(KeyAction::ChannelG, &["G"]),
                bind(KeyAction::ChannelB, &["B"]),
                bind(KeyAction::ChannelA, &["A"]),
                bind(KeyAction::ToggleTonemap, &["T"]),
                bind(KeyAction::ExposureUp, &["]"]),
                bind(KeyAction::ExposureDown, &["["]),
                bind(KeyAction::ExposureReset, &[]),
                bind(KeyAction::ToggleOutline, &[]),
                bind(KeyAction::CycleBackdrop, &["Z"]),
                bind(KeyAction::PrevImage, &["Left"]),
                bind(KeyAction::NextImage, &["Right"]),
                bind(KeyAction::ToggleFullscreen, &["F11"]),
                bind(KeyAction::CloseOrExitFullscreen, &["Esc"]),
                bind(KeyAction::ToggleFlipbook, &["K"]),
                bind(KeyAction::FlipbookPlayPause, &["Space"]),
                bind(KeyAction::FlipbookPrevFrame, &[","]),
                bind(KeyAction::FlipbookNextFrame, &["."]),
            ],
        }
    }

    /// Build from the config table: start from [`Self::defaults`] and override each action the user
    /// listed. An unknown action name or an unparseable chord is skipped (the default stays), so a
    /// typo costs you one binding, never the whole keyboard. An explicit `""` unbinds.
    pub fn from_config(cfg: &KeybindsCfg) -> Self {
        let mut kb = Self::defaults();
        for (name, value) in cfg.iter() {
            let Some(action) = KeyAction::from_name(name) else {
                continue;
            };
            let chords: Vec<KeyChord> = value
                .as_strings()
                .iter()
                .filter_map(|s| KeyChord::parse(s))
                .filter(|c| !c.is_reserved())
                .collect();
            kb.set_chords(action, chords);
        }
        kb
    }

    /// The config table to persist: only the actions whose chords differ from the defaults, so an
    /// untouched keyboard writes an empty `[keybinds]` and still inherits future default changes.
    /// An action the user unbound is written as `""`.
    pub fn to_config(&self) -> KeybindsCfg {
        let defaults = Self::defaults();
        let mut out = KeybindsCfg::new();
        for (action, chords) in &self.bindings {
            if defaults.chords(*action) == chords.as_slice() {
                continue;
            }
            let value = match chords.as_slice() {
                [] => KeyValue::One(String::new()),
                [one] => KeyValue::One(one.format()),
                many => KeyValue::Many(many.iter().map(|c| c.format()).collect()),
            };
            out.insert(action.name().to_string(), value);
        }
        out
    }

    /// The chords bound to `action` (empty = unbound).
    pub fn chords(&self, action: KeyAction) -> &[KeyChord] {
        self.bindings
            .iter()
            .find(|(a, _)| *a == action)
            .map(|(_, c)| c.as_slice())
            .unwrap_or(&[])
    }

    /// The action a press maps to, or `None` if the chord is unbound. Flipbook-context bindings are
    /// consulted first while the mode is active, and are inert outside it.
    pub fn lookup(&self, chord: KeyChord, in_flipbook: bool) -> Option<KeyAction> {
        if in_flipbook {
            if let Some(a) = self.find(chord, true) {
                return Some(a);
            }
        }
        self.find(chord, false)
    }

    fn find(&self, chord: KeyChord, flipbook_ctx: bool) -> Option<KeyAction> {
        self.bindings
            .iter()
            .find(|(a, cs)| a.is_flipbook_context() == flipbook_ctx && cs.contains(&chord))
            .map(|(a, _)| *a)
    }

    /// The action already holding `chord`, ignoring `except` — the settings tab's conflict check.
    /// One flat namespace: a flipbook-only key still conflicts with a global one.
    pub fn conflict(&self, chord: KeyChord, except: KeyAction) -> Option<KeyAction> {
        self.bindings
            .iter()
            .find(|(a, cs)| *a != except && cs.contains(&chord))
            .map(|(a, _)| *a)
    }

    /// Bind `chord` to `action` as its only chord, **stealing** it from whatever held it (that
    /// action loses just this chord, and may end up unbound). Returns the action it was taken from,
    /// so the dialog can say so. Deterministic and always leaves the table conflict-free.
    pub fn rebind(&mut self, action: KeyAction, chord: KeyChord) -> Option<KeyAction> {
        let loser = self.conflict(chord, action);
        if let Some(l) = loser {
            for (a, cs) in &mut self.bindings {
                if *a == l {
                    cs.retain(|c| *c != chord);
                }
            }
        }
        self.set_chords(action, vec![chord]);
        loser
    }

    /// Remove every chord from `action`. (Only the config path unbinds today — an empty `""` in
    /// `[keybinds]`; the dialog's capture always assigns a chord.)
    #[cfg(test)]
    pub fn unbind(&mut self, action: KeyAction) {
        self.set_chords(action, Vec::new());
    }

    /// Restore one action's shipped chords. May reintroduce a conflict (if the user moved the
    /// default chord elsewhere), so the caller re-checks — the settings tab does.
    pub fn reset(&mut self, action: KeyAction) {
        let d = Self::defaults();
        let chords = d.chords(action).to_vec();
        self.set_chords(action, chords);
    }

    fn set_chords(&mut self, action: KeyAction, chords: Vec<KeyChord>) {
        if let Some((_, cs)) = self.bindings.iter_mut().find(|(a, _)| *a == action) {
            *cs = chords;
        }
    }

    /// The shortcut labels the toolbar tooltips render (primary chord per action), so rebinding a
    /// key relabels its button.
    pub fn labels(&self) -> ShortcutLabels {
        ShortcutLabels(
            self.bindings
                .iter()
                .filter_map(|(a, cs)| cs.first().map(|c| (*a, c.display())))
                .collect(),
        )
    }
}

/// Primary-chord display strings, keyed by action — carried in the chrome's [`crate::chrome::ViewSnapshot`]
/// so tooltips show the *current* binding rather than a literal baked into the string.
#[derive(Debug, Clone, Default)]
pub struct ShortcutLabels(Vec<(KeyAction, String)>);

impl ShortcutLabels {
    /// The primary chord for `action`, or `None` when it is unbound (the tooltip then shows no
    /// parenthetical at all).
    pub fn get(&self, action: KeyAction) -> Option<&str> {
        self.0
            .iter()
            .find(|(a, _)| *a == action)
            .map(|(_, s)| s.as_str())
    }

    /// `"  (F)"` — the tooltip suffix for `action`, or an empty string when unbound.
    pub fn suffix(&self, action: KeyAction) -> String {
        self.get(action)
            .map(|k| format!("  ({k})"))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every VK we can name round-trips through `format` → `parse`, including the hex fallback and
    /// the computed letter/digit/numpad/function ranges.
    #[test]
    fn chord_round_trip() {
        let vks = NAMED_VKS
            .iter()
            .map(|(v, _)| *v)
            .chain(0x30..=0x39) // digits
            .chain(0x41..=0x5A) // letters
            .chain(0x60..=0x69) // numpad digits
            .chain(0x70..=0x87) // F1-F24
            .chain([0x2F]); // unnamed → hex fallback
        for vk in vks {
            for (ctrl, alt, shift) in [
                (false, false, false),
                (true, false, false),
                (false, true, false),
                (false, false, true),
                (true, true, true),
            ] {
                let c = KeyChord {
                    vk,
                    ctrl,
                    alt,
                    shift,
                };
                let s = c.format();
                assert_eq!(KeyChord::parse(&s), Some(c), "round-trip {s:?}");
            }
        }
    }

    #[test]
    fn chord_parse_is_lenient_but_strict_enough() {
        assert_eq!(KeyChord::parse("f"), Some(KeyChord::plain(0x46)));
        assert_eq!(
            KeyChord::parse("ctrl+shift+k"),
            Some(KeyChord {
                vk: 0x4B,
                ctrl: true,
                alt: false,
                shift: true
            })
        );
        // "+" is accepted as an alias for the `=` key (VK_OEM_PLUS), with or without modifiers.
        assert_eq!(KeyChord::parse("+"), Some(KeyChord::plain(0xBB)));
        assert_eq!(KeyChord::parse(""), None);
        assert_eq!(KeyChord::parse("Ctrl"), None); // modifier with no key
        assert_eq!(KeyChord::parse("F+G"), None); // two keys
        assert_eq!(KeyChord::parse("Nope"), None);
    }

    /// Arrow keys read as glyphs in the UI but stay typeable in the config file.
    #[test]
    fn arrow_display_vs_format() {
        let left = KeyChord::plain(VK_LEFT);
        assert_eq!(left.format(), "Left");
        assert_eq!(left.display(), "\u{2190}");
        let ctrl_right = KeyChord {
            vk: VK_RIGHT,
            ctrl: true,
            alt: false,
            shift: false,
        };
        assert_eq!(ctrl_right.format(), "Ctrl+Right");
        assert_eq!(ctrl_right.display(), "Ctrl+\u{2192}");
    }

    /// The defaults reproduce the keyboard Fire shipped with (the table that used to live in
    /// `App::handle_key` as raw VK matches).
    #[test]
    fn defaults_match_the_legacy_key_table() {
        let kb = Keybinds::defaults();
        let cases: &[(u32, KeyAction)] = &[
            (0x46, KeyAction::Fit),
            (0x31, KeyAction::ActualSize),
            (0x52, KeyAction::ChannelR),
            (0x47, KeyAction::ChannelG),
            (0x42, KeyAction::ChannelB),
            (0x41, KeyAction::ChannelA),
            (0x43, KeyAction::ChannelRgb),
            (0x54, KeyAction::ToggleTonemap),
            (0x4B, KeyAction::ToggleFlipbook),
            (0xDD, KeyAction::ExposureUp),
            (0xDB, KeyAction::ExposureDown),
            (0xBB, KeyAction::ZoomIn),
            (0x6B, KeyAction::ZoomIn),
            (0xBD, KeyAction::ZoomOut),
            (0x6D, KeyAction::ZoomOut),
            (0x25, KeyAction::PrevImage),
            (0x27, KeyAction::NextImage),
            (0x7A, KeyAction::ToggleFullscreen),
            (0x1B, KeyAction::CloseOrExitFullscreen),
        ];
        for (vk, action) in cases {
            assert_eq!(
                kb.lookup(KeyChord::plain(*vk), false),
                Some(*action),
                "vk {vk:#04X}"
            );
        }
        // Shift+= (how "+" is typed) also zooms in.
        assert_eq!(
            kb.lookup(
                KeyChord {
                    vk: 0xBB,
                    ctrl: false,
                    alt: false,
                    shift: true
                },
                false
            ),
            Some(KeyAction::ZoomIn)
        );
        // The flipbook keys fire only inside the mode.
        for (vk, action) in [
            (0x20, KeyAction::FlipbookPlayPause),
            (0xBC, KeyAction::FlipbookPrevFrame),
            (0xBE, KeyAction::FlipbookNextFrame),
        ] {
            assert_eq!(kb.lookup(KeyChord::plain(vk), true), Some(action));
            assert_eq!(kb.lookup(KeyChord::plain(vk), false), None);
        }
        // Two actions ship unbound.
        assert!(kb.chords(KeyAction::ExposureReset).is_empty());
        assert!(kb.chords(KeyAction::ToggleOutline).is_empty());
        // Z walks the backdrops.
        assert_eq!(
            kb.lookup(KeyChord::plain(0x5A), false),
            Some(KeyAction::CycleBackdrop)
        );
    }

    /// The file commands ship on Ctrl chords, so a bare `O` / `W` stays free — and, being global,
    /// they still fire inside flipbook mode.
    #[test]
    fn file_commands_are_ctrl_chords() {
        let kb = Keybinds::defaults();
        let ctrl = |vk| KeyChord {
            vk,
            ctrl: true,
            alt: false,
            shift: false,
        };
        for in_flipbook in [false, true] {
            assert_eq!(
                kb.lookup(ctrl(0x4F), in_flipbook),
                Some(KeyAction::OpenFile)
            );
            assert_eq!(
                kb.lookup(ctrl(0x57), in_flipbook),
                Some(KeyAction::CloseImage)
            );
            // Chords match exactly, so the unmodified letters are untouched.
            assert_eq!(kb.lookup(KeyChord::plain(0x4F), in_flipbook), None);
            assert_eq!(kb.lookup(KeyChord::plain(0x57), in_flipbook), None);
        }
    }

    /// A chord held by another action is *stolen*, leaving the table conflict-free.
    #[test]
    fn rebind_steals_the_chord() {
        let mut kb = Keybinds::defaults();
        let f = KeyChord::plain(0x46); // F, currently Fit
        assert_eq!(kb.conflict(f, KeyAction::NextImage), Some(KeyAction::Fit));

        let loser = kb.rebind(KeyAction::NextImage, f);
        assert_eq!(loser, Some(KeyAction::Fit));
        assert_eq!(kb.lookup(f, false), Some(KeyAction::NextImage));
        assert!(kb.chords(KeyAction::Fit).is_empty());
        // Next image's old binding is gone (rebind replaces, it doesn't append).
        assert_eq!(kb.lookup(KeyChord::plain(0x27), false), None);

        kb.reset(KeyAction::NextImage);
        assert_eq!(
            kb.lookup(KeyChord::plain(0x27), false),
            Some(KeyAction::NextImage)
        );
    }

    /// Stealing only one chord of a multi-chord action leaves its other chords alone.
    #[test]
    fn rebind_steals_only_the_one_chord() {
        let mut kb = Keybinds::defaults();
        let numplus = KeyChord::plain(0x6B); // an alias of ZoomIn
        kb.rebind(KeyAction::ExposureReset, numplus);
        assert_eq!(kb.lookup(numplus, false), Some(KeyAction::ExposureReset));
        // `=` still zooms in.
        assert_eq!(
            kb.lookup(KeyChord::plain(0xBB), false),
            Some(KeyAction::ZoomIn)
        );
    }

    /// Only rebound actions are written; an untouched table serializes to nothing.
    #[test]
    fn to_config_writes_only_overrides() {
        let kb = Keybinds::defaults();
        assert!(kb.to_config().is_empty());

        let mut kb = Keybinds::defaults();
        // Q, a chord nothing ships bound to — so exactly two actions end up differing from the
        // defaults. (Rebinding onto an *occupied* chord would unbind its owner and write a third
        // entry; that's `rebind_steals_the_chord`'s job, not this test's.)
        kb.rebind(KeyAction::Fit, KeyChord::plain(0x51));
        kb.unbind(KeyAction::ToggleTonemap);
        let cfg = kb.to_config();
        assert_eq!(cfg.get("fit"), Some(&KeyValue::One("Q".into())));
        assert_eq!(
            cfg.get("toggle-tonemap"),
            Some(&KeyValue::One(String::new()))
        );
        assert_eq!(cfg.len(), 2);

        // …and reading it back reproduces the table exactly.
        assert_eq!(Keybinds::from_config(&cfg), kb);
    }

    /// A garbage config entry costs one binding at most — never the whole keyboard.
    #[test]
    fn from_config_ignores_junk() {
        let mut cfg = KeybindsCfg::new();
        cfg.insert("not-an-action".into(), KeyValue::One("Q".into()));
        cfg.insert("fit".into(), KeyValue::One("NotAKey".into()));
        cfg.insert(
            "zoom-in".into(),
            KeyValue::Many(vec!["W".into(), "bogus".into()]),
        );
        let kb = Keybinds::from_config(&cfg);
        // "fit" had no parseable chord → it ends up unbound (the entry was present, just useless).
        assert!(kb.chords(KeyAction::Fit).is_empty());
        // zoom-in keeps the one chord that parsed.
        assert_eq!(kb.chords(KeyAction::ZoomIn), &[KeyChord::plain(0x57)]);
        // Everything else is untouched.
        assert_eq!(
            kb.lookup(KeyChord::plain(0x54), false),
            Some(KeyAction::ToggleTonemap)
        );
    }

    /// Tooltip labels follow the live bindings.
    #[test]
    fn labels_track_rebinds() {
        let mut kb = Keybinds::defaults();
        assert_eq!(kb.labels().suffix(KeyAction::Fit), "  (F)");
        assert_eq!(kb.labels().get(KeyAction::PrevImage), Some("\u{2190}"));
        assert_eq!(kb.labels().suffix(KeyAction::ExposureReset), "");

        kb.rebind(
            KeyAction::Fit,
            KeyChord {
                vk: 0x5A,
                ctrl: true,
                alt: false,
                shift: false,
            },
        );
        assert_eq!(kb.labels().suffix(KeyAction::Fit), "  (Ctrl+Z)");
    }
}
