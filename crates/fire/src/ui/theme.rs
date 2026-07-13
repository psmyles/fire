//! The ImGui style — **loaded from [`theme.toml`](./theme.toml), not written here.**
//!
//! Every color, metric and spacing value the UI draws with lives in that file, next to this one.
//! This module is the machinery around it: it parses the stylesheet, resolves its little color
//! grammar (`#hex` / `accent` / `lift(…)` / `alpha(…)` / `contrast(…)`) against the light or dark
//! token set and the user's system accent, and applies the result to ImGui's two styles — [`apply`]
//! for the chrome (a *toolbar*) and [`form`] for the settings window (a *form*). The mapping from a
//! semantic token to an ImGui `StyleColor` is the only styling decision that stays in the code, and
//! it is deliberately mechanical: one line per color, no arithmetic.
//!
//! **Where the stylesheet comes from:**
//! * *release* — [`EMBEDDED`], compiled in with `include_str!`. The disk is never touched.
//! * *debug* — read from [`SOURCE_PATH`] (this file's neighbour in the source tree) at startup, and
//!   watched for changes by [`crate::hotstyle`], which calls [`reload`] and pokes the window. So an
//!   edit to `theme.toml` restyles the running app. A bad edit prints and changes nothing: [`reload`]
//!   swaps the live theme in only once the new one has parsed *and* every color in it has resolved,
//!   so a typo'd token name can never reach the screen.
//!
//! Sizes in the stylesheet are logical (96-dpi) px and are scaled here by the monitor's DPI factor —
//! ImGui's `font_scale_dpi` handles *glyphs* only. Border widths are deliberately left unscaled: a
//! 1 px hairline stays 1 physical px.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

use dear_imgui_rs::{Style, StyleColor};
use serde::Deserialize;

/// The stylesheet, compiled into the exe. This is what a release build always uses.
const EMBEDDED: &str = include_str!("theme.toml");

/// The same file, in the source tree. Debug builds load and watch this one so edits land live.
#[cfg(debug_assertions)]
pub const SOURCE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui/theme.toml");

/// Drawn in place of a color that failed to resolve. Unreachable — [`Theme::parse`] resolves every
/// color before a stylesheet is accepted — and violently visible if it ever isn't.
const UNRESOLVED: [f32; 4] = [1.0, 0.0, 1.0, 1.0];

/// How deep a color may reference another before we call it a cycle.
const MAX_DEPTH: u32 = 8;

/// The live stylesheet. Behind an `RwLock` only because hot reload can replace it from the watcher
/// thread; reads are an `Arc` clone, and there are a handful per frame at most.
static THEME: LazyLock<RwLock<Arc<Theme>>> = LazyLock::new(|| RwLock::new(Arc::new(load())));

/// The stylesheet in force right now.
pub fn current() -> Arc<Theme> {
    THEME.read().expect("theme lock poisoned").clone()
}

/// Re-read `theme.toml` from the source tree and install it — the hot-reload path
/// ([`crate::hotstyle`]). On any error the live theme is left alone and the error is returned for
/// the caller to print; the window keeps showing the last stylesheet that worked.
#[cfg(debug_assertions)]
pub fn reload() -> Result<(), String> {
    let src =
        std::fs::read_to_string(SOURCE_PATH).map_err(|e| format!("{SOURCE_PATH}: {e}"))?;
    let theme = Theme::parse(&src)?;
    *THEME.write().expect("theme lock poisoned") = Arc::new(theme);
    Ok(())
}

/// The startup load: the source-tree copy in debug (so a tweak survives a restart without a
/// rebuild), the embedded copy otherwise. A malformed *embedded* stylesheet is a build-time bug —
/// the `embedded_stylesheet_is_valid` test below exists so it can't ship.
fn load() -> Theme {
    #[cfg(debug_assertions)]
    if let Ok(src) = std::fs::read_to_string(SOURCE_PATH) {
        match Theme::parse(&src) {
            Ok(t) => return t,
            // Not fatal: fall through to the copy we compiled in, which is known-good.
            Err(e) => eprintln!("fire: {e}\nfire: using the embedded stylesheet instead"),
        }
    }
    Theme::parse(EMBEDDED).expect("the embedded theme.toml is malformed")
}

// ---------------------------------------------------------------------------------------------
// The stylesheet
// ---------------------------------------------------------------------------------------------

/// `theme.toml`, parsed. Field names are the TOML keys; unknown keys are rejected, so a typo is a
/// load error rather than a value that silently keeps its old default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    pub font: Font,
    pub accent: Accent,
    pub colors: Modes,
    pub chrome: Chrome,
    pub form: Form,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Font {
    pub size: f32,
    /// Logical edge of a toolbar icon; the atlas is rastered at this × the DPI scale.
    pub icon_size: f32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accent {
    fallback: Hex,
    min_luminance: f32,
    max_luminance: f32,
    contrast_threshold: f32,
}

/// The per-mode token sets. Free-form: a stylesheet may invent tokens, and a color may name any of
/// them (plus the built-in `accent`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Modes {
    dark: Tokens,
    light: Tokens,
}

type Tokens = BTreeMap<String, Expr>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chrome {
    toolbar_h: f32,
    status_h: f32,
    transport_h: f32,
    edge_pad: f32,
    status_pad: f32,
    chip_offset_y: f32,
    divider_top: f32,
    divider_bottom: f32,
    empty_hint_line_gap: f32,
    geom: Geom,
    colors: ChromeColors,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Form {
    /// The settings window's opening size, as a fraction of the viewport.
    pub open_fraction: [f32; 2],
    geom: Geom,
    colors: FormColors,
}

/// The shape of a style: everything ImGui calls a "style var". Both styles carry a full set — that
/// is what makes them independent, and why the settings window can be a form while the chrome is a
/// toolbar.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Geom {
    window_padding: [f32; 2],
    frame_padding: [f32; 2],
    item_spacing: [f32; 2],
    item_inner_spacing: [f32; 2],
    indent_spacing: f32,
    scrollbar_size: f32,
    grab_min_size: f32,
    window_rounding: f32,
    child_rounding: f32,
    frame_rounding: f32,
    popup_rounding: f32,
    grab_rounding: f32,
    scrollbar_rounding: f32,
    tab_rounding: f32,
    tab_bar_overline: f32,
    window_border: f32,
    child_border: f32,
    frame_border: f32,
    popup_border: f32,
    image_border: f32,
}

/// Declares a block of colors: the struct serde fills, plus the list of `(key, expr)` pairs
/// [`Theme::parse`] walks to prove every one of them resolves before the stylesheet is accepted.
/// Writing the field list twice is what the macro exists to avoid — a color that isn't in
/// `entries()` is a color that a typo could smuggle past validation and onto the screen.
macro_rules! color_block {
    ($name:ident { $($field:ident),* $(,)? }) => {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct $name { $($field: Expr,)* }

        impl $name {
            fn entries(&self) -> Vec<(&'static str, &Expr)> {
                vec![$((stringify!($field), &self.$field),)*]
            }
        }
    };
}

color_block!(ChromeColors {
    text,
    text_disabled,
    window_bg,
    child_bg,
    popup_bg,
    border,
    border_shadow,
    separator,
    button,
    button_hovered,
    button_active,
    frame_bg,
    frame_bg_hovered,
    frame_bg_active,
    header,
    header_hovered,
    header_active,
    slider_grab,
    slider_grab_active,
    check_mark,
    scrollbar_bg,
    scrollbar_grab,
    scrollbar_grab_hovered,
    scrollbar_grab_active,
    // Not ImGui style colors — fire's own two fills (the status bar, and the GPU clear).
    status_bg,
    view_clear,
});

color_block!(FormColors {
    text,
    text_disabled,
    window_bg,
    popup_bg,
    child_bg,
    border,
    border_shadow,
    separator,
    title_bg,
    title_bg_active,
    title_bg_collapsed,
    frame_bg,
    frame_bg_hovered,
    frame_bg_active,
    button,
    button_hovered,
    button_active,
    check_mark,
    checkbox_selected_bg,
    slider_grab,
    slider_grab_active,
    header,
    header_hovered,
    header_active,
    nav_cursor,
    text_selected_bg,
    input_text_cursor,
    tab,
    tab_hovered,
    tab_selected,
    tab_selected_overline,
    tab_dimmed,
    tab_dimmed_selected,
    tab_dimmed_selected_overline,
    scrollbar_bg,
    scrollbar_grab,
    scrollbar_grab_hovered,
    scrollbar_grab_active,
    resize_grip,
    resize_grip_hovered,
    resize_grip_active,
});

impl Theme {
    /// Parse a stylesheet **and prove it**: every color in it is resolved, in both modes, against a
    /// stand-in accent. So `reload` either installs a stylesheet that will draw correctly or reports
    /// exactly which key is wrong — an unknown token name never reaches a widget.
    fn parse(src: &str) -> Result<Theme, String> {
        let theme: Theme = toml::from_str(src).map_err(|e| format!("theme.toml: {e}"))?;
        for dark in [true, false] {
            let mode = if dark { "colors.dark" } else { "colors.light" };
            let cx = Cx {
                tokens: theme.tokens(dark),
                accent: [0.0, 0.5, 1.0, 1.0], // stand-in: only *names* are being checked here
                threshold: theme.accent.contrast_threshold,
            };
            let blocks = [
                ("chrome.colors", theme.chrome.colors.entries()),
                ("form.colors", theme.form.colors.entries()),
            ];
            for (block, entries) in blocks {
                for (key, expr) in entries {
                    cx.resolve(expr, 0)
                        .map_err(|e| format!("theme.toml: {block}.{key} (with [{mode}]): {e}"))?;
                }
            }
        }
        Ok(theme)
    }

    fn tokens(&self, dark: bool) -> &Tokens {
        if dark {
            &self.colors.dark
        } else {
            &self.colors.light
        }
    }

    /// A resolver bound to one mode: the tokens of that mode, plus the live system accent.
    fn cx(&self, dark: bool) -> Cx<'_> {
        Cx {
            tokens: self.tokens(dark),
            accent: self.accent_rgba(),
            threshold: self.accent.contrast_threshold,
        }
    }

    /// The `accent` built-in: the user's Windows highlight color, or the stylesheet's fallback when
    /// that highlight is so dark or so bright that text on top of it would be unreadable (which
    /// includes some high-contrast themes).
    fn accent_rgba(&self) -> [f32; 4] {
        let c = crate::chrome::system_highlight();
        // COLORREF is 0x00BBGGRR.
        let (r, g, b) = (
            (c & 0xFF) as f32,
            ((c >> 8) & 0xFF) as f32,
            ((c >> 16) & 0xFF) as f32,
        );
        let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        if lum < self.accent.min_luminance || lum > self.accent.max_luminance {
            return self.accent.fallback.0;
        }
        [r / 255.0, g / 255.0, b / 255.0, 1.0]
    }
}

// ---------------------------------------------------------------------------------------------
// Physical metrics
// ---------------------------------------------------------------------------------------------

/// The chrome's stylesheet numbers, resolved for the current DPI — everything [`crate::ui`] needs to
/// lay a frame out. Rebuilt on a DPI change and on a hot reload; nothing caches it.
#[derive(Clone, Copy)]
pub struct Metrics {
    pub scale: f32,
    pub toolbar_h: f32,
    pub status_h: f32,
    pub transport_h: f32,
    /// Window edge → first/last toolbar button.
    pub edge_pad: f32,
    /// Window edge → status-bar text.
    pub status_pad: f32,
    /// Top of the image → the flipbook hint chip.
    pub chip_offset_y: f32,
    /// Padding inside a toolbar button (so a button is `icon + 2 × pad`, which is how ImGui sizes
    /// it — this must stay the same number the style is given, or the layout and the widgets
    /// disagree about how wide a button is).
    pub button_pad: [f32; 2],
    /// Gap between adjacent toolbar buttons.
    pub item_spacing: f32,
    /// The group divider's extent, as a fraction of the toolbar's height.
    pub divider_top: f32,
    pub divider_bottom: f32,
    /// Line spacing of the empty-state hint, as a multiple of the line height.
    pub empty_hint_line_gap: f32,
}

impl Metrics {
    pub fn new(dpi: u32) -> Self {
        let t = current();
        let c = &t.chrome;
        let scale = dpi.max(96) as f32 / 96.0;
        let s = |v: f32| (v * scale).round();
        Metrics {
            scale,
            toolbar_h: s(c.toolbar_h),
            status_h: s(c.status_h),
            transport_h: s(c.transport_h),
            edge_pad: s(c.edge_pad),
            status_pad: s(c.status_pad),
            chip_offset_y: s(c.chip_offset_y),
            button_pad: [s(c.geom.frame_padding[0]), s(c.geom.frame_padding[1])],
            item_spacing: s(c.geom.item_spacing[0]),
            divider_top: c.divider_top,
            divider_bottom: c.divider_bottom,
            empty_hint_line_gap: c.empty_hint_line_gap,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Applying it
// ---------------------------------------------------------------------------------------------

/// Apply the **chrome** style — the toolbar, status bar, transport, chip and popup menus. Called at
/// startup and again whenever the theme, the accent, the DPI or the stylesheet changes.
///
/// `scale` is the DPI factor. Every size in the stylesheet is logical px and is scaled here; ImGui's
/// `font_scale_dpi` covers glyphs only, so without this the chrome would stay 96-dpi-sized on a
/// HiDPI monitor.
pub fn apply(style: &mut Style, dark: bool, scale: f32) {
    let t = current();
    let cx = t.cx(dark);
    let c = &t.chrome.colors;

    style.set_font_size_base(t.font.size);
    style.set_font_scale_dpi(scale);
    geom(style, &t.chrome.geom, scale);

    style.set_color(StyleColor::Text, cx.c(&c.text));
    style.set_color(StyleColor::TextDisabled, cx.c(&c.text_disabled));
    style.set_color(StyleColor::WindowBg, cx.c(&c.window_bg));
    style.set_color(StyleColor::ChildBg, cx.c(&c.child_bg));
    style.set_color(StyleColor::PopupBg, cx.c(&c.popup_bg));
    style.set_color(StyleColor::Border, cx.c(&c.border));
    style.set_color(StyleColor::BorderShadow, cx.c(&c.border_shadow));
    style.set_color(StyleColor::Separator, cx.c(&c.separator));

    style.set_color(StyleColor::Button, cx.c(&c.button));
    style.set_color(StyleColor::ButtonHovered, cx.c(&c.button_hovered));
    style.set_color(StyleColor::ButtonActive, cx.c(&c.button_active));

    style.set_color(StyleColor::FrameBg, cx.c(&c.frame_bg));
    style.set_color(StyleColor::FrameBgHovered, cx.c(&c.frame_bg_hovered));
    style.set_color(StyleColor::FrameBgActive, cx.c(&c.frame_bg_active));

    style.set_color(StyleColor::Header, cx.c(&c.header));
    style.set_color(StyleColor::HeaderHovered, cx.c(&c.header_hovered));
    style.set_color(StyleColor::HeaderActive, cx.c(&c.header_active));

    style.set_color(StyleColor::SliderGrab, cx.c(&c.slider_grab));
    style.set_color(StyleColor::SliderGrabActive, cx.c(&c.slider_grab_active));
    style.set_color(StyleColor::CheckMark, cx.c(&c.check_mark));

    style.set_color(StyleColor::ScrollbarBg, cx.c(&c.scrollbar_bg));
    style.set_color(StyleColor::ScrollbarGrab, cx.c(&c.scrollbar_grab));
    style.set_color(StyleColor::ScrollbarGrabHovered, cx.c(&c.scrollbar_grab_hovered));
    style.set_color(StyleColor::ScrollbarGrabActive, cx.c(&c.scrollbar_grab_active));
}

/// Apply the **settings window's** style.
///
/// Deliberately not [`apply`], and it starts from a different base: ImGui's factory style (see
/// [`crate::render::imgui::FormStyle`]), which is a *form* geometry. The chrome's is a toolbar —
/// transparent buttons, tight spacing, tuned to sit over an image — and a dialog that inherited it
/// would have invisible buttons and inputs with no visible edges. Same palette, different shape.
pub fn form(style: &mut Style, dark: bool, scale: f32) {
    let t = current();
    let cx = t.cx(dark);
    let c = &t.form.colors;

    geom(style, &t.form.geom, scale);

    style.set_color(StyleColor::Text, cx.c(&c.text));
    style.set_color(StyleColor::TextDisabled, cx.c(&c.text_disabled));
    style.set_color(StyleColor::WindowBg, cx.c(&c.window_bg));
    style.set_color(StyleColor::PopupBg, cx.c(&c.popup_bg));
    style.set_color(StyleColor::ChildBg, cx.c(&c.child_bg));
    style.set_color(StyleColor::Border, cx.c(&c.border));
    style.set_color(StyleColor::BorderShadow, cx.c(&c.border_shadow));
    style.set_color(StyleColor::Separator, cx.c(&c.separator));

    style.set_color(StyleColor::TitleBg, cx.c(&c.title_bg));
    style.set_color(StyleColor::TitleBgActive, cx.c(&c.title_bg_active));
    style.set_color(StyleColor::TitleBgCollapsed, cx.c(&c.title_bg_collapsed));

    style.set_color(StyleColor::FrameBg, cx.c(&c.frame_bg));
    style.set_color(StyleColor::FrameBgHovered, cx.c(&c.frame_bg_hovered));
    style.set_color(StyleColor::FrameBgActive, cx.c(&c.frame_bg_active));

    style.set_color(StyleColor::Button, cx.c(&c.button));
    style.set_color(StyleColor::ButtonHovered, cx.c(&c.button_hovered));
    style.set_color(StyleColor::ButtonActive, cx.c(&c.button_active));

    style.set_color(StyleColor::CheckMark, cx.c(&c.check_mark));
    style.set_color(StyleColor::CheckboxSelectedBg, cx.c(&c.checkbox_selected_bg));
    style.set_color(StyleColor::SliderGrab, cx.c(&c.slider_grab));
    style.set_color(StyleColor::SliderGrabActive, cx.c(&c.slider_grab_active));
    style.set_color(StyleColor::Header, cx.c(&c.header));
    style.set_color(StyleColor::HeaderHovered, cx.c(&c.header_hovered));
    style.set_color(StyleColor::HeaderActive, cx.c(&c.header_active));
    style.set_color(StyleColor::NavCursor, cx.c(&c.nav_cursor));
    style.set_color(StyleColor::TextSelectedBg, cx.c(&c.text_selected_bg));
    style.set_color(StyleColor::InputTextCursor, cx.c(&c.input_text_cursor));

    style.set_color(StyleColor::Tab, cx.c(&c.tab));
    style.set_color(StyleColor::TabHovered, cx.c(&c.tab_hovered));
    style.set_color(StyleColor::TabSelected, cx.c(&c.tab_selected));
    style.set_color(StyleColor::TabSelectedOverline, cx.c(&c.tab_selected_overline));
    style.set_color(StyleColor::TabDimmed, cx.c(&c.tab_dimmed));
    style.set_color(StyleColor::TabDimmedSelected, cx.c(&c.tab_dimmed_selected));
    style.set_color(
        StyleColor::TabDimmedSelectedOverline,
        cx.c(&c.tab_dimmed_selected_overline),
    );

    style.set_color(StyleColor::ScrollbarBg, cx.c(&c.scrollbar_bg));
    style.set_color(StyleColor::ScrollbarGrab, cx.c(&c.scrollbar_grab));
    style.set_color(StyleColor::ScrollbarGrabHovered, cx.c(&c.scrollbar_grab_hovered));
    style.set_color(StyleColor::ScrollbarGrabActive, cx.c(&c.scrollbar_grab_active));

    style.set_color(StyleColor::ResizeGrip, cx.c(&c.resize_grip));
    style.set_color(StyleColor::ResizeGripHovered, cx.c(&c.resize_grip_hovered));
    style.set_color(StyleColor::ResizeGripActive, cx.c(&c.resize_grip_active));
}

/// The shape half of a style: paddings, spacings and roundings scaled to the monitor; border widths
/// left alone, because a border is a hairline and 1.5 physical px of hairline is a blur.
fn geom(style: &mut Style, g: &Geom, scale: f32) {
    let s = |v: f32| (v * scale).round();
    let s2 = |v: [f32; 2]| [s(v[0]), s(v[1])];

    style.set_window_padding(s2(g.window_padding));
    style.set_frame_padding(s2(g.frame_padding));
    style.set_item_spacing(s2(g.item_spacing));
    style.set_item_inner_spacing(s2(g.item_inner_spacing));
    style.set_indent_spacing(s(g.indent_spacing));
    style.set_scrollbar_size(s(g.scrollbar_size));
    style.set_grab_min_size(s(g.grab_min_size));

    style.set_window_rounding(s(g.window_rounding));
    style.set_child_rounding(s(g.child_rounding));
    style.set_frame_rounding(s(g.frame_rounding));
    style.set_popup_rounding(s(g.popup_rounding));
    style.set_grab_rounding(s(g.grab_rounding));
    style.set_scrollbar_rounding(s(g.scrollbar_rounding));
    style.set_tab_rounding(s(g.tab_rounding));
    style.set_tab_bar_overline_size(s(g.tab_bar_overline));

    style.set_window_border_size(g.window_border);
    style.set_child_border_size(g.child_border);
    style.set_frame_border_size(g.frame_border);
    style.set_popup_border_size(g.popup_border);
    style.set_image_border_size(g.image_border);
}

/// The status bar's fill — its own window, one step deeper than the toolbar's.
pub fn status_bg(dark: bool) -> [f32; 4] {
    let t = current();
    t.cx(dark).c(&t.chrome.colors.status_bg)
}

/// The chrome fill, used to clear the parts of the backbuffer the image doesn't cover.
pub fn chrome_bg(dark: bool) -> [f32; 4] {
    let t = current();
    t.cx(dark).c(&t.chrome.colors.window_bg)
}

/// The viewport backdrop (letterbox / no image), as the `0x00RRGGBB` the GPU surface's clear wants.
pub fn view_clear_packed(dark: bool) -> u32 {
    let t = current();
    let c = t.cx(dark).c(&t.chrome.colors.view_clear);
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (q(c[0]) << 16) | (q(c[1]) << 8) | q(c[2])
}

/// Black or white, whichever stays readable **on** `c` — the tint for an icon sitting on an
/// accent-filled button, and the same rule the stylesheet's `contrast(…)` uses.
pub(crate) fn on_accent(c: [f32; 4]) -> [f32; 4] {
    contrast(c, current().accent.contrast_threshold)
}

// ---------------------------------------------------------------------------------------------
// The color grammar
// ---------------------------------------------------------------------------------------------

/// One color from the stylesheet, parsed but not yet resolved: it may still name a token, or be
/// derived from one. See the grammar at the top of `theme.toml`.
#[derive(Debug, Clone)]
enum Expr {
    Lit([f32; 4]),
    /// A token from the current mode's block — or the built-in `accent`.
    Name(String),
    Lift(Box<Expr>, f32),
    Alpha(Box<Expr>, f32),
    Contrast(Box<Expr>),
}

/// A literal color, for the one place an expression makes no sense (the accent's own fallback).
#[derive(Debug, Deserialize)]
#[serde(try_from = "String")]
struct Hex([f32; 4]);

impl TryFrom<String> for Hex {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        let h = s
            .strip_prefix('#')
            .ok_or_else(|| format!("`{s}` must be a literal color, e.g. \"#0078d7\""))?;
        parse_hex(h).map(Hex)
    }
}

impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_color(&s).map_err(serde::de::Error::custom)
    }
}

/// Resolves an [`Expr`] to RGBA, in one mode, against one accent.
struct Cx<'a> {
    tokens: &'a Tokens,
    accent: [f32; 4],
    threshold: f32,
}

impl Cx<'_> {
    /// Infallible resolve, for the draw path: the stylesheet was proved at parse time, so a failure
    /// here is impossible and paints magenta if it somehow happens anyway.
    fn c(&self, e: &Expr) -> [f32; 4] {
        self.resolve(e, 0).unwrap_or(UNRESOLVED)
    }

    fn resolve(&self, e: &Expr, depth: u32) -> Result<[f32; 4], String> {
        if depth > MAX_DEPTH {
            return Err("color references itself (or nests more than 8 deep)".into());
        }
        let sub = |e: &Expr| self.resolve(e, depth + 1);
        Ok(match e {
            Expr::Lit(c) => *c,
            Expr::Name(n) if n == "accent" => self.accent,
            Expr::Name(n) => {
                let e = self
                    .tokens
                    .get(n)
                    .ok_or_else(|| format!("unknown color token `{n}`"))?;
                self.resolve(e, depth + 1)?
            }
            Expr::Lift(e, amount) => lift(sub(e)?, *amount),
            Expr::Alpha(e, a) => {
                let mut c = sub(e)?;
                c[3] = *a;
                c
            }
            Expr::Contrast(e) => contrast(sub(e)?, self.threshold),
        })
    }
}

/// Nudge a color toward white — or, if it is already bright, toward black. The "one step more
/// prominent" a hover needs, without a second hand-picked color per token (and without knowing, at
/// authoring time, what the user's accent is).
fn lift(c: [f32; 4], amount: f32) -> [f32; 4] {
    let dir = if luminance(c) > 0.5 { -1.0 } else { 1.0 };
    let f = |v: f32| (v + dir * amount).clamp(0.0, 1.0);
    [f(c[0]), f(c[1]), f(c[2]), c[3]]
}

/// Black or white, whichever stays readable on `c`.
fn contrast(c: [f32; 4], threshold: f32) -> [f32; 4] {
    if luminance(c) > threshold {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        [1.0, 1.0, 1.0, 1.0]
    }
}

fn luminance(c: [f32; 4]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn parse_color(s: &str) -> Result<Expr, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex).map(Expr::Lit);
    }
    if let Some((head, args)) = as_call(s) {
        let args = split_args(args);
        return match (head, args.len()) {
            ("lift", 2) => Ok(Expr::Lift(
                Box::new(parse_color(args[0])?),
                parse_amount(args[1])?,
            )),
            ("alpha", 2) => Ok(Expr::Alpha(
                Box::new(parse_color(args[0])?),
                parse_amount(args[1])?,
            )),
            ("contrast", 1) => Ok(Expr::Contrast(Box::new(parse_color(args[0])?))),
            ("lift" | "alpha", n) => Err(format!("`{head}` takes a color and an amount, got {n}")),
            ("contrast", n) => Err(format!("`contrast` takes one color, got {n}")),
            _ => Err(format!("unknown color function `{head}`")),
        };
    }
    if s == "none" {
        return Ok(Expr::Lit([0.0; 4]));
    }
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Ok(Expr::Name(s.to_string()));
    }
    Err(format!(
        "`{s}` is not a color, a token name, or one of lift()/alpha()/contrast()"
    ))
}

/// `f(args)` → `("f", "args")`.
fn as_call(s: &str) -> Option<(&str, &str)> {
    let open = s.find('(')?;
    let inner = s.strip_suffix(')')?;
    Some((s[..open].trim(), inner[open + 1..].trim()))
}

/// Split on top-level commas, so a nested call keeps its own arguments.
fn split_args(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0u32, 0usize);
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

fn parse_amount(s: &str) -> Result<f32, String> {
    s.parse::<f32>()
        .map_err(|_| format!("`{s}` is not a number"))
}

/// `rgb`, `rrggbb` or `rrggbbaa`, without the `#`.
fn parse_hex(h: &str) -> Result<[f32; 4], String> {
    let bad = || format!("`#{h}` is not a #rgb, #rrggbb or #rrggbbaa color");
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| bad());
    let f = |v: u8| v as f32 / 255.0;
    match h.len() {
        3 => {
            let nib = |i: usize| {
                u8::from_str_radix(&h[i..i + 1], 16)
                    .map(|v| v * 17) // 0xa → 0xaa
                    .map_err(|_| bad())
            };
            Ok([f(nib(0)?), f(nib(1)?), f(nib(2)?), 1.0])
        }
        6 => Ok([f(byte(0)?), f(byte(2)?), f(byte(4)?), 1.0]),
        8 => Ok([f(byte(0)?), f(byte(2)?), f(byte(4)?), f(byte(6)?)]),
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one that matters: a release build `expect`s this file to parse, and every color in it to
    /// resolve. If someone renames a token and misses a use, this fails instead of the exe.
    #[test]
    fn embedded_stylesheet_is_valid() {
        Theme::parse(EMBEDDED).expect("theme.toml");
    }

    /// The source-tree copy is the one a debug build actually loads, and it is the file a designer
    /// edits — so it is checked too, not just the snapshot compiled in.
    #[cfg(debug_assertions)]
    #[test]
    fn source_stylesheet_is_valid() {
        let src = std::fs::read_to_string(SOURCE_PATH).expect("theme.toml on disk");
        Theme::parse(&src).expect("theme.toml");
    }

    fn cx<'a>(tokens: &'a Tokens, accent: [f32; 4]) -> Cx<'a> {
        Cx {
            tokens,
            accent,
            threshold: 0.59,
        }
    }

    #[test]
    fn colors_parse_and_resolve() {
        let mut tokens = Tokens::new();
        tokens.insert("surface".into(), parse_color("#2b2b2b").unwrap());
        tokens.insert("hover".into(), parse_color("lift(surface, 0.1)").unwrap());
        let cx = cx(&tokens, [0.0, 0.47, 0.84, 1.0]);

        let c = |s: &str| cx.resolve(&parse_color(s).unwrap(), 0).unwrap();

        assert_eq!(c("#fff"), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(c("#000000"), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(c("none"), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(c("accent"), [0.0, 0.47, 0.84, 1.0]);
        assert_eq!(c("alpha(accent, 0.5)")[3], 0.5);
        // A token that is itself derived from another token.
        assert!(c("hover")[0] > c("surface")[0]);
        // Dark stays dark under a lift; white is already bright, so it lifts *down*.
        assert!(c("lift(#000000, 0.2)")[0] > 0.0);
        assert!(c("lift(#ffffff, 0.2)")[0] < 1.0);
        // Readable text on a pale accent is black, on a dark one white.
        assert_eq!(c("contrast(#ffff00)"), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(c("contrast(#101010)"), [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn bad_colors_are_rejected_at_parse() {
        assert!(parse_color("#12345").is_err());
        assert!(parse_color("rgb(1,2,3)").is_err());
        assert!(parse_color("lift(#fff)").is_err());
        assert!(parse_color("lift(#fff, blue)").is_err());
        assert!(parse_color("hot pink").is_err());
    }

    #[test]
    fn an_unknown_token_is_an_error_not_a_magenta_widget() {
        let tokens = Tokens::new();
        let cx = cx(&tokens, [0.0; 4]);
        let e = cx.resolve(&parse_color("surfce").unwrap(), 0).unwrap_err();
        assert!(e.contains("surfce"), "{e}");
    }

    #[test]
    fn a_cycle_terminates() {
        let mut tokens = Tokens::new();
        tokens.insert("a".into(), parse_color("b").unwrap());
        tokens.insert("b".into(), parse_color("a").unwrap());
        let cx = cx(&tokens, [0.0; 4]);
        assert!(cx.resolve(&parse_color("a").unwrap(), 0).is_err());
    }

    #[test]
    fn metrics_scale_with_dpi() {
        let lo = Metrics::new(96);
        let hi = Metrics::new(192);
        assert_eq!(hi.scale, 2.0);
        assert_eq!(hi.toolbar_h, lo.toolbar_h * 2.0);
        assert_eq!(hi.edge_pad, lo.edge_pad * 2.0);
        // Unitless fractions must *not* scale.
        assert_eq!(hi.divider_top, lo.divider_top);
    }
}
