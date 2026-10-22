//! PSD layer stepping: the pure half — where the cursor is, which layers are worth decoding ahead
//! of it, what to keep, and what the status bar says. No window, GPU or thread lives here, so all
//! of it is unit-tested; the viewer (`app::viewer`) and the decode pool drive it.
//!
//! **Positions.** Position 0 is the composite; position `p` in `1..=len` is
//! `PsdLayers::layers()[p - 1]`, counted from the top of the stack — so Down (+1) walks the
//! Layers panel downward, and the composite is "above" every layer.
//!
//! **The window.** Work is only worth doing near the cursor: the target itself, [`AHEAD`] layers
//! further in the direction of travel and [`BEHIND`] layer back. Workers re-check
//! [`in_window`] against the live cursor ([`SessionCtl`]) between stages, so when the user holds
//! the key down, the jobs the cursor has already passed stop within milliseconds instead of
//! finishing for nobody ("latest wins").

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use fire_decode::{BlendMode, LayerInfo, PixelFormat};

/// How long the composite must stay on screen before layers start decoding in the background.
/// Flipping through a folder faster than this never starts any layer work.
pub const DWELL_MS: u64 = 300;
/// Layers decoded ahead of the cursor, in the direction of travel.
pub const AHEAD: u32 = 2;
/// Layers kept decoded behind the cursor, for a step back.
pub const BEHIND: u32 = 1;
/// Background (prefetch) jobs one session may have in flight.
pub const MAX_BACKGROUND_JOBS: usize = 2;
/// A background job at least this big counts as large; only one large one runs at a time.
pub const LARGE_JOB_BYTES: u64 = 256 << 20;
/// Decoded layers (pixels + mip chain) kept in RAM per session.
pub const CPU_CACHE_BYTES: u64 = 1 << 30;
/// Layer textures kept uploaded but not shown, per window. The displayed texture and the parked
/// composite are not counted.
pub const GPU_STASH_BYTES: u64 = 512 << 20;
/// How long after the last step before an idle frame uploads a neighbour, so holding a key down
/// never pays for uploads the cursor is about to leave behind.
pub const STAGE_IDLE_MS: u64 = 40;

/// The state a session shares with the workers decoding for it.
#[derive(Debug)]
pub struct SessionCtl {
    pub id: u64,
    /// The session is over (another file, a reload, the window closed): every job stops.
    pub cancelled: AtomicBool,
    /// The position being shown or asked for.
    pub cursor: AtomicU32,
    /// The direction of the last step, +1 or -1.
    pub dir: AtomicI32,
}

impl SessionCtl {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            cancelled: AtomicBool::new(false),
            cursor: AtomicU32::new(0),
            dir: AtomicI32::new(1),
        }
    }

    /// Whether a job for `pos` is still wanted.
    pub fn wants(&self, pos: u32) -> bool {
        !self.cancelled.load(Ordering::Relaxed)
            && in_window(
                pos,
                self.cursor.load(Ordering::Relaxed),
                self.dir.load(Ordering::Relaxed),
            )
    }
}

/// One job's controls.
#[derive(Debug, Default)]
pub struct JobCtl {
    pub cancelled: AtomicBool,
    /// The user is waiting on this one: use every core. Read at each stage, so a background job
    /// the cursor lands on speeds up mid-flight.
    pub foreground: AtomicBool,
    /// Set by the pool when it queues the job on the main lane (a foreground submit, or no
    /// prefetch lane to send it to).
    pub main_lane: AtomicBool,
}

/// Whether position `pos` is near enough to `cursor` (moving in `dir`) to be worth decoding.
pub fn in_window(pos: u32, cursor: u32, dir: i32) -> bool {
    if pos == 0 {
        return false; // the composite is never a layer job
    }
    let d = (pos as i64 - cursor as i64) * if dir < 0 { -1 } else { 1 };
    d >= -(BEHIND as i64) && d <= AHEAD as i64
}

/// The cursor over position 0 (the composite) and `1..=len` (the layers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nav {
    pub pos: u32,
    pub len: u32,
    pub dir: i32,
}

impl Nav {
    pub fn new(len: u32) -> Self {
        Self {
            pos: 0,
            len,
            dir: 1,
        }
    }

    /// Move by `delta`, stopping at either end. Returns whether the position changed.
    pub fn step(&mut self, delta: i32) -> bool {
        let next = (self.pos as i64 + delta as i64).clamp(0, self.len as i64) as u32;
        if next == self.pos {
            return false;
        }
        self.dir = if delta < 0 { -1 } else { 1 };
        self.pos = next;
        true
    }

    /// Positions to decode, most wanted first: the target (a layer), then [`AHEAD`] further in
    /// the direction of travel, then [`BEHIND`] back. Never the composite.
    pub fn wanted(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(4);
        let mut push = |p: i64| {
            if p >= 1 && p <= self.len as i64 && !out.contains(&(p as u32)) {
                out.push(p as u32);
            }
        };
        let (pos, dir) = (self.pos as i64, self.dir as i64);
        push(pos);
        for k in 1..=AHEAD as i64 {
            push(pos + k * dir);
        }
        for k in 1..=BEHIND as i64 {
            push(pos - k * dir);
        }
        out
    }

    /// Positions worth holding as uploaded textures, most wanted first: the next one in the
    /// direction of travel, the one just left, then one further on. The composite (0) qualifies
    /// like any other position.
    pub fn stage_order(&self) -> Vec<u32> {
        let (pos, dir) = (self.pos as i64, self.dir as i64);
        [pos + dir, pos - dir, pos + 2 * dir]
            .into_iter()
            .filter(|&p| p >= 0 && p <= self.len as i64)
            .map(|p| p as u32)
            .collect()
    }
}

impl Nav {
    /// [`Self::stage_order`], then every other already-uploaded position nearest first: what is
    /// already on the GPU stays while the budget allows, so reversing direction is a binding swap
    /// rather than a re-upload.
    pub fn keep_order(&self, staged: &[u32]) -> Vec<u32> {
        let mut out = self.stage_order();
        let mut rest: Vec<u32> = staged
            .iter()
            .copied()
            .filter(|p| *p != self.pos && !out.contains(p))
            .collect();
        // Nearest first; at equal distance, the one ahead in the direction of travel.
        let behind = |p: u32| (p as i64 - self.pos as i64) * (self.dir as i64) < 0;
        rest.sort_by_key(|&p| (p.abs_diff(self.pos), behind(p)));
        out.extend(rest);
        out
    }
}

/// Bytes one decoded layer of `w`×`h` in `format` holds in RAM: level 0 plus its mip chain.
pub fn est_bytes(w: u32, h: u32, format: PixelFormat) -> u64 {
    let bpp = crate::render::mips::bytes_per_texel(format) as u64;
    w as u64 * h as u64 * bpp * 4 / 3
}

/// Keyed by position, bounded by bytes. Evicts the entry farthest from the cursor first (the
/// least recently used one breaks ties), never one the caller marks as protected.
#[derive(Debug)]
pub struct ByteCache<V> {
    entries: Vec<Entry<V>>,
    budget: u64,
    clock: u64,
}

#[derive(Debug)]
struct Entry<V> {
    key: u32,
    value: V,
    bytes: u64,
    used: u64,
}

impl<V> ByteCache<V> {
    pub fn new(budget: u64) -> Self {
        Self {
            entries: Vec::new(),
            budget,
            clock: 0,
        }
    }

    pub fn contains(&self, key: u32) -> bool {
        self.entries.iter().any(|e| e.key == key)
    }

    pub fn get(&mut self, key: u32) -> Option<&V> {
        self.clock += 1;
        let clock = self.clock;
        let e = self.entries.iter_mut().find(|e| e.key == key)?;
        e.used = clock;
        Some(&e.value)
    }

    /// Insert (or replace) `key`.
    pub fn insert(&mut self, key: u32, value: V, bytes: u64) {
        self.clock += 1;
        self.entries.retain(|e| e.key != key);
        self.entries.push(Entry {
            key,
            value,
            bytes,
            used: self.clock,
        });
    }

    pub fn total(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }

    /// Evict until the cache fits its budget, sparing every key in `protect`. Returns what was
    /// evicted, keyed, so the caller can free it off the UI thread.
    pub fn trim(&mut self, cursor: u32, protect: &[u32]) -> Vec<(u32, V)> {
        let mut out = Vec::new();
        while self.total() > self.budget {
            let victim = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !protect.contains(&e.key))
                .max_by_key(|(_, e)| (e.key.abs_diff(cursor), std::cmp::Reverse(e.used)))
                .map(|(i, _)| i);
            match victim {
                Some(i) => {
                    let e = self.entries.swap_remove(i);
                    out.push((e.key, e.value));
                }
                None => break, // everything left is protected
            }
        }
        out
    }

    /// Remove everything, returning it.
    pub fn drain(&mut self) -> Vec<V> {
        self.entries.drain(..).map(|e| e.value).collect()
    }
}

/// Which of the `candidates` (in preference order, as from [`Nav::stage_order`]) to hold as
/// textures within `budget`, given each one's size. Position 0 is always kept and costs nothing
/// against the budget: it is the composite, parked rather than staged.
pub fn stage_keep(
    candidates: &[u32],
    size_of: impl Fn(u32) -> Option<u64>,
    budget: u64,
) -> Vec<u32> {
    let mut used = 0u64;
    let mut keep = vec![0];
    for &p in candidates {
        if p == 0 {
            continue;
        }
        if let Some(b) = size_of(p) {
            if used + b <= budget {
                used += b;
                keep.push(p);
            }
        }
    }
    keep
}

/// The parts of a [`LayerInfo`] the status bar describes.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerLabel {
    pub name: String,
    pub group_path: Vec<String>,
    pub hidden: bool,
    pub hidden_by_group: bool,
    pub opacity: u8,
    pub blend: BlendMode,
    pub clipped: bool,
}

impl From<&LayerInfo> for LayerLabel {
    fn from(l: &LayerInfo) -> Self {
        Self {
            name: l.name.clone(),
            group_path: l.group_path.clone(),
            hidden: l.hidden,
            hidden_by_group: l.hidden_by_group,
            opacity: l.opacity,
            blend: l.blend,
            clipped: l.clipped,
        }
    }
}

/// What the status bar says about the layer being shown, after the file name: the group path
/// (its last three parts), whether it is hidden, opacity and blend mode when they would change
/// how it composites, and clipping.
pub fn status_detail(l: &LayerLabel) -> String {
    let mut parts = Vec::new();
    let name = if l.name.trim().is_empty() {
        "(unnamed layer)"
    } else {
        l.name.as_str()
    };
    let mut path: Vec<&str> = l.group_path.iter().map(String::as_str).collect();
    if path.len() > 3 {
        path.drain(..path.len() - 3);
        path.insert(0, "…");
    }
    path.push(name);
    parts.push(path.join(" / "));
    if l.hidden {
        parts.push("hidden".to_string());
    } else if l.hidden_by_group {
        parts.push("hidden (group)".to_string());
    }
    let opacity = (l.opacity as u32 * 100 + 127) / 255;
    match (l.blend, opacity < 100) {
        (BlendMode::Normal, false) => {}
        (BlendMode::Normal, true) => parts.push(format!("{opacity}%")),
        (b, false) => parts.push(b.label()),
        (b, true) => parts.push(format!("{} {opacity}%", b.label())),
    }
    if l.clipped {
        parts.push("clipped".to_string());
    }
    parts.join("   ·   ")
}

/// Where a layer stands, for the status bar's counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerState {
    Shown,
    Loading,
    Failed(String),
}

/// The status bar's counter: `Layer 3 / 12`, with what is still pending or what went wrong; at
/// position 0 a hint that layers exist.
pub fn status_counter(pos: u32, len: u32, state: &LayerState) -> String {
    if pos == 0 {
        return if len == 1 {
            "1 layer".to_string()
        } else {
            format!("{len} layers")
        };
    }
    match state {
        LayerState::Shown => format!("Layer {pos} / {len}"),
        LayerState::Loading => format!("Layer {pos} / {len} — loading…"),
        LayerState::Failed(e) => format!("Layer {pos} / {len} — failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_clamps_at_both_ends() {
        let mut n = Nav::new(3);
        assert!(!n.step(-1), "already at the composite");
        assert!(n.step(1));
        assert_eq!((n.pos, n.dir), (1, 1));
        assert!(n.step(1) && n.step(1));
        assert!(!n.step(1), "already at the bottom layer");
        assert_eq!(n.pos, 3);
        assert!(n.step(-1));
        assert_eq!((n.pos, n.dir), (2, -1));
        assert!(n.step(-5));
        assert_eq!(n.pos, 0);
    }

    #[test]
    fn window_follows_the_direction_of_travel() {
        // Moving down from 5: 5, 6, 7 ahead, 4 behind.
        for (p, want) in [
            (3, false),
            (4, true),
            (5, true),
            (6, true),
            (7, true),
            (8, false),
        ] {
            assert_eq!(in_window(p, 5, 1), want, "pos {p} going down");
        }
        // Moving up from 5: 5, 4, 3 ahead, 6 behind.
        for (p, want) in [
            (2, false),
            (3, true),
            (4, true),
            (5, true),
            (6, true),
            (7, false),
        ] {
            assert_eq!(in_window(p, 5, -1), want, "pos {p} going up");
        }
        assert!(!in_window(0, 0, 1), "the composite is never a job");
        // Before the first step (cursor 0, going down) the warm-up layers are in the window.
        assert!(in_window(1, 0, 1) && in_window(2, 0, 1) && !in_window(3, 0, 1));
    }

    #[test]
    fn wanted_and_stage_order() {
        let mut n = Nav::new(10);
        assert_eq!(n.wanted(), [1, 2], "warm-up from the composite");
        n.pos = 5;
        assert_eq!(n.wanted(), [5, 6, 7, 4]);
        assert_eq!(n.stage_order(), [6, 4, 7]);
        n.dir = -1;
        assert_eq!(n.wanted(), [5, 4, 3, 6]);
        n.pos = 1;
        assert_eq!(n.wanted(), [1, 2]);
        assert_eq!(
            n.stage_order(),
            [0, 2],
            "the composite is a staging candidate"
        );
        n.pos = 10;
        n.dir = 1;
        assert_eq!(n.wanted(), [10, 9]);
    }

    #[test]
    fn keep_order_retains_uploaded_layers_nearest_first() {
        let n = Nav {
            pos: 5,
            len: 10,
            dir: -1,
        };
        // Preferred three first (4, 6, 3), then the rest of what is staged by distance.
        assert_eq!(n.keep_order(&[9, 6, 7, 0, 5, 1]), [4, 6, 3, 7, 1, 9, 0]);
    }

    #[test]
    fn cache_evicts_farthest_from_the_cursor_and_spares_protected() {
        let mut c = ByteCache::new(300);
        for k in 1..=4 {
            c.insert(k, k, 100);
        }
        // Over budget by one entry: cursor at 1 → 4 is farthest.
        assert_eq!(c.trim(1, &[]), [(4, 4)]);
        c.insert(5, 5, 100);
        // Cursor at 5, but 1 (farthest) is protected → 2 goes.
        assert_eq!(c.trim(5, &[1]), [(2, 2)]);
        assert!(c.contains(1) && c.contains(3) && c.contains(5));
        // A single oversized entry nobody protects is evicted outright.
        c.insert(9, 9, 1000);
        let mut gone = c.trim(9, &[]);
        gone.sort();
        assert!(gone.contains(&(1, 1)), "the far entries go first");
        // Everything protected: trim stops rather than looping.
        let mut c = ByteCache::new(10);
        c.insert(1, (), 100);
        assert!(c.trim(1, &[1]).is_empty());
        assert_eq!(c.total(), 100);
    }

    #[test]
    fn cache_breaks_distance_ties_by_least_recent_use() {
        let mut c = ByteCache::new(200);
        c.insert(4, 'a', 100);
        c.insert(6, 'b', 100);
        let _ = c.get(4); // 6 is now the least recently used of the two equidistant entries
        c.insert(5, 'c', 100);
        assert_eq!(c.trim(5, &[5]), [(6, 'b')]);
    }

    #[test]
    fn stage_keep_respects_the_budget_in_preference_order() {
        let size = |p: u32| Some(if p == 6 { 400 } else { 100 });
        assert_eq!(stage_keep(&[6, 4, 7], size, 500), [0, 6, 4]);
        assert_eq!(stage_keep(&[6, 4, 7], size, 300), [0, 4, 7]);
        assert_eq!(
            stage_keep(&[0, 2], size, 0),
            [0],
            "the composite is always kept"
        );
        assert_eq!(
            stage_keep(&[3], |_| None, 1000),
            [0],
            "only decoded layers can be staged"
        );
    }

    fn label(name: &str, path: &[&str]) -> LayerLabel {
        LayerLabel {
            name: name.into(),
            group_path: path.iter().map(|s| s.to_string()).collect(),
            hidden: false,
            hidden_by_group: false,
            opacity: 255,
            blend: BlendMode::Normal,
            clipped: false,
        }
    }

    #[test]
    fn detail_text() {
        assert_eq!(status_detail(&label("Hero", &[])), "Hero");
        assert_eq!(
            status_detail(&label("Hero", &["Characters"])),
            "Characters / Hero"
        );
        assert_eq!(
            status_detail(&label("x", &["a", "b", "c", "d", "e"])),
            "… / c / d / e / x",
            "only the innermost three groups"
        );
        assert_eq!(status_detail(&label(" ", &[])), "(unnamed layer)");
        let mut l = label("Shade", &["G"]);
        l.hidden_by_group = true;
        l.blend = BlendMode::Multiply;
        l.opacity = 128;
        l.clipped = true;
        assert_eq!(
            status_detail(&l),
            "G / Shade   ·   hidden (group)   ·   Multiply 50%   ·   clipped"
        );
        l.hidden = true;
        l.opacity = 255;
        l.clipped = false;
        assert_eq!(status_detail(&l), "G / Shade   ·   hidden   ·   Multiply");
        l.blend = BlendMode::Normal;
        l.opacity = 64;
        assert_eq!(status_detail(&l), "G / Shade   ·   hidden   ·   25%");
    }

    #[test]
    fn counter_text() {
        assert_eq!(status_counter(0, 12, &LayerState::Shown), "12 layers");
        assert_eq!(status_counter(0, 1, &LayerState::Shown), "1 layer");
        assert_eq!(status_counter(3, 12, &LayerState::Shown), "Layer 3 / 12");
        assert_eq!(
            status_counter(3, 12, &LayerState::Loading),
            "Layer 3 / 12 — loading…"
        );
        assert_eq!(
            status_counter(3, 12, &LayerState::Failed("bad RLE".into())),
            "Layer 3 / 12 — failed: bad RLE"
        );
    }
}
