//! Timers without threads: a deadline min-heap the event loop sleeps on.
//!
//! The shell's rendering is event-driven — no input, no timer, no event → no frame — and winit's
//! `ControlFlow::WaitUntil` is what keeps that true with zero threads: the loop parks until the
//! earliest deadline here, wakes, and [`crate::app::Fire::about_to_wait`] dispatches whatever fell
//! due. Every timer in the app (GIF frame advance, the flipbook pump, the caret blink) goes through
//! this queue.
//!
//! Cancellation is by sequence number rather than by removal: arming returns a `seq`, the window
//! remembers the `seq` of the timer it currently wants for that kind, and a popped entry whose
//! `seq` no longer matches is simply dropped. A re-armed timer therefore never has to find and
//! delete its predecessor in the heap.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::rc::Rc;
use std::time::Instant;

use winit::window::WindowId;

/// The kinds of timer a window can arm. One of each may be pending per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerKind {
    /// Animated-image (GIF) playback: the displayed frame's delay has elapsed.
    Anim = 0,
    /// Flipbook playback: the ~60 Hz pump that keeps playback fed when nothing else asks for a
    /// frame (see `Viewer::FLIPBOOK_TICK_MS`).
    Flipbook = 1,
    /// The text caret's blink, armed only while a text field is being edited.
    Caret = 2,
}

/// The number of [`TimerKind`]s, for per-window arrays indexed by kind.
pub const KINDS: usize = 3;

/// The process-wide timer queue, shared by the event loop and every window.
#[derive(Debug, Default)]
pub struct TimerQueue {
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
    meta: HashMap<u64, (WindowId, TimerKind)>,
    next_seq: u64,
}

pub type Timers = Rc<RefCell<TimerQueue>>;

impl TimerQueue {
    /// Arm a timer for `window` to fire at `at`. Returns its sequence number; the window keeps it
    /// and ignores the firing if it has since re-armed or killed the kind.
    pub fn arm(&mut self, window: WindowId, kind: TimerKind, at: Instant) -> u64 {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.heap.push(Reverse((at, seq)));
        self.meta.insert(seq, (window, kind));
        seq
    }

    /// The earliest pending deadline, for `ControlFlow::WaitUntil`. Stale entries (already
    /// cancelled by re-arming) are still counted: they wake the loop once and are dropped, which
    /// is cheaper than keeping the heap exact.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.heap.peek().map(|Reverse((at, _))| *at)
    }

    /// Pop everything due at `now`, in deadline order.
    pub fn pop_due(&mut self, now: Instant) -> Vec<(WindowId, TimerKind, u64)> {
        let mut due = Vec::new();
        while let Some(Reverse((at, seq))) = self.heap.peek().copied() {
            if at > now {
                break;
            }
            self.heap.pop();
            if let Some((window, kind)) = self.meta.remove(&seq) {
                due.push((window, kind, seq));
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn fires_in_deadline_order_and_reports_the_seq() {
        let w = WindowId::from(1u64);
        let mut q = TimerQueue::default();
        let t0 = Instant::now();
        let late = q.arm(w, TimerKind::Anim, t0 + Duration::from_millis(50));
        let early = q.arm(w, TimerKind::Caret, t0 + Duration::from_millis(10));
        assert_eq!(q.next_deadline(), Some(t0 + Duration::from_millis(10)));
        assert!(q.pop_due(t0).is_empty());
        assert_eq!(
            q.pop_due(t0 + Duration::from_millis(20)),
            vec![(w, TimerKind::Caret, early)]
        );
        assert_eq!(
            q.pop_due(t0 + Duration::from_secs(1)),
            vec![(w, TimerKind::Anim, late)]
        );
        assert_eq!(q.next_deadline(), None);
    }
}
