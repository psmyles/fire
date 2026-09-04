//! Keyboard bindings — pure logic, no window system (unit-tested like [`crate::folder`] and
//! [`crate::render::view`]).
//!
//! Every keyboard command the viewer has is a [`KeyAction`]; a [`Keybinds`] table maps chords to
//! them. The shell builds a [`KeyChord`] from the pressed *physical* key plus the live modifier
//! state, looks it up here, and dispatches — so *what* a key does lives in one table instead of a
//! `match` on key codes, which is what makes the settings dialog's rebind editor possible. It is
//! also where the toolbar's tooltips get their "(F)" suffixes ([`Keybinds::labels`]), so a rebound
//! key relabels the button it belongs to.
//!
//! **Keys are physical.** A chord names a [`KeyCode`] — the key at the position `F` has on a US
//! keyboard — not the character it types, so one `config.toml` means the same thing on every layout
//! and every OS. The modifier is `Primary`: Ctrl on Windows and Linux, ⌘ on macOS, resolved when
//! the chord is matched.
//!
//! **Chords match exactly.** `Left` and `Ctrl+Left` are different bindings, so a modifier held by
//! accident no longer triggers the plain command (and, conversely, `Ctrl+…` chords are bindable).
//! The one concession is `Shift+=`, bound alongside `=` by default, because that is how you type
//! `+` on most layouts.
//!
//! Chords round-trip through `config.toml` as strings (`"F"`, `"Primary+Shift+K"`, `"Num+"`) — see
//! [`Keybinds::from_config`] / [`Keybinds::to_config`]. `Ctrl+` and `Cmd+` are accepted as
//! spellings of `Primary+`, so a file written by an earlier version still reads. Only bindings that
//! *differ* from the defaults are written, so a user who never rebinds anything keeps an empty
//! `[keybinds]` table and inherits future default changes.

use winit::keyboard::KeyCode;

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
            KeyAction::ChannelRgb => "All channels: RGBA \u{2194} RGB",
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

/// One key press: a physical key plus the modifiers held with it. Matched exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub key: KeyCode,
    /// The platform's command modifier: Ctrl on Windows and Linux, ⌘ on macOS.
    pub primary: bool,
    pub alt: bool,
    pub shift: bool,
}

/// How the primary modifier is spelled in the UI on this OS. (The config file always says
/// `Primary+`, so one file serves both.)
const PRIMARY_LABEL: &str = if cfg!(target_os = "macos") {
    "Cmd+"
} else {
    "Ctrl+"
};

impl KeyChord {
    /// A chord with no modifiers.
    pub fn plain(key: KeyCode) -> Self {
        Self {
            key,
            primary: false,
            alt: false,
            shift: false,
        }
    }

    /// The canonical config string: `"Primary+Alt+Shift+K"`. A key without a name of its own
    /// falls back to its `KeyCode` identifier (`"IntlBackslash"`), so even an exotic capture
    /// round-trips rather than being silently dropped.
    pub fn format(self) -> String {
        self.format_with(if self.primary { "Primary+" } else { "" })
    }

    /// The string the UI shows: like [`Self::format`], but with the primary modifier spelled for
    /// this OS (`Ctrl+` / `Cmd+`) and arrow glyphs for the arrow keys. The config file keeps the
    /// typeable `Primary+Left` form.
    pub fn display(self) -> String {
        let mut s = self.format_with(if self.primary { PRIMARY_LABEL } else { "" });
        let arrow = match self.key {
            KeyCode::ArrowLeft => Some('\u{2190}'),
            KeyCode::ArrowUp => Some('\u{2191}'),
            KeyCode::ArrowRight => Some('\u{2192}'),
            KeyCode::ArrowDown => Some('\u{2193}'),
            _ => None,
        };
        if let Some(g) = arrow {
            let cut = s.len() - key_name(self.key).len();
            s.truncate(cut);
            s.push(g);
        }
        s
    }

    fn format_with(self, primary: &str) -> String {
        let mut s = String::from(primary);
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(&key_name(self.key));
        s
    }

    /// Parse a config/UI string. Modifier names are case-insensitive (`Primary`, and its
    /// per-OS spellings `Ctrl`/`Control`/`Cmd`/`Command`, plus `Alt`/`Option` and `Shift`); the
    /// key name matches the canonical table (also case-insensitively) or a bare `KeyCode`
    /// identifier. Returns `None` for an empty or unrecognized string — the caller treats that as
    /// "leave the default in place".
    ///
    /// Modifier *prefixes* are stripped one at a time rather than splitting on `+`, because the key
    /// name itself can be `+` or `Num+` ("Ctrl++" is a legitimate chord).
    pub fn parse(s: &str) -> Option<Self> {
        const MODS: &[(&str, u8)] = &[
            ("primary+", 0),
            ("ctrl+", 0),
            ("control+", 0),
            ("cmd+", 0),
            ("command+", 0),
            ("alt+", 1),
            ("option+", 1),
            ("shift+", 2),
        ];
        let mut rest = s.trim();
        let mut chord = KeyChord::plain(KeyCode::Escape);
        'strip: loop {
            for (prefix, which) in MODS {
                // `len() > prefix.len()` keeps a bare "Ctrl+" (a modifier with no key) unparseable,
                // and guarantees the split lands on a char boundary (the prefix is all ASCII).
                if rest.len() > prefix.len()
                    && rest.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
                {
                    match which {
                        0 => chord.primary = true,
                        1 => chord.alt = true,
                        _ => chord.shift = true,
                    }
                    rest = rest[prefix.len()..].trim_start();
                    continue 'strip;
                }
            }
            break;
        }
        chord.key = parse_key(rest)?;
        Some(chord)
    }

    /// Whether this chord is one the dialog must not let the user bind (see
    /// [`crate::ui::settings`]): a bare modifier key is not a chord at all, and the OS key opens
    /// the system menu.
    pub fn is_reserved(self) -> bool {
        matches!(
            self.key,
            KeyCode::ShiftLeft
                | KeyCode::ShiftRight
                | KeyCode::ControlLeft
                | KeyCode::ControlRight
                | KeyCode::AltLeft
                | KeyCode::AltRight
                | KeyCode::SuperLeft
                | KeyCode::SuperRight
                | KeyCode::Meta
                | KeyCode::Hyper
        )
    }
}

/// Keys with a fixed printable name. Letters, digits, numpad digits and function keys are
/// computed instead (see [`key_name`] / [`parse_key`]); anything else falls back to the `KeyCode`
/// identifier, which [`EXTRA`] makes parseable too.
const NAMED: &[(KeyCode, &str)] = &[
    (KeyCode::Backspace, "Backspace"),
    (KeyCode::Tab, "Tab"),
    (KeyCode::Enter, "Enter"),
    (KeyCode::Escape, "Esc"),
    (KeyCode::Space, "Space"),
    (KeyCode::PageUp, "PageUp"),
    (KeyCode::PageDown, "PageDown"),
    (KeyCode::End, "End"),
    (KeyCode::Home, "Home"),
    (KeyCode::ArrowLeft, "Left"),
    (KeyCode::ArrowUp, "Up"),
    (KeyCode::ArrowRight, "Right"),
    (KeyCode::ArrowDown, "Down"),
    (KeyCode::Insert, "Insert"),
    (KeyCode::Delete, "Delete"),
    (KeyCode::NumpadMultiply, "Num*"),
    (KeyCode::NumpadAdd, "Num+"),
    (KeyCode::NumpadEnter, "NumEnter"),
    (KeyCode::NumpadSubtract, "Num-"),
    (KeyCode::NumpadDecimal, "Num."),
    (KeyCode::NumpadDivide, "Num/"),
    (KeyCode::Semicolon, ";"),
    (KeyCode::Equal, "="),
    (KeyCode::Comma, ","),
    (KeyCode::Minus, "-"),
    (KeyCode::Period, "."),
    (KeyCode::Slash, "/"),
    (KeyCode::Backquote, "`"),
    (KeyCode::BracketLeft, "["),
    (KeyCode::Backslash, "\\"),
    (KeyCode::BracketRight, "]"),
    (KeyCode::Quote, "'"),
];

/// Keys without a name of their own that are still worth being able to *parse* back from their
/// `KeyCode` identifier — the ones a keyboard is likely to have. A key outside this list still
/// formats (as its identifier); it just cannot be typed into `config.toml` by hand.
const EXTRA: &[KeyCode] = &[
    KeyCode::CapsLock,
    KeyCode::NumLock,
    KeyCode::ScrollLock,
    KeyCode::PrintScreen,
    KeyCode::Pause,
    KeyCode::ContextMenu,
    KeyCode::IntlBackslash,
    KeyCode::IntlRo,
    KeyCode::IntlYen,
    KeyCode::NumpadEqual,
    KeyCode::NumpadComma,
    KeyCode::Fn,
];

const LETTERS: [KeyCode; 26] = [
    KeyCode::KeyA,
    KeyCode::KeyB,
    KeyCode::KeyC,
    KeyCode::KeyD,
    KeyCode::KeyE,
    KeyCode::KeyF,
    KeyCode::KeyG,
    KeyCode::KeyH,
    KeyCode::KeyI,
    KeyCode::KeyJ,
    KeyCode::KeyK,
    KeyCode::KeyL,
    KeyCode::KeyM,
    KeyCode::KeyN,
    KeyCode::KeyO,
    KeyCode::KeyP,
    KeyCode::KeyQ,
    KeyCode::KeyR,
    KeyCode::KeyS,
    KeyCode::KeyT,
    KeyCode::KeyU,
    KeyCode::KeyV,
    KeyCode::KeyW,
    KeyCode::KeyX,
    KeyCode::KeyY,
    KeyCode::KeyZ,
];

const DIGITS: [KeyCode; 10] = [
    KeyCode::Digit0,
    KeyCode::Digit1,
    KeyCode::Digit2,
    KeyCode::Digit3,
    KeyCode::Digit4,
    KeyCode::Digit5,
    KeyCode::Digit6,
    KeyCode::Digit7,
    KeyCode::Digit8,
    KeyCode::Digit9,
];

const NUMPAD: [KeyCode; 10] = [
    KeyCode::Numpad0,
    KeyCode::Numpad1,
    KeyCode::Numpad2,
    KeyCode::Numpad3,
    KeyCode::Numpad4,
    KeyCode::Numpad5,
    KeyCode::Numpad6,
    KeyCode::Numpad7,
    KeyCode::Numpad8,
    KeyCode::Numpad9,
];

const FKEYS: [KeyCode; 24] = [
    KeyCode::F1,
    KeyCode::F2,
    KeyCode::F3,
    KeyCode::F4,
    KeyCode::F5,
    KeyCode::F6,
    KeyCode::F7,
    KeyCode::F8,
    KeyCode::F9,
    KeyCode::F10,
    KeyCode::F11,
    KeyCode::F12,
    KeyCode::F13,
    KeyCode::F14,
    KeyCode::F15,
    KeyCode::F16,
    KeyCode::F17,
    KeyCode::F18,
    KeyCode::F19,
    KeyCode::F20,
    KeyCode::F21,
    KeyCode::F22,
    KeyCode::F23,
    KeyCode::F24,
];

/// A key's canonical name (`"F"`, `"F11"`, `"Num+"`, `"["`), or its `KeyCode` identifier for one
/// we have no name for.
fn key_name(key: KeyCode) -> String {
    if let Some((_, n)) = NAMED.iter().find(|(k, _)| *k == key) {
        return (*n).to_string();
    }
    if let Some(i) = LETTERS.iter().position(|k| *k == key) {
        return char::from(b'A' + i as u8).to_string();
    }
    if let Some(i) = DIGITS.iter().position(|k| *k == key) {
        return char::from(b'0' + i as u8).to_string();
    }
    if let Some(i) = NUMPAD.iter().position(|k| *k == key) {
        return format!("Num{i}");
    }
    if let Some(i) = FKEYS.iter().position(|k| *k == key) {
        return format!("F{}", i + 1);
    }
    format!("{key:?}")
}

/// Inverse of [`key_name`] (case-insensitive), including the identifier fallback for [`EXTRA`].
fn parse_key(name: &str) -> Option<KeyCode> {
    if let Some((k, _)) = NAMED.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)) {
        return Some(*k);
    }
    // "+" is how `Num+`-less layouts spell the plus key; accept it as an alias for "=".
    if name == "+" {
        return Some(KeyCode::Equal);
    }
    let upper = name.to_ascii_uppercase();
    match upper.as_bytes() {
        [c @ b'0'..=b'9'] => Some(DIGITS[(c - b'0') as usize]),
        [c @ b'A'..=b'Z'] => Some(LETTERS[(c - b'A') as usize]),
        _ => {
            // `Num5` / `F11` — but `NumLock` and `Fn` also start this way and are identifiers
            // (below), so a prefix that isn't followed by a number falls through.
            if let Some(d) = upper
                .strip_prefix("NUM")
                .and_then(|n| n.parse::<usize>().ok())
            {
                return NUMPAD.get(d).copied();
            }
            if let Some(d) = upper
                .strip_prefix('F')
                .and_then(|n| n.parse::<usize>().ok())
            {
                return (1..=24).contains(&d).then(|| FKEYS[d - 1]);
            }
            EXTRA
                .iter()
                .copied()
                .find(|k| format!("{k:?}").eq_ignore_ascii_case(name))
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
                bind(KeyAction::OpenFile, &["Primary+O"]),
                bind(KeyAction::CloseImage, &["Primary+W"]),
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
            let strings = value.as_strings();
            let chords: Vec<KeyChord> = strings
                .iter()
                .filter_map(|s| KeyChord::parse(s))
                .filter(|c| !c.is_reserved())
                .collect();
            // Nothing parsed: an explicit `""` is an unbind, but a typo keeps the default rather
            // than silently unbinding the action.
            let explicit_unbind = strings.iter().all(|s| s.trim().is_empty());
            if chords.is_empty() && !explicit_unbind {
                continue;
            }
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
            .map_or(&[], |(_, c)| c.as_slice())
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

    /// Every key with a name round-trips through format → parse, including the identifier
    /// fallback for the extras.
    #[test]
    fn names_round_trip() {
        let keys = NAMED
            .iter()
            .map(|(k, _)| *k)
            .chain(LETTERS)
            .chain(DIGITS)
            .chain(NUMPAD)
            .chain(FKEYS)
            .chain(EXTRA.iter().copied());
        for key in keys {
            for (primary, alt, shift) in [
                (false, false, false),
                (true, false, false),
                (true, true, true),
            ] {
                let chord = KeyChord {
                    key,
                    primary,
                    alt,
                    shift,
                };
                assert_eq!(
                    KeyChord::parse(&chord.format()),
                    Some(chord),
                    "{}",
                    chord.format()
                );
            }
        }
    }

    #[test]
    fn parse_is_case_insensitive_and_accepts_every_primary_spelling() {
        assert_eq!(KeyChord::parse("f"), Some(KeyChord::plain(KeyCode::KeyF)));
        let expect = KeyChord {
            key: KeyCode::KeyK,
            primary: true,
            alt: false,
            shift: true,
        };
        for s in [
            "Primary+Shift+K",
            "ctrl+shift+k",
            "Control+Shift+K",
            "Cmd+Shift+K",
            "command+shift+k",
        ] {
            assert_eq!(KeyChord::parse(s), Some(expect), "{s}");
        }
        // "+" is accepted as an alias for the `=` key, with or without modifiers.
        assert_eq!(KeyChord::parse("+"), Some(KeyChord::plain(KeyCode::Equal)));
        assert_eq!(
            KeyChord::parse("Ctrl++"),
            Some(KeyChord {
                key: KeyCode::Equal,
                primary: true,
                alt: false,
                shift: false
            })
        );
        // A bare modifier, an empty string and an unknown name all fail to parse.
        assert_eq!(KeyChord::parse("Ctrl+"), None);
        assert_eq!(KeyChord::parse(""), None);
        assert_eq!(KeyChord::parse("Bogus"), None);
    }

    #[test]
    fn display_uses_arrow_glyphs_and_the_os_modifier_name() {
        let left = KeyChord::plain(KeyCode::ArrowLeft);
        assert_eq!(left.display(), "\u{2190}");
        assert_eq!(left.format(), "Left");
        let ctrl_right = KeyChord {
            key: KeyCode::ArrowRight,
            primary: true,
            alt: false,
            shift: false,
        };
        assert_eq!(ctrl_right.format(), "Primary+Right");
        assert_eq!(ctrl_right.display(), format!("{PRIMARY_LABEL}\u{2192}"));
    }

    #[test]
    fn defaults_match_the_shipped_keyboard() {
        let kb = Keybinds::defaults();
        let cases = [
            (KeyCode::KeyF, KeyAction::Fit),
            (KeyCode::Digit1, KeyAction::ActualSize),
            (KeyCode::KeyR, KeyAction::ChannelR),
            (KeyCode::KeyG, KeyAction::ChannelG),
            (KeyCode::KeyB, KeyAction::ChannelB),
            (KeyCode::KeyA, KeyAction::ChannelA),
            (KeyCode::KeyC, KeyAction::ChannelRgb),
            (KeyCode::KeyT, KeyAction::ToggleTonemap),
            (KeyCode::KeyK, KeyAction::ToggleFlipbook),
            (KeyCode::BracketRight, KeyAction::ExposureUp),
            (KeyCode::BracketLeft, KeyAction::ExposureDown),
            (KeyCode::Equal, KeyAction::ZoomIn),
            (KeyCode::NumpadAdd, KeyAction::ZoomIn),
            (KeyCode::Minus, KeyAction::ZoomOut),
            (KeyCode::NumpadSubtract, KeyAction::ZoomOut),
            (KeyCode::ArrowLeft, KeyAction::PrevImage),
            (KeyCode::ArrowRight, KeyAction::NextImage),
            (KeyCode::F11, KeyAction::ToggleFullscreen),
            (KeyCode::Escape, KeyAction::CloseOrExitFullscreen),
        ];
        for (key, action) in cases {
            assert_eq!(
                kb.lookup(KeyChord::plain(key), false),
                Some(action),
                "{key:?}"
            );
        }
        // Shift+= is bound alongside = (that's how + is typed).
        assert_eq!(
            kb.lookup(
                KeyChord {
                    key: KeyCode::Equal,
                    primary: false,
                    alt: false,
                    shift: true,
                },
                false
            ),
            Some(KeyAction::ZoomIn)
        );
        // The flipbook keys are inert outside the mode.
        for (key, action) in [
            (KeyCode::Space, KeyAction::FlipbookPlayPause),
            (KeyCode::Comma, KeyAction::FlipbookPrevFrame),
            (KeyCode::Period, KeyAction::FlipbookNextFrame),
        ] {
            assert_eq!(kb.lookup(KeyChord::plain(key), true), Some(action));
            assert_eq!(kb.lookup(KeyChord::plain(key), false), None);
        }
        // Z cycles the backdrop, in either mode.
        assert_eq!(
            kb.lookup(KeyChord::plain(KeyCode::KeyZ), false),
            Some(KeyAction::CycleBackdrop)
        );
        // Ctrl+O / Ctrl+W are chords; the bare letters do nothing.
        let ctrl = |key| KeyChord {
            key,
            primary: true,
            alt: false,
            shift: false,
        };
        for in_flipbook in [false, true] {
            assert_eq!(
                kb.lookup(ctrl(KeyCode::KeyO), in_flipbook),
                Some(KeyAction::OpenFile)
            );
            assert_eq!(
                kb.lookup(ctrl(KeyCode::KeyW), in_flipbook),
                Some(KeyAction::CloseImage)
            );
            assert_eq!(kb.lookup(KeyChord::plain(KeyCode::KeyO), in_flipbook), None);
            assert_eq!(kb.lookup(KeyChord::plain(KeyCode::KeyW), in_flipbook), None);
        }
    }

    #[test]
    fn chords_match_exactly() {
        let kb = Keybinds::defaults();
        // A stray modifier no longer triggers the plain command.
        assert_eq!(
            kb.lookup(
                KeyChord {
                    key: KeyCode::KeyF,
                    primary: true,
                    alt: false,
                    shift: false,
                },
                false
            ),
            None
        );
    }

    #[test]
    fn config_overrides_and_unbinds() {
        let mut cfg = KeybindsCfg::new();
        cfg.insert("fit".into(), KeyValue::One("Ctrl+F".into()));
        cfg.insert(
            "zoom-in".into(),
            KeyValue::Many(vec!["=".into(), "Num+".into()]),
        );
        cfg.insert("toggle-tonemap".into(), KeyValue::One(String::new()));
        cfg.insert("no-such-action".into(), KeyValue::One("Q".into()));
        cfg.insert("zoom-out".into(), KeyValue::One("Bogus".into()));
        let kb = Keybinds::from_config(&cfg);
        assert_eq!(kb.lookup(KeyChord::plain(KeyCode::KeyF), false), None);
        assert_eq!(
            kb.lookup(
                KeyChord {
                    key: KeyCode::KeyF,
                    primary: true,
                    alt: false,
                    shift: false,
                },
                false
            ),
            Some(KeyAction::Fit)
        );
        // Shift+= dropped by the override; = and Num+ kept.
        assert_eq!(
            kb.lookup(KeyChord::plain(KeyCode::Equal), false),
            Some(KeyAction::ZoomIn)
        );
        assert_eq!(
            kb.lookup(
                KeyChord {
                    key: KeyCode::Equal,
                    primary: false,
                    alt: false,
                    shift: true,
                },
                false
            ),
            None
        );
        // "" unbinds.
        assert_eq!(kb.lookup(KeyChord::plain(KeyCode::KeyT), false), None);
        // A bad chord leaves the default in place.
        assert_eq!(
            kb.lookup(KeyChord::plain(KeyCode::Minus), false),
            Some(KeyAction::ZoomOut)
        );
    }

    #[test]
    fn to_config_writes_only_the_differences() {
        let mut kb = Keybinds::defaults();
        assert!(kb.to_config().is_empty());
        kb.rebind(KeyAction::Fit, KeyChord::plain(KeyCode::KeyQ));
        kb.unbind(KeyAction::ChannelR);
        let cfg = kb.to_config();
        assert_eq!(cfg.len(), 2);
        assert_eq!(cfg.get("fit"), Some(&KeyValue::One("Q".into())));
        assert_eq!(cfg.get("red-channel"), Some(&KeyValue::One(String::new())));
        // And reading it back reproduces the table.
        assert_eq!(Keybinds::from_config(&cfg), kb);
    }

    #[test]
    fn rebind_steals_and_reports_the_loser() {
        let mut kb = Keybinds::defaults();
        let f = KeyChord::plain(KeyCode::KeyF);
        assert_eq!(kb.rebind(KeyAction::ZoomIn, f), Some(KeyAction::Fit));
        assert_eq!(kb.chords(KeyAction::Fit), &[]);
        assert_eq!(kb.chords(KeyAction::ZoomIn), &[f]);
        assert_eq!(kb.conflict(f, KeyAction::ZoomIn), None);
        // A chord nobody holds reports no loser.
        assert_eq!(
            kb.rebind(KeyAction::ExposureReset, KeyChord::plain(KeyCode::KeyQ)),
            None
        );
        // Reset restores the shipped chord (F), which now conflicts with ZoomIn.
        kb.reset(KeyAction::Fit);
        assert_eq!(kb.conflict(f, KeyAction::Fit), Some(KeyAction::ZoomIn));
    }

    #[test]
    fn labels_follow_the_primary_chord() {
        let mut kb = Keybinds::defaults();
        let l = kb.labels();
        assert_eq!(l.get(KeyAction::Fit), Some("F"));
        assert_eq!(l.get(KeyAction::PrevImage), Some("\u{2190}"));
        assert_eq!(l.suffix(KeyAction::Fit), "  (F)");
        assert_eq!(l.get(KeyAction::ExposureReset), None);
        assert_eq!(l.suffix(KeyAction::ExposureReset), "");
        kb.rebind(KeyAction::Fit, KeyChord::plain(KeyCode::KeyQ));
        assert_eq!(kb.labels().get(KeyAction::Fit), Some("Q"));
    }

    #[test]
    fn reserved_keys_are_not_chords() {
        assert!(KeyChord::plain(KeyCode::ShiftLeft).is_reserved());
        assert!(KeyChord::plain(KeyCode::SuperLeft).is_reserved());
        assert!(!KeyChord::plain(KeyCode::KeyF).is_reserved());
    }
}
