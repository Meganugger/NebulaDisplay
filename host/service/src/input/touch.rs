//! Multi-touch frame tracking (ROADMAP P2.14), OS-agnostic.
//!
//! Windows synthetic-pointer touch injection
//! (`CreateSyntheticPointerDevice(PT_TOUCH, …)`) requires every injection
//! frame to describe **all currently active contacts** with stable
//! per-contact pointer ids, while viewers only send per-contact deltas
//! (`Touch { id, phase, x, y, pressure }`). This tracker turns one wire
//! event into a full frame. It is pure logic — unit-tested on every
//! platform; only the raw injection lives in `windows_inject`.

use ndsp_protocol::messages::TouchPhase;

/// Contacts the synthetic touch device is created with. Windows allows up
/// to `MAX_TOUCH_COUNT` (256); ten covers every consumer touchscreen.
pub const MAX_TOUCH_CONTACTS: usize = 10;

/// What one contact does within an injected frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactAction {
    Down,
    Update,
    Up,
    Cancel,
}

/// One contact inside a full injection frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameContact {
    /// Stable pointer id for the OS — unique among *active* contacts, and
    /// stable for the lifetime of the contact (Windows tracks contacts by
    /// this id across frames).
    pub slot: u32,
    pub x: f32,
    pub y: f32,
    /// 0..1 as sent by the viewer.
    pub pressure: f32,
    pub action: ContactAction,
}

#[derive(Debug, Clone, Copy)]
struct Active {
    wire_id: u32,
    slot: u32,
    x: f32,
    y: f32,
    pressure: f32,
}

/// Turns per-contact wire events into full injection frames.
#[derive(Default)]
pub struct TouchTracker {
    active: Vec<Active>,
}

impl TouchTracker {
    /// Feed one wire event. Returns the full frame to inject (every active
    /// contact — the event's contact with its `Down`/`Update`/`Up`/`Cancel`
    /// action, the others as position-holding `Update`s), or `None` when
    /// nothing must be injected (unknown-id end/cancel, contact overflow).
    pub fn event(
        &mut self,
        id: u32,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
    ) -> Option<Vec<FrameContact>> {
        let known = self.active.iter().position(|c| c.wire_id == id);
        let (idx, action) = match (phase, known) {
            // A Start for an already-active contact means we missed its End
            // (viewer hiccup) — treat it as a move, exactly like the pen path.
            (TouchPhase::Start, Some(i)) => (i, ContactAction::Update),
            // A Move for an unknown contact means the stream began
            // mid-gesture (input grant toggled on) — implicit touch-down.
            (TouchPhase::Start, None) | (TouchPhase::Move, None) => {
                (self.insert(id, x, y, pressure)?, ContactAction::Down)
            }
            (TouchPhase::Move, Some(i)) => (i, ContactAction::Update),
            (TouchPhase::End, Some(i)) => (i, ContactAction::Up),
            (TouchPhase::Cancel, Some(i)) => (i, ContactAction::Cancel),
            // End/Cancel for a contact we never saw: nothing to lift.
            (TouchPhase::End | TouchPhase::Cancel, None) => return None,
        };
        let c = &mut self.active[idx];
        c.x = x;
        c.y = y;
        c.pressure = pressure;

        let frame = self
            .active
            .iter()
            .enumerate()
            .map(|(i, c)| FrameContact {
                slot: c.slot,
                x: c.x,
                y: c.y,
                pressure: c.pressure,
                action: if i == idx {
                    action
                } else {
                    ContactAction::Update
                },
            })
            .collect();
        if matches!(action, ContactAction::Up | ContactAction::Cancel) {
            self.active.remove(idx);
        }
        Some(frame)
    }

    /// True while no contact is down.
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Frame lifting every active contact as cancelled (device teardown).
    pub fn cancel_all(&mut self) -> Vec<FrameContact> {
        let frame = self
            .active
            .iter()
            .map(|c| FrameContact {
                slot: c.slot,
                x: c.x,
                y: c.y,
                pressure: c.pressure,
                action: ContactAction::Cancel,
            })
            .collect();
        self.active.clear();
        frame
    }

    /// Register a new contact; returns its index, or `None` at capacity
    /// (an 11th finger is ignored rather than corrupting the frame).
    fn insert(&mut self, id: u32, x: f32, y: f32, pressure: f32) -> Option<usize> {
        if self.active.len() >= MAX_TOUCH_CONTACTS {
            return None;
        }
        // Smallest slot not held by an active contact — ids stay small and
        // are only reused once the previous contact fully lifted.
        let slot = (0..).find(|s| self.active.iter().all(|c| c.slot != *s))?;
        self.active.push(Active {
            wire_id: id,
            slot,
            x,
            y,
            pressure,
        });
        Some(self.active.len() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(frame: &[FrameContact]) -> Vec<ContactAction> {
        frame.iter().map(|c| c.action).collect()
    }

    #[test]
    fn single_contact_lifecycle() {
        let mut t = TouchTracker::default();
        let down = t.event(7, TouchPhase::Start, 0.1, 0.2, 1.0).unwrap();
        assert_eq!(down.len(), 1);
        assert_eq!(down[0].action, ContactAction::Down);
        assert_eq!(down[0].slot, 0);

        let mv = t.event(7, TouchPhase::Move, 0.3, 0.4, 0.9).unwrap();
        assert_eq!(actions(&mv), [ContactAction::Update]);
        assert_eq!((mv[0].x, mv[0].y), (0.3, 0.4));

        let up = t.event(7, TouchPhase::End, 0.3, 0.4, 0.0).unwrap();
        assert_eq!(actions(&up), [ContactAction::Up]);
        assert!(t.is_empty());
    }

    #[test]
    fn frames_carry_all_active_contacts() {
        let mut t = TouchTracker::default();
        t.event(1, TouchPhase::Start, 0.1, 0.1, 1.0).unwrap();
        let second_down = t.event(2, TouchPhase::Start, 0.9, 0.9, 1.0).unwrap();
        assert_eq!(
            actions(&second_down),
            [ContactAction::Update, ContactAction::Down],
            "existing contact rides along as a position-holding update"
        );

        // Moving finger 1 keeps finger 2 in the frame at its last position.
        let mv = t.event(1, TouchPhase::Move, 0.2, 0.2, 1.0).unwrap();
        assert_eq!(mv.len(), 2);
        assert_eq!(actions(&mv), [ContactAction::Update, ContactAction::Update]);
        assert_eq!((mv[1].x, mv[1].y), (0.9, 0.9));

        // Lifting finger 2 still reports finger 1.
        let up = t.event(2, TouchPhase::End, 0.9, 0.9, 0.0).unwrap();
        assert_eq!(actions(&up), [ContactAction::Update, ContactAction::Up]);

        // ...and the next frame no longer contains it.
        let mv = t.event(1, TouchPhase::Move, 0.25, 0.25, 1.0).unwrap();
        assert_eq!(mv.len(), 1);
        assert_eq!(mv[0].slot, 0);
    }

    #[test]
    fn slots_are_stable_and_reused_only_after_lift() {
        let mut t = TouchTracker::default();
        t.event(100, TouchPhase::Start, 0.0, 0.0, 1.0).unwrap();
        t.event(200, TouchPhase::Start, 0.0, 0.0, 1.0).unwrap();
        t.event(100, TouchPhase::End, 0.0, 0.0, 0.0).unwrap();
        // Slot 0 freed; 200 keeps slot 1; a new contact takes slot 0.
        let down = t.event(300, TouchPhase::Start, 0.5, 0.5, 1.0).unwrap();
        let new = down
            .iter()
            .find(|c| c.action == ContactAction::Down)
            .unwrap();
        assert_eq!(new.slot, 0);
        let held = down
            .iter()
            .find(|c| c.action == ContactAction::Update)
            .unwrap();
        assert_eq!(held.slot, 1);
    }

    #[test]
    fn duplicate_start_is_a_move() {
        let mut t = TouchTracker::default();
        t.event(5, TouchPhase::Start, 0.1, 0.1, 1.0).unwrap();
        let again = t.event(5, TouchPhase::Start, 0.2, 0.2, 1.0).unwrap();
        assert_eq!(actions(&again), [ContactAction::Update]);
        assert_eq!(again.len(), 1, "no ghost second contact");
    }

    #[test]
    fn move_for_unknown_contact_is_an_implicit_down() {
        let mut t = TouchTracker::default();
        let frame = t.event(9, TouchPhase::Move, 0.4, 0.4, 1.0).unwrap();
        assert_eq!(actions(&frame), [ContactAction::Down]);
    }

    #[test]
    fn end_or_cancel_for_unknown_contact_is_ignored() {
        let mut t = TouchTracker::default();
        assert!(t.event(3, TouchPhase::End, 0.0, 0.0, 0.0).is_none());
        assert!(t.event(3, TouchPhase::Cancel, 0.0, 0.0, 0.0).is_none());
        assert!(t.is_empty());
    }

    #[test]
    fn cancel_lifts_with_cancel_action() {
        let mut t = TouchTracker::default();
        t.event(1, TouchPhase::Start, 0.1, 0.1, 1.0).unwrap();
        let frame = t.event(1, TouchPhase::Cancel, 0.1, 0.1, 0.0).unwrap();
        assert_eq!(actions(&frame), [ContactAction::Cancel]);
        assert!(t.is_empty());
    }

    #[test]
    fn contact_overflow_is_ignored() {
        let mut t = TouchTracker::default();
        for id in 0..MAX_TOUCH_CONTACTS as u32 {
            assert!(t.event(id, TouchPhase::Start, 0.5, 0.5, 1.0).is_some());
        }
        assert!(
            t.event(999, TouchPhase::Start, 0.5, 0.5, 1.0).is_none(),
            "an 11th finger must not corrupt the frame"
        );
        // Its later events stay ignored too.
        assert!(t.event(999, TouchPhase::End, 0.5, 0.5, 0.0).is_none());
        // Lifting a real contact frees capacity again.
        t.event(0, TouchPhase::End, 0.5, 0.5, 0.0).unwrap();
        assert!(t.event(999, TouchPhase::Start, 0.5, 0.5, 1.0).is_some());
    }

    #[test]
    fn cancel_all_lifts_everything() {
        let mut t = TouchTracker::default();
        t.event(1, TouchPhase::Start, 0.1, 0.1, 1.0).unwrap();
        t.event(2, TouchPhase::Start, 0.2, 0.2, 1.0).unwrap();
        let frame = t.cancel_all();
        assert_eq!(
            actions(&frame),
            [ContactAction::Cancel, ContactAction::Cancel]
        );
        assert!(t.is_empty());
    }
}
