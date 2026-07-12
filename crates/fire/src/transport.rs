//! The flipbook transport band: a hand-painted GDI strip above the status bar with the
//! sprite-sheet controls (grid cols×rows, frame count, play/pause, a scrub slider with a
//! "frame / total" readout, fps, and a blend toggle). Shown only in flipbook mode.
//!
//! Fire deliberately uses **no** Win32 common controls (dark-mode reasons, see [`crate::chrome`]),
//! so every widget is painted and hit-tested here. Numeric fields support three inputs: horizontal
//! click-**drag** and mouse **wheel** for quick nudges, and click-to-type (a caret + [`WM_CHAR`]
//! buffer) for exact values — fire's first typed-input widget. The win shell ([`crate::win`]) owns
//! the mouse/keyboard plumbing (capture, `WM_CHAR` routing) and applies the [`TransportEdit`]s this
//! module returns to the per-path [`crate::flipbook::FlipbookState`].

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, HDC, TRANSPARENT,
};

use crate::chrome::{draw_text, fill, text_width, Chrome};
use crate::flipbook::{FPS_MAX, FPS_MIN};
use crate::icons::Icon;

/// The transport widgets, left→right. Each maps to a laid-out rect for hit-testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widget {
    Cols,
    Rows,
    Count,
    PlayPause,
    Slider,
    Fps,
    Blend,
}

/// A change the band requests; the win shell applies it to the active [`FlipbookState`].
#[derive(Debug, Clone, Copy)]
pub enum TransportEdit {
    SetCols(u32),
    SetRows(u32),
    SetCount(u32),
    SetFps(f32),
    ToggleBlend,
    TogglePlay,
    /// Fractional playback position from the slider (already snapped to an integer when blend off).
    Scrub(f32),
}

/// Read-only state the band paints from (built by the win shell from the active flipbook state).
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

/// The outcome of a press in the band: an optional immediate edit (a toggle or a slider seek) and
/// whether the win shell should `SetCapture` for a drag.
pub struct Press {
    pub edit: Option<TransportEdit>,
    pub capture: bool,
    /// True when a slider drag started — grabbing the playhead stops playback (the shell pauses a
    /// playing flipbook and leaves it parked where the scrub ends).
    pub slider: bool,
}

/// Drag pixels per unit change: integer fields step 1 per 8 px; fps steps 0.5 per 4 px (= 1 fps
/// per 8 px). Small ranges, so this reaches the ends quickly without feeling twitchy.
const FIELD_PX_PER_UNIT: f64 = 8.0;
const FPS_PX_PER_UNIT: f64 = 8.0;
/// A press that releases within this many px (no real drag) enters type-to-edit mode instead.
const CLICK_SLOP: i32 = 3;

/// Slider metrics (96-dpi logical px, DPI-scaled at layout/paint). The thumb is a grab handle, not
/// a hairline: half-width `THUMB_HALF_W` and half-height `THUMB_HALF_H` about the track's midline.
const THUMB_HALF_W: i32 = 5;
const THUMB_HALF_H: i32 = 8;
/// Half-thickness of the track groove.
const TRACK_HALF_H: i32 = 2;
/// Edge of the blend checkbox's square.
const CHECK_BOX: i32 = 16;

#[derive(Clone, Copy)]
enum Drag {
    None,
    Slider,
    Field {
        widget: Widget,
        anchor_x: i32,
        start: f64,
        moved: bool,
    },
}

/// An in-progress typed edit of one field: the digits so far (caret is always at the end).
struct FieldEdit {
    widget: Widget,
    buffer: String,
}

/// Transport band state: the laid-out widget rects, hover, and the active drag / typed edit.
pub struct Transport {
    rects: Vec<(Widget, RECT)>,
    /// The slider's track (the groove the thumb rides); a sub-rect of the Slider widget rect.
    slider_track: RECT,
    /// The thumb's half-width at the current DPI. The thumb's *centre* travels the track inset by
    /// this much at each end (so it never hangs off the groove), which is also the value ↔ x
    /// mapping [`Self::scrub_at`] inverts — kept here so the mapping needs no `Chrome`.
    thumb_half: i32,
    hover: Option<Widget>,
    drag: Drag,
    editing: Option<FieldEdit>,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            rects: Vec::new(),
            slider_track: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            thumb_half: THUMB_HALF_W,
            hover: None,
            drag: Drag::None,
            editing: None,
        }
    }
}

impl Transport {
    /// Lay the widgets out within `band` (frame-client coords) for the current metrics. Left group
    /// (grid · count · play) packs from the left, the fps/blend group from the right, and the
    /// slider fills the middle with a fixed-width "frame / total" readout at its right end.
    pub fn layout(&mut self, band: RECT, chrome: &Chrome) {
        self.rects.clear();
        let m = &chrome.metrics;
        let margin = m.scale(8);
        let gap = m.scale(6);
        let fw = m.scale(34); // numeric field width
        let cw = m.scale(44); // count field (wider — up to 4096)
        let fpsw = m.scale(40);
        let btn = m.scale(22); // play/pause square
        let sep = m.scale(10);
        let readout_w = m.scale(64);
        let cy = (band.top + band.bottom) / 2;
        let fh = m.scale(20); // field height
        let field = |x: i32, w: i32| RECT {
            left: x,
            top: cy - fh / 2,
            right: x + w,
            bottom: cy + fh / 2,
        };

        // Left group: Cols × Rows | Count | Play/Pause.
        let mut x = band.left + margin;
        self.rects.push((Widget::Cols, field(x, fw)));
        x += fw + text_span("\u{00d7}", chrome) + gap; // room for the × label between fields
        self.rects.push((Widget::Rows, field(x, fw)));
        x += fw + gap + sep;
        self.rects.push((Widget::Count, field(x, cw)));
        x += cw + gap + sep;
        let pp = RECT {
            left: x,
            top: cy - btn / 2,
            right: x + btn,
            bottom: cy + btn / 2,
        };
        self.rects.push((Widget::PlayPause, pp));
        x += btn + gap;
        let left_end = x;

        // Right group (packed right→left): Blend checkbox, then Fps.
        let mut rx = band.right - margin;
        let blend_label_w = text_span("Blend", chrome);
        let check = m.scale(CHECK_BOX);
        let blend_w = check + m.scale(6) + blend_label_w;
        // The box is small, so the checkbox's hit rect spans the band's full height (and its label,
        // which toggles too) rather than just the box.
        self.rects.push((
            Widget::Blend,
            RECT {
                left: rx - blend_w,
                top: band.top + 1,
                right: rx,
                bottom: band.bottom,
            },
        ));
        rx -= blend_w + sep;
        let fps_label_w = text_span("fps", chrome);
        rx -= fps_label_w;
        self.rects.push((Widget::Fps, field(rx - fpsw, fpsw)));
        rx -= fpsw + gap;

        // Slider fills the middle; the readout sits at its right end.
        let slider_left = left_end + gap;
        let readout_left = (rx - gap - readout_w).max(slider_left);
        let track = RECT {
            left: slider_left,
            top: cy - fh / 2,
            right: (readout_left - gap).max(slider_left + 1),
            bottom: cy + fh / 2,
        };
        self.slider_track = track;
        self.thumb_half = m.scale(THUMB_HALF_W);
        // The thumb is a few px wide but the *target* is the whole band-height strip over the
        // track (plus a thumb's half-width of slack at each end, so the first/last frame are
        // reachable without pixel-hunting). The readout is deliberately outside it: clicking the
        // "37 / 64" text should not seek to the last frame.
        self.rects.push((
            Widget::Slider,
            RECT {
                left: (track.left - self.thumb_half).max(band.left),
                top: band.top + 1,
                right: track.right + self.thumb_half,
                bottom: band.bottom,
            },
        ));
    }

    /// Paint the band into `band` (frame-client coords).
    pub fn paint(&self, hdc: HDC, band: RECT, chrome: &Chrome, snap: &TransportSnapshot) {
        let p = chrome.palette();
        let m = &chrome.metrics;
        fill(hdc, &band, p.toolbar_bg);
        // A hairline top border, matching the status bar.
        fill(
            hdc,
            &RECT {
                left: band.left,
                top: band.top,
                right: band.right,
                bottom: band.top + 1,
            },
            p.border,
        );

        let prev = unsafe { SelectObject(hdc, m.font as _) };
        unsafe { SetBkMode(hdc, TRANSPARENT as i32) };

        // The × between the grid fields.
        if let (Some(cols_r), Some(rows_r)) = (self.rect(Widget::Cols), self.rect(Widget::Rows)) {
            let mut xr = RECT {
                left: cols_r.right,
                top: cols_r.top,
                right: rows_r.left,
                bottom: cols_r.bottom,
            };
            unsafe { SetTextColor(hdc, p.text_dim) };
            draw_text(
                hdc,
                "\u{00d7}",
                &mut xr,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }

        for &(w, r) in &self.rects {
            match w {
                Widget::Cols => self.paint_field(hdc, chrome, r, w, &snap.cols.to_string()),
                Widget::Rows => self.paint_field(hdc, chrome, r, w, &snap.rows.to_string()),
                Widget::Count => self.paint_field(hdc, chrome, r, w, &snap.frame_count.to_string()),
                Widget::Fps => {
                    self.paint_field(hdc, chrome, r, w, &fmt_fps(snap.fps));
                    // "fps" label just to the right of the field.
                    let mut lr = RECT {
                        left: r.right,
                        top: r.top,
                        right: r.right + text_span("fps", chrome) + m.scale(4),
                        bottom: r.bottom,
                    };
                    unsafe { SetTextColor(hdc, p.text_dim) };
                    draw_text(
                        hdc,
                        " fps",
                        &mut lr,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }
                Widget::PlayPause => {
                    let hovered = self.hover == Some(w);
                    if hovered {
                        fill(hdc, &r, p.btn_hover);
                    }
                    let icon = if snap.playing {
                        Icon::Pause
                    } else {
                        Icon::Play
                    };
                    chrome.icons().draw(
                        hdc,
                        icon,
                        (r.left + r.right) / 2,
                        (r.top + r.bottom) / 2,
                        p.text,
                    );
                }
                Widget::Slider => self.paint_slider(hdc, chrome, snap),
                Widget::Blend => self.paint_blend(hdc, chrome, r, snap.blend),
            }
        }
        unsafe { SelectObject(hdc, prev) };
    }

    fn paint_field(&self, hdc: HDC, chrome: &Chrome, r: RECT, w: Widget, value: &str) {
        let p = chrome.palette();
        // Field box: hover/edit highlight, hairline border.
        let editing = self.editing.as_ref().is_some_and(|e| e.widget == w);
        let bg = if editing || self.hover == Some(w) {
            p.btn_hover
        } else {
            p.status_bg
        };
        fill(hdc, &r, bg);
        outline(hdc, &r, p.separator);

        let shown = if editing {
            self.editing
                .as_ref()
                .map(|e| e.buffer.clone())
                .unwrap_or_default()
        } else {
            value.to_string()
        };
        let mut tr = RECT {
            left: r.left + 2,
            top: r.top,
            right: r.right - 2,
            bottom: r.bottom,
        };
        unsafe { SetTextColor(hdc, p.text) };
        draw_text(
            hdc,
            &shown,
            &mut tr,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        if editing {
            // A static caret bar just right of the text (no blink timer).
            let tw = text_width(hdc, &shown);
            let cx = ((r.left + r.right) / 2 + tw / 2 + 1).min(r.right - 2);
            fill(
                hdc,
                &RECT {
                    left: cx,
                    top: r.top + 3,
                    right: cx + 1,
                    bottom: r.bottom - 3,
                },
                p.text,
            );
        }
    }

    fn paint_slider(&self, hdc: HDC, chrome: &Chrome, snap: &TransportSnapshot) {
        let p = chrome.palette();
        let m = &chrome.metrics;
        let t = self.slider_track;
        let mid = (t.top + t.bottom) / 2;
        let groove = m.scale(TRACK_HALF_H).max(1);
        // The groove, then the played portion over it.
        fill(
            hdc,
            &RECT {
                left: t.left,
                top: mid - groove,
                right: t.right,
                bottom: mid + groove,
            },
            p.separator,
        );
        let count = snap.frame_count.max(1);
        let frac = if count > 1 {
            (snap.frame_pos / count as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_x = self.thumb_x(frac);
        fill(
            hdc,
            &RECT {
                left: t.left,
                top: mid - groove,
                right: thumb_x,
                bottom: mid + groove,
            },
            p.btn_active,
        );
        // The thumb: a real grab handle, accented while hovered or dragged, with a 1px cutout in
        // the band color so it reads as a separate object sitting on the groove.
        let dragging = matches!(self.drag, Drag::Slider);
        let hw = self.thumb_half;
        let hh = m.scale(THUMB_HALF_H);
        let thumb = RECT {
            left: thumb_x - hw,
            top: mid - hh,
            right: thumb_x + hw,
            bottom: mid + hh,
        };
        fill(
            hdc,
            &thumb,
            if dragging || self.hover == Some(Widget::Slider) {
                p.btn_active
            } else {
                p.text
            },
        );
        outline(hdc, &thumb, p.toolbar_bg);
        // Readout "frame / total" (1-based), fixed width to the right of the track.
        let frame_1 = (snap.frame_pos.floor() as u32 + 1).min(count);
        let text = format!("{frame_1} / {count}");
        let mut rr = RECT {
            left: t.right + chrome.metrics.scale(6),
            top: t.top,
            right: t.right + chrome.metrics.scale(70),
            bottom: t.bottom,
        };
        unsafe { SetTextColor(hdc, p.text_dim) };
        draw_text(
            hdc,
            &text,
            &mut rr,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }

    fn paint_blend(&self, hdc: HDC, chrome: &Chrome, r: RECT, on: bool) {
        let p = chrome.palette();
        let check = chrome.metrics.scale(CHECK_BOX);
        let cy = (r.top + r.bottom) / 2;
        let box_r = RECT {
            left: r.left,
            top: cy - check / 2,
            right: r.left + check,
            bottom: cy + check / 2,
        };
        let hovered = self.hover == Some(Widget::Blend);
        fill(
            hdc,
            &box_r,
            if on {
                p.btn_active
            } else if hovered {
                p.btn_hover
            } else {
                p.status_bg
            },
        );
        outline(hdc, &box_r, if on { p.btn_active } else { p.separator });
        if on {
            // The tick comes from the same anti-aliased icon pipeline as the toolbar glyphs; the
            // mask is transparent outside the stroke, so drawing it at the icon size (larger than
            // the box) still only marks pixels inside.
            chrome.icons().draw(
                hdc,
                Icon::Check,
                (box_r.left + box_r.right) / 2,
                cy,
                p.btn_active_text,
            );
        }
        let mut lr = RECT {
            left: box_r.right + chrome.metrics.scale(6),
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        };
        unsafe { SetTextColor(hdc, p.text) };
        draw_text(
            hdc,
            "Blend",
            &mut lr,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }

    // --- hit-testing + hover ---------------------------------------------------

    fn rect(&self, w: Widget) -> Option<RECT> {
        self.rects.iter().find(|(x, _)| *x == w).map(|(_, r)| *r)
    }

    /// The widget under a point (frame-client coords), if any.
    pub fn hit(&self, x: i32, y: i32) -> Option<Widget> {
        self.rects
            .iter()
            .find(|(_, r)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(w, _)| *w)
    }

    /// Update hover to the widget at `(x, y)`; returns whether it changed (→ repaint).
    pub fn set_hover(&mut self, x: i32, y: i32) -> bool {
        let h = self.hit(x, y);
        if h != self.hover {
            self.hover = h;
            true
        } else {
            false
        }
    }

    /// Clear hover (cursor left the band); returns whether it changed.
    pub fn clear_hover(&mut self) -> bool {
        if self.hover.is_some() {
            self.hover = None;
            true
        } else {
            false
        }
    }

    // --- input -----------------------------------------------------------------

    pub fn is_dragging(&self) -> bool {
        !matches!(self.drag, Drag::None)
    }

    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Begin an interaction at `(x, y)`. Toggles fire immediately; the slider seeks and captures;
    /// a field starts a drag (which becomes a type-edit if released without moving).
    pub fn press(&mut self, x: i32, y: i32, snap: &TransportSnapshot) -> Press {
        let none = Press {
            edit: None,
            capture: false,
            slider: false,
        };
        let Some(w) = self.hit(x, y) else { return none };
        match w {
            Widget::PlayPause => Press {
                edit: Some(TransportEdit::TogglePlay),
                capture: false,
                slider: false,
            },
            Widget::Blend => Press {
                edit: Some(TransportEdit::ToggleBlend),
                capture: false,
                slider: false,
            },
            Widget::Slider => {
                self.drag = Drag::Slider;
                Press {
                    edit: Some(self.scrub_at(x, snap)),
                    capture: true,
                    slider: true,
                }
            }
            Widget::Cols | Widget::Rows | Widget::Count | Widget::Fps => {
                self.drag = Drag::Field {
                    widget: w,
                    anchor_x: x,
                    start: field_value(w, snap),
                    moved: false,
                };
                Press {
                    edit: None,
                    capture: true,
                    slider: false,
                }
            }
        }
    }

    /// Continue a drag to `x`; returns the resulting edit (if any).
    pub fn drag_to(&mut self, x: i32, snap: &TransportSnapshot) -> Option<TransportEdit> {
        match self.drag {
            Drag::Slider => Some(self.scrub_at(x, snap)),
            Drag::Field {
                widget,
                anchor_x,
                start,
                ..
            } => {
                let dx = (x - anchor_x) as f64;
                if dx.abs() as i32 > CLICK_SLOP {
                    if let Drag::Field { moved, .. } = &mut self.drag {
                        *moved = true;
                    }
                }
                let ppu = if widget == Widget::Fps {
                    FPS_PX_PER_UNIT
                } else {
                    FIELD_PX_PER_UNIT
                };
                let step = if widget == Widget::Fps { 0.5 } else { 1.0 };
                let raw = start + dx / ppu; // ppu already encodes the per-pixel rate
                let snapped = (raw / step).round() * step;
                Some(field_edit(widget, snapped, snap))
            }
            Drag::None => None,
        }
    }

    /// End a drag. If a field press never moved beyond the slop, enter type-to-edit mode.
    pub fn release(&mut self) {
        if let Drag::Field { widget, moved, .. } = self.drag {
            if !moved {
                self.editing = Some(FieldEdit {
                    widget,
                    buffer: String::new(),
                });
            }
        }
        self.drag = Drag::None;
    }

    /// Cancel an in-progress drag without applying anything (e.g. a DPI change mid-drag).
    pub fn cancel_drag(&mut self) {
        self.drag = Drag::None;
    }

    /// Mouse wheel over the band: nudge the field under the cursor (±1, Ctrl → ±0.1 fps), or step
    /// the frame on the slider. `notches` is the signed wheel delta in detents.
    pub fn wheel(
        &mut self,
        x: i32,
        y: i32,
        notches: i32,
        ctrl: bool,
        snap: &TransportSnapshot,
    ) -> Option<TransportEdit> {
        let w = self.hit(x, y)?;
        match w {
            Widget::Fps => {
                let step = if ctrl { 0.1 } else { 1.0 };
                Some(field_edit(
                    Widget::Fps,
                    snap.fps as f64 + notches as f64 * step,
                    snap,
                ))
            }
            Widget::Cols | Widget::Rows | Widget::Count => {
                Some(field_edit(w, field_value(w, snap) + notches as f64, snap))
            }
            Widget::Slider => {
                let pos = (snap.frame_pos.round() + notches as f32)
                    .rem_euclid(snap.frame_count.max(1) as f32);
                Some(TransportEdit::Scrub(pos.floor()))
            }
            _ => None,
        }
    }

    /// Feed a typed character to the active field edit (digits, `.` for fps, Backspace = `\u{8}`).
    /// Returns whether the buffer changed (→ repaint). Non-editing or invalid chars are ignored.
    pub fn type_char(&mut self, ch: char) -> bool {
        let Some(e) = self.editing.as_mut() else {
            return false;
        };
        if ch == '\u{8}' {
            return e.buffer.pop().is_some();
        }
        let allow_dot = e.widget == Widget::Fps;
        if ch.is_ascii_digit() || (allow_dot && ch == '.' && !e.buffer.contains('.')) {
            // Keep the buffer bounded (max 4096 grids / 120 fps).
            if e.buffer.len() < 6 {
                e.buffer.push(ch);
                return true;
            }
        }
        false
    }

    /// Commit the active typed edit: parse the buffer and return the resulting edit (clamped). An
    /// empty/invalid buffer commits nothing. Clears edit mode either way.
    pub fn commit(&mut self, snap: &TransportSnapshot) -> Option<TransportEdit> {
        let e = self.editing.take()?;
        if e.buffer.is_empty() {
            return None;
        }
        if e.widget == Widget::Fps {
            e.buffer
                .parse::<f64>()
                .ok()
                .map(|v| field_edit(Widget::Fps, v, snap))
        } else {
            e.buffer
                .parse::<f64>()
                .ok()
                .map(|v| field_edit(e.widget, v, snap))
        }
    }

    /// Abandon the active typed edit without applying it (Esc). Returns whether one was active.
    pub fn cancel_edit(&mut self) -> bool {
        self.editing.take().is_some()
    }

    /// The thumb's centre x for a 0..1 position along the track (the inverse of [`Self::scrub_at`]).
    fn thumb_x(&self, frac: f32) -> i32 {
        let (lo, hi) = self.thumb_travel();
        lo + (frac * (hi - lo) as f32) as i32
    }

    /// The x range the thumb's centre sweeps: the track inset by the thumb's half-width at each
    /// end, so the thumb stays on the groove at 0 and 1.
    fn thumb_travel(&self) -> (i32, i32) {
        let t = self.slider_track;
        let lo = t.left + self.thumb_half;
        (lo, (t.right - self.thumb_half).max(lo + 1))
    }

    fn scrub_at(&self, x: i32, snap: &TransportSnapshot) -> TransportEdit {
        let (lo, hi) = self.thumb_travel();
        let frac = ((x - lo) as f32 / (hi - lo) as f32).clamp(0.0, 1.0);
        let count = snap.frame_count.max(1) as f32;
        // Map to [0, count); when blend is off, snap to an integer frame.
        let raw = (frac * count).min(count - 1e-3).max(0.0);
        let pos = if snap.blend { raw } else { raw.floor() };
        TransportEdit::Scrub(pos)
    }
}

/// The current numeric value of a field as f64.
fn field_value(w: Widget, snap: &TransportSnapshot) -> f64 {
    match w {
        Widget::Cols => snap.cols as f64,
        Widget::Rows => snap.rows as f64,
        Widget::Count => snap.frame_count as f64,
        Widget::Fps => snap.fps as f64,
        _ => 0.0,
    }
}

/// Build a clamped edit for a field from a raw value.
fn field_edit(w: Widget, v: f64, snap: &TransportSnapshot) -> TransportEdit {
    match w {
        Widget::Cols => TransportEdit::SetCols(clamp_u32(v, 1, snap.grid_max)),
        Widget::Rows => TransportEdit::SetRows(clamp_u32(v, 1, snap.grid_max)),
        Widget::Count => TransportEdit::SetCount(clamp_u32(v, 1, (snap.cols * snap.rows).max(1))),
        Widget::Fps => TransportEdit::SetFps((v as f32).clamp(FPS_MIN, FPS_MAX)),
        _ => TransportEdit::TogglePlay, // unreachable for non-fields
    }
}

fn clamp_u32(v: f64, lo: u32, hi: u32) -> u32 {
    (v.round().max(0.0) as u32).clamp(lo, hi.max(lo))
}

/// Format fps compactly: integer when whole, else one decimal.
fn fmt_fps(fps: f32) -> String {
    if (fps - fps.round()).abs() < 0.05 {
        format!("{}", fps.round() as i32)
    } else {
        format!("{fps:.1}")
    }
}

/// Width (px) of a short label in the UI font, measured with a temporary DC on the desktop — used
/// only for layout gaps, so a rough measure via the chrome font is enough.
fn text_span(s: &str, chrome: &Chrome) -> i32 {
    use windows_sys::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
    unsafe {
        let screen: HWND = core::ptr::null_mut();
        let hdc = GetDC(screen);
        let prev = SelectObject(hdc, chrome.metrics.font as _);
        let w = text_width(hdc, s);
        SelectObject(hdc, prev);
        ReleaseDC(screen, hdc);
        w
    }
}

/// A 1px rectangle outline (four fills), for field/checkbox borders.
fn outline(hdc: HDC, r: &RECT, color: u32) {
    fill(
        hdc,
        &RECT {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.top + 1,
        },
        color,
    );
    fill(
        hdc,
        &RECT {
            left: r.left,
            top: r.bottom - 1,
            right: r.right,
            bottom: r.bottom,
        },
        color,
    );
    fill(
        hdc,
        &RECT {
            left: r.left,
            top: r.top,
            right: r.left + 1,
            bottom: r.bottom,
        },
        color,
    );
    fill(
        hdc,
        &RECT {
            left: r.right - 1,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        },
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> TransportSnapshot {
        TransportSnapshot {
            cols: 8,
            rows: 8,
            frame_count: 64,
            fps: 24.0,
            blend: false,
            playing: false,
            frame_pos: 0.0,
            grid_max: 64,
        }
    }

    #[test]
    fn field_edit_clamps() {
        let s = snap();
        assert!(matches!(
            field_edit(Widget::Cols, 100.0, &s),
            TransportEdit::SetCols(64)
        ));
        assert!(matches!(
            field_edit(Widget::Cols, 0.0, &s),
            TransportEdit::SetCols(1)
        ));
        assert!(matches!(
            field_edit(Widget::Count, 999.0, &s),
            TransportEdit::SetCount(64)
        ));
        assert!(matches!(
            field_edit(Widget::Count, -5.0, &s),
            TransportEdit::SetCount(1)
        ));
        match field_edit(Widget::Fps, 999.0, &s) {
            TransportEdit::SetFps(f) => assert_eq!(f, FPS_MAX),
            _ => panic!("expected SetFps"),
        }
        match field_edit(Widget::Fps, 0.0, &s) {
            TransportEdit::SetFps(f) => assert_eq!(f, FPS_MIN),
            _ => panic!("expected SetFps"),
        }
    }

    #[test]
    fn fps_formats_compactly() {
        assert_eq!(fmt_fps(24.0), "24");
        assert_eq!(fmt_fps(23.976), "24"); // within 0.05 of a whole number
        assert_eq!(fmt_fps(12.5), "12.5");
    }

    #[test]
    fn layout_rects_valid_and_slider_within_band() {
        let chrome = Chrome::new(96, false);
        let band = RECT {
            left: 0,
            top: 100,
            right: 800,
            bottom: 130,
        };
        let mut t = Transport::default();
        t.layout(band, &chrome);
        // Every widget rect is non-degenerate and inside the band vertically.
        for &(_, r) in &t.rects {
            assert!(r.right > r.left, "widget rect has non-positive width");
            assert!(r.bottom > r.top);
            assert!(r.top >= band.top && r.bottom <= band.bottom);
        }
        // The slider track sits within the band and left of the right group.
        assert!(t.slider_track.left >= band.left && t.slider_track.right <= band.right);
        assert!(t.slider_track.right > t.slider_track.left);
        // A hit at the cols field center resolves to Cols.
        let cols = t.rect(Widget::Cols).unwrap();
        assert_eq!(
            t.hit((cols.left + cols.right) / 2, (cols.top + cols.bottom) / 2),
            Some(Widget::Cols)
        );
    }

    #[test]
    fn slider_scrub_snaps_when_blend_off() {
        let chrome = Chrome::new(96, false);
        let band = RECT {
            left: 0,
            top: 100,
            right: 800,
            bottom: 130,
        };
        let mut t = Transport::default();
        t.layout(band, &chrome);
        let s = snap(); // blend off, 64 frames
                        // Scrub at the track's left end → frame 0.
        let e = t.scrub_at(t.slider_track.left, &s);
        match e {
            TransportEdit::Scrub(p) => assert_eq!(p, 0.0),
            _ => panic!("expected Scrub"),
        }
        // Scrub past the right end clamps to the last integer frame (blend off snaps).
        let e = t.scrub_at(t.slider_track.right + 100, &s);
        match e {
            TransportEdit::Scrub(p) => assert_eq!(p, 63.0),
            _ => panic!("expected Scrub"),
        }
    }

    #[test]
    fn slider_target_spans_the_band_and_excludes_the_readout() {
        let chrome = Chrome::new(96, false);
        let band = RECT {
            left: 0,
            top: 100,
            right: 800,
            bottom: 130,
        };
        let mut t = Transport::default();
        t.layout(band, &chrome);
        let track = t.slider_track;
        // The thumb is a few px tall, but the target is the full band height over the track: a
        // press just under the band's top border and one just above its bottom both hit the slider.
        let x = (track.left + track.right) / 2;
        assert_eq!(t.hit(x, band.top + 1), Some(Widget::Slider));
        assert_eq!(t.hit(x, band.bottom - 1), Some(Widget::Slider));
        // The "frame / total" readout sits right of the track and is inert — clicking it must not
        // seek to the last frame.
        assert_eq!(t.hit(track.right + t.thumb_half + 2, band.top + 15), None);
        // The thumb's centre stays on the groove at both extremes.
        assert!(t.thumb_x(0.0) - t.thumb_half >= track.left);
        assert!(t.thumb_x(1.0) + t.thumb_half <= track.right);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)] // test pokes the private `editing` field directly
    fn type_edit_buffer_and_commit() {
        let mut t = Transport::default();
        t.editing = Some(FieldEdit {
            widget: Widget::Cols,
            buffer: String::new(),
        });
        assert!(t.type_char('1'));
        assert!(t.type_char('2'));
        assert!(!t.type_char('x')); // non-digit ignored for an integer field
        let s = snap();
        match t.commit(&s) {
            Some(TransportEdit::SetCols(12)) => {}
            other => panic!("expected SetCols(12), got {other:?}"),
        }
        assert!(!t.is_editing());
        // Fps field accepts a single dot.
        t.editing = Some(FieldEdit {
            widget: Widget::Fps,
            buffer: String::new(),
        });
        for c in "23.9".chars() {
            t.type_char(c);
        }
        assert!(!t.type_char('.')); // second dot rejected
        match t.commit(&s) {
            Some(TransportEdit::SetFps(f)) => assert!((f - 23.9).abs() < 1e-3),
            other => panic!("expected SetFps, got {other:?}"),
        }
    }
}
