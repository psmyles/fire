//! The flipbook transport's *vocabulary*: what the band can show and what it can ask for.
//!
//! The band itself is drawn by [`crate::ui`] in ImGui. What used to live here — a hand-rolled
//! layout, hit-test, hover/drag state machine, and a typed-number-field editor, all painted with
//! GDI — is gone: sliders, numeric fields and checkboxes are solved widgets, and reimplementing them
//! is exactly the kind of work that produced the bugs this migration exists to kill.
//!
//! What remains is pure data with no Win32 in it: the state the band renders from, and the edits it
//! requests. The win shell applies them to the live [`crate::flipbook::FlipbookState`].

/// A change the band requests; the win shell applies it to the active flipbook state.
#[derive(Debug, Clone, Copy)]
pub enum TransportEdit {
    SetCols(u32),
    SetRows(u32),
    SetCount(u32),
    SetFps(f32),
    ToggleBlend,
    TogglePlay,
    /// Stop playback. **Idempotent, and that is the point** — it is emitted for as long as the user
    /// holds the scrub bar (see [`crate::ui`]), which is every frame of a drag, so a `TogglePlay`
    /// there would flicker between playing and paused instead of pausing.
    Pause,
    /// Fractional playback position from the slider (snapped to an integer when blend is off).
    Scrub(f32),
}

/// Read-only state the band renders from (built by the win shell from the active flipbook state).
#[derive(Debug, Clone, Copy)]
pub struct TransportSnapshot {
    pub cols: u32,
    pub rows: u32,
    pub frame_count: u32,
    pub fps: f32,
    pub blend: bool,
    pub playing: bool,
    pub frame_pos: f32,
    pub grid_max: u32,
}
