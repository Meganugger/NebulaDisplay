//! Multi-touch contact tracking (ROADMAP P2 item 14).
//!
//! Windows synthetic-pointer touch injection
//! (`InjectSyntheticPointerInput` with `PT_TOUCH`) requires every injection
//! call to carry the **full frame of active contacts**, not just the one
//! that changed — otherwise the OS considers the missing contacts lifted.
//! This module is the pure, platform-independent state machine that turns
//! NDSP's per-contact [`InputEvent::Touch`](ndsp_protocol::messages::InputEvent)
//! stream into those frames, so it can be unit-tested everywhere while the
//! `unsafe` injection stays a thin Windows-only shim.
//!
//! Robustness rules (viewers are not trusted to send perfect sequences):
//! * `Move`/`End` for an unknown id: `Move` implies a missed `Start` (treated
//!   as one); `End`/`Cancel` for an unknown id is a no-op.
//! * `Start` for an already-tracked id is a missed `End` — treated as a move.
//! * More than [`MAX_CONTACTS`] simultaneous contacts: extra starts are
//!   ignored (Windows caps synthetic touch devices at 10 contacts).

use ndsp_protocol::messages::TouchPhase;

/// Windows synthetic pointer devices support at most 10 touch contacts.
pub const MAX_CONTACTS: usize = 10;

/// Per-contact transition to inject for this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// New contact this frame (`POINTER_FLAG_DOWN`).
    Down,
    /// Contact continues (`POINTER_FLAG_UPDATE`), moved or held.
    Update,
    /// Contact lifted this frame (`POINTER_FLAG_UP`).
    Up,
    /// Contact aborted this frame (`POINTER_FLAG_UP | CANCELED`).
    Cancel,
}

/// One contact within an injection frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contact {
    /// Stable slot index (0..[`MAX_CONTACTS`]) — used as the OS pointer id so
    /// arbitrary viewer-side ids (browsers use large counters) stay in the
    /// range synthetic pointer devices accept.
    pub slot: u32,
    /// Normalized position (0..1) on the streamed surface.
    pub x: f32,
    pub y: f32,
    /// Normalized pressure 0..1 (viewers report 1.0 when unavailable).
    pub pressure: f32,
    pub action: Action,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    id: u32,
    x: f32,
    y: f32,
    pressure: f32,
}

/// Tracks active contacts and produces full injection frames.
#[derive(Default)]
pub struct TouchTracker {
    slots: [Option<Slot>; MAX_CONTACTS],
}

impl TouchTracker {
    /// Apply one NDSP touch event. Returns the full frame to inject (every
    /// active contact, the changed one carrying its transition action), or
    /// `None` when the event is a no-op (unknown id lift, contact overflow).
    pub fn apply(
        &mut self,
        id: u32,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
    ) -> Option<Vec<Contact>> {
        let existing = self.slot_of(id);
        let (slot, action) = match (phase, existing) {
            // Start on a tracked id = missed End; keep continuity as a move.
            (TouchPhase::Start, Some(s)) => (s, Action::Update),
            (TouchPhase::Start, None) => (self.claim(id)?, Action::Down),
            (TouchPhase::Move, Some(s)) => (s, Action::Update),
            // Move without Start = missed Start (e.g. grant toggled mid-gesture).
            (TouchPhase::Move, None) => (self.claim(id)?, Action::Down),
            (TouchPhase::End, Some(s)) => (s, Action::Up),
            (TouchPhase::Cancel, Some(s)) => (s, Action::Cancel),
            (TouchPhase::End | TouchPhase::Cancel, None) => return None,
        };
        self.slots[slot] = Some(Slot { id, x, y, pressure });
        let frame = self.frame_with(slot, action);
        if matches!(action, Action::Up | Action::Cancel) {
            self.slots[slot] = None;
        }
        Some(frame)
    }

    /// Cancel every active contact (session teardown / injection failure) —
    /// returns the final frame to inject, or `None` when nothing is active.
    pub fn cancel_all(&mut self) -> Option<Vec<Contact>> {
        let frame: Vec<Contact> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.map(|s| Contact {
                    slot: i as u32,
                    x: s.x,
                    y: s.y,
                    pressure: s.pressure,
                    action: Action::Cancel,
                })
            })
            .collect();
        self.slots = Default::default();
        if frame.is_empty() {
            None
        } else {
            Some(frame)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    fn slot_of(&self, id: u32) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.is_some_and(|s| s.id == id))
    }

    fn claim(&mut self, id: u32) -> Option<usize> {
        let slot = self.slots.iter().position(Option::is_none)?;
        self.slots[slot] = Some(Slot {
            id,
            x: 0.0,
            y: 0.0,
            pressure: 0.0,
        });
        Some(slot)
    }

    /// Full frame: every active contact as `Update`, except `changed`.
    fn frame_with(&self, changed: usize, action: Action) -> Vec<Contact> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.map(|s| Contact {
                    slot: i as u32,
                    x: s.x,
                    y: s.y,
                    pressure: s.pressure,
                    action: if i == changed { action } else { Action::Update },
                })
            })
            .collect()
    }
}

/// NDSP touch pressure (0..1) → Windows touch pressure (0..1024), with the
/// same contact floor as the pen path (a zero would read as "no contact").
pub fn touch_pressure_1024(pressure: f32, in_contact: bool) -> u32 {
    super::pen_pressure_1024(pressure, in_contact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use TouchPhase::*;

    fn actions(frame: &[Contact]) -> Vec<(u32, Action)> {
        frame.iter().map(|c| (c.slot, c.action)).collect()
    }

    #[test]
    fn single_tap_lifecycle() {
        let mut t = TouchTracker::default();
        let f = t.apply(7, Start, 0.5, 0.5, 1.0).unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Down)]);
        let f = t.apply(7, Move, 0.6, 0.5, 1.0).unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Update)]);
        assert!((f[0].x - 0.6).abs() < 1e-6);
        let f = t.apply(7, End, 0.6, 0.5, 0.0).unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Up)]);
        assert!(t.is_empty());
    }

    #[test]
    fn two_finger_pinch_frames_carry_both_contacts() {
        let mut t = TouchTracker::default();
        t.apply(100, Start, 0.4, 0.5, 1.0).unwrap();
        let f = t.apply(200, Start, 0.6, 0.5, 1.0).unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Update), (1, Action::Down)]);
        // Moving one finger still injects both (full-frame requirement).
        let f = t.apply(100, Move, 0.35, 0.5, 1.0).unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Update), (1, Action::Update)]);
        assert!((f[0].x - 0.35).abs() < 1e-6);
        assert!((f[1].x - 0.6).abs() < 1e-6);
        // Lifting the first keeps the second active.
        let f = t.apply(100, End, 0.35, 0.5, 0.0).unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Up), (1, Action::Update)]);
        let f = t.apply(200, End, 0.6, 0.5, 0.0).unwrap();
        assert_eq!(actions(&f), vec![(1, Action::Up)]);
        assert!(t.is_empty());
    }

    #[test]
    fn slots_are_reused_and_stable() {
        let mut t = TouchTracker::default();
        t.apply(1, Start, 0.1, 0.1, 1.0).unwrap();
        t.apply(2, Start, 0.2, 0.2, 1.0).unwrap();
        t.apply(1, End, 0.1, 0.1, 0.0).unwrap();
        // New contact takes the freed slot 0; contact 2 keeps slot 1.
        let f = t.apply(3, Start, 0.3, 0.3, 1.0).unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Down), (1, Action::Update)]);
    }

    #[test]
    fn unknown_move_is_a_start_and_unknown_end_is_ignored() {
        let mut t = TouchTracker::default();
        let f = t.apply(9, Move, 0.5, 0.5, 1.0).unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Down)]);
        assert!(t.apply(12345, End, 0.0, 0.0, 0.0).is_none());
        assert!(!t.is_empty());
    }

    #[test]
    fn restart_of_tracked_id_is_a_move() {
        let mut t = TouchTracker::default();
        t.apply(5, Start, 0.1, 0.1, 1.0).unwrap();
        let f = t.apply(5, Start, 0.2, 0.2, 1.0).unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Update)]);
    }

    #[test]
    fn overflow_contacts_are_ignored() {
        let mut t = TouchTracker::default();
        for id in 0..MAX_CONTACTS as u32 {
            assert!(t.apply(id, Start, 0.5, 0.5, 1.0).is_some());
        }
        assert!(t.apply(999, Start, 0.5, 0.5, 1.0).is_none());
        // Existing contacts still work.
        let f = t.apply(0, Move, 0.6, 0.6, 1.0).unwrap();
        assert_eq!(f.len(), MAX_CONTACTS);
    }

    #[test]
    fn cancel_all_flushes_every_contact() {
        let mut t = TouchTracker::default();
        assert!(t.cancel_all().is_none(), "empty tracker cancels nothing");
        t.apply(1, Start, 0.1, 0.1, 1.0).unwrap();
        t.apply(2, Start, 0.2, 0.2, 1.0).unwrap();
        let f = t.cancel_all().unwrap();
        assert_eq!(actions(&f), vec![(0, Action::Cancel), (1, Action::Cancel)]);
        assert!(t.is_empty());
    }

    #[test]
    fn touch_pressure_matches_pen_scaling() {
        assert_eq!(touch_pressure_1024(0.0, true), 1);
        assert_eq!(touch_pressure_1024(1.0, true), 1024);
        assert_eq!(touch_pressure_1024(0.0, false), 0);
    }
}
