//! Loss recovery without a keyframe: the state the transport and the encode thread share.
//!
//! The encoder can mark a long-term reference, predict only from marked pictures and drop a lost
//! picture from the reference set (`Encoder::mark_ltr` / `use_ltr` / `invalidate_reference`), but
//! never learns what arrived. The transport learns exactly that from transport-wide feedback
//! (`congestion::SendHistory`), but cannot touch the encoder. So the transport writes receipts
//! here and the encode thread turns them into encoder calls at its next submit. Nothing here
//! blocks, allocates on the encode path beyond a bounded ring, or holds a lock for longer than a
//! push.
//!
//! Both sides name a frame by its host-clock capture instant; [`RepairState::note_encoded`]
//! joins that to the encoder's own frame index.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use super::lock::LockExt;

/// Recent frames kept in the capture-instant to encoder-index map. It only has to outlive the
/// longest feedback delay worth acting on: four seconds at 60 fps.
const MAP_CAPACITY: usize = 256;

/// Lost frames that may queue for invalidation before the oldest is dropped. A longer burst has
/// taken the whole reference chain with it and needs a keyframe anyway.
const INVALIDATE_CAPACITY: usize = 16;

/// What the encode thread should do to the next picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepairAction {
    /// Mark the next picture as a long-term reference.
    pub mark_ltr: bool,
    /// Predict the next picture only from these long-term slots. Zero means "no constraint".
    pub use_ltr_bitmap: u32,
}

impl RepairAction {
    pub fn is_empty(&self) -> bool {
        !self.mark_ltr && self.use_ltr_bitmap == 0
    }
}

/// Shared between the transport and the encode thread for one session.
#[derive(Debug, Default)]
pub struct RepairState {
    /// `(capture_ns, encoder_frame_index)`, oldest first. Written by the encode thread, read by
    /// both.
    map: Mutex<VecDeque<(u64, u64)>>,
    /// Encoder frame indices the client proved it did not receive whole.
    lost: Mutex<VecDeque<u64>>,
    /// Long-term reference slots the encoder has marked: `slot -> encoder frame index`.
    slots: Mutex<Vec<Option<u64>>>,
    /// Slots the client has confirmed receiving whole, as a bitmap.
    acked_slots: AtomicU32,
    /// One past the newest encoder frame index the client has confirmed whole, so zero means
    /// nothing confirmed yet and frame 0 can still be acknowledged.
    acked_index: AtomicU64,
    /// How many long-term slots the encoder has. Zero means every repair must be a keyframe.
    ltr_slots: AtomicU32,
    /// The transport has asked for a repair frame.
    repair_wanted: AtomicBool,
    /// The encode thread should mark the next picture.
    mark_wanted: AtomicBool,
    repairs: AtomicU64,
    invalidations: AtomicU64,
    keyframes_avoided: AtomicU64,
}

impl RepairState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare how many long-term reference slots the encoder has.
    ///
    /// Called when the encode thread opens its encoder, with zero when the engine does not
    /// support invalidation. Zero leaves [`Self::can_repair`] false, so the transport keeps
    /// asking for keyframes. Only NVENC with `SE_NVENC_LTR=1` and the AMF ring with
    /// `amf_ring_ltr_off` switched off open any slots.
    pub fn set_ltr_slots(&self, slots: u32) {
        self.ltr_slots.store(slots, Ordering::Relaxed);
        let mut guard = self.slots.lock_recover();
        guard.clear();
        guard.resize(slots as usize, None);
        // nothing confirmed yet: the previous encoder's slots and frame numbering mean nothing here
        self.acked_slots.store(0, Ordering::Relaxed);
        self.acked_index.store(0, Ordering::Relaxed);
        self.repair_wanted.store(false, Ordering::Relaxed);
        if slots == 0 {
            self.mark_wanted.store(false, Ordering::Relaxed);
        }
        // mark the first picture, or there is nothing to repair from until the cadence fires
        if slots > 0 {
            self.mark_wanted.store(true, Ordering::Relaxed);
        }
    }

    /// Whether a repair can be anything other than a keyframe right now.
    ///
    /// The encoder must have slots and the client must have confirmed holding at least one: a
    /// repair against a reference the client never received is a picture it cannot decode.
    pub fn can_repair(&self) -> bool {
        self.ltr_slots.load(Ordering::Relaxed) > 0 && self.acked_slots.load(Ordering::Relaxed) != 0
    }

    /// The encode thread, after a successful submit: this capture instant is that frame index.
    pub fn note_encoded(&self, capture_ns: u64, frame_index: u64) {
        let mut map = self.map.lock_recover();
        if map.len() == MAP_CAPACITY {
            map.pop_front();
        }
        map.push_back((capture_ns, frame_index));
    }

    /// The encode thread, when it has just marked a picture as a long-term reference.
    pub fn note_marked(&self, slot: u32, frame_index: u64) {
        let mut slots = self.slots.lock_recover();
        if let Some(entry) = slots.get_mut(slot as usize) {
            *entry = Some(frame_index);
        }
        // a re-marked slot is no longer one the client is known to hold
        self.acked_slots
            .fetch_and(!(1u32 << slot), Ordering::Relaxed);
    }

    fn index_for(&self, capture_ns: u64) -> Option<u64> {
        self.map
            .lock_recover()
            .iter()
            .rev()
            .find(|(ns, _)| *ns == capture_ns)
            .map(|(_, idx)| *idx)
    }

    /// The transport: the client has this whole frame.
    ///
    /// Any long-term slot holding a frame at or before it is now known-held, so a repair may
    /// predict from it. The next picture is also marked, so the newest reference the client is
    /// known to hold keeps moving forward instead of going stale.
    pub fn acknowledge(&self, capture_ns: u64) {
        let Some(index) = self.index_for(capture_ns) else {
            return;
        };
        let prev = self
            .acked_index
            .fetch_max(index.saturating_add(1), Ordering::Relaxed);
        if index < prev {
            return;
        }
        // read and set under the slots lock, or an IDR between the two has its evicted slots
        // re-armed by this receipt
        let slots = self.slots.lock_recover();
        let mut bits = 0u32;
        for (slot, held) in slots.iter().enumerate() {
            if held.is_some_and(|marked| marked <= index) {
                bits |= 1u32 << slot;
            }
        }
        if bits != 0 {
            self.acked_slots.fetch_or(bits, Ordering::Relaxed);
            // the newest reference the client holds moved forward, so take a new one
            self.mark_wanted.store(true, Ordering::Relaxed);
        }
        drop(slots);
    }

    /// The transport: the client did not receive this whole frame.
    ///
    /// Queued rather than acted on, because only the encode thread may touch the encoder.
    pub fn note_lost(&self, capture_ns: u64) {
        let Some(index) = self.index_for(capture_ns) else {
            return;
        };
        let mut lost = self.lost.lock_recover();
        if lost.len() == INVALIDATE_CAPACITY {
            lost.pop_front();
        }
        if !lost.contains(&index) {
            lost.push_back(index);
        }
    }

    /// The transport: repair from a reference the client holds instead of sending a keyframe.
    ///
    /// Returns whether the request was taken. `false` means there is nothing to repair from and
    /// the caller must fall back to its keyframe request.
    pub fn request_repair(&self) -> bool {
        if !self.can_repair() {
            return false;
        }
        self.repair_wanted.store(true, Ordering::Relaxed);
        self.keyframes_avoided.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// The encode thread, when it emitted the IDR at `frame_index`: the IDR drops every
    /// long-term reference marked before it on both sides, so none may be named by a repair
    /// until it is marked and confirmed again. A reference marked on the IDR itself survives.
    pub fn note_idr(&self, frame_index: u64) {
        if self.ltr_slots.load(Ordering::Relaxed) == 0 {
            return;
        }
        // empty the slots, not only the confirmations: a later acknowledgement must not re-arm
        // a reference the IDR evicted
        let mut kept = false;
        let mut slots = self.slots.lock_recover();
        for slot in slots.iter_mut() {
            match *slot {
                Some(marked) if marked == frame_index => kept = true,
                _ => *slot = None,
            }
        }
        self.acked_slots.store(0, Ordering::Relaxed);
        drop(slots);
        self.repair_wanted.store(false, Ordering::Relaxed);
        if !kept {
            self.mark_wanted.store(true, Ordering::Relaxed);
        }
    }

    /// The encode thread, before submitting the next picture: what should happen to it.
    pub fn take_action(&self) -> RepairAction {
        let mark = self.mark_wanted.swap(false, Ordering::Relaxed);
        let repair = self.repair_wanted.swap(false, Ordering::Relaxed);
        let bitmap = if repair {
            self.repairs.fetch_add(1, Ordering::Relaxed);
            self.acked_slots.load(Ordering::Relaxed)
        } else {
            0
        };
        RepairAction {
            mark_ltr: mark,
            use_ltr_bitmap: bitmap,
        }
    }

    /// The encode thread: frames to drop out of the reference set, oldest first.
    pub fn take_lost(&self, out: &mut Vec<u64>) {
        out.clear();
        let mut lost = self.lost.lock_recover();
        out.extend(lost.drain(..));
        self.invalidations
            .fetch_add(out.len() as u64, Ordering::Relaxed);
    }

    pub fn repairs(&self) -> u64 {
        self.repairs.load(Ordering::Relaxed)
    }

    pub fn invalidations(&self) -> u64 {
        self.invalidations.load(Ordering::Relaxed)
    }

    pub fn keyframes_avoided(&self) -> u64 {
        self.keyframes_avoided.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(s: &RepairState, ns: u64, idx: u64) {
        s.note_encoded(ns, idx);
    }

    #[test]
    fn an_encoder_without_long_term_references_never_claims_it_can_repair() {
        let s = RepairState::new();
        encoded(&s, 1_000, 1);
        s.acknowledge(1_000);
        assert!(!s.can_repair());
        assert!(
            !s.request_repair(),
            "the caller must fall back to a keyframe"
        );
        assert_eq!(s.take_action(), RepairAction::default());
    }

    #[test]
    fn a_repair_needs_a_reference_the_client_has_actually_confirmed() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        // the encoder opened, so the first picture is marked
        let action = s.take_action();
        assert!(action.mark_ltr);
        s.note_marked(0, 10);
        encoded(&s, 1_000, 10);

        // marked but not acknowledged: a repair would predict from a picture the client may
        // never have received
        assert!(!s.can_repair());
        assert!(!s.request_repair());

        s.acknowledge(1_000);
        assert!(s.can_repair());
        assert!(s.request_repair());
        let action = s.take_action();
        assert_eq!(action.use_ltr_bitmap, 0b01);
        assert_eq!(s.repairs(), 1);
        assert_eq!(s.keyframes_avoided(), 1);
    }

    #[test]
    fn acknowledging_a_frame_advances_the_marking_cadence() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        s.take_action();
        s.note_marked(0, 10);
        encoded(&s, 1_000, 10);
        encoded(&s, 2_000, 20);

        s.acknowledge(1_000);
        assert!(
            s.take_action().mark_ltr,
            "the reference the client holds moved forward, so take a fresh one"
        );
        // a second acknowledgement of the same frame is not a new event
        s.acknowledge(1_000);
        assert!(!s.take_action().mark_ltr);
    }

    #[test]
    fn remarking_a_slot_withdraws_what_the_client_was_believed_to_hold() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        s.take_action();
        s.note_marked(0, 10);
        encoded(&s, 1_000, 10);
        s.acknowledge(1_000);
        assert!(s.can_repair());

        // slot 0 now holds a picture nobody has confirmed
        s.note_marked(0, 40);
        assert!(
            !s.can_repair(),
            "a re-used slot must not still count as held"
        );
    }

    #[test]
    fn a_lost_frame_becomes_one_invalidation_and_is_not_repeated() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        encoded(&s, 5_000, 50);
        s.note_lost(5_000);
        s.note_lost(5_000);
        let mut out = Vec::new();
        s.take_lost(&mut out);
        assert_eq!(out, vec![50]);
        assert_eq!(s.invalidations(), 1);
        s.take_lost(&mut out);
        assert!(out.is_empty(), "the queue is drained, not replayed");
    }

    #[test]
    fn a_capture_instant_the_encoder_never_reported_is_ignored_rather_than_guessed() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        s.acknowledge(999);
        s.note_lost(999);
        let mut out = Vec::new();
        s.take_lost(&mut out);
        assert!(out.is_empty());
        assert!(!s.can_repair());
    }

    #[test]
    fn the_frame_map_stays_bounded_under_a_long_session() {
        let s = RepairState::new();
        for i in 0..(MAP_CAPACITY as u64 * 3) {
            encoded(&s, i, i);
        }
        assert_eq!(s.map.lock_recover().len(), MAP_CAPACITY);
        // the newest is still resolvable, and it is the only one feedback can be about
        s.set_ltr_slots(1);
        s.take_action();
        s.note_marked(0, MAP_CAPACITY as u64 * 3 - 1);
        s.acknowledge(MAP_CAPACITY as u64 * 3 - 1);
        assert!(s.can_repair());
    }

    #[test]
    fn an_idr_or_a_reopened_encoder_forgets_what_the_client_held() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        s.take_action();
        s.note_marked(0, 10);
        encoded(&s, 1_000, 10);
        s.acknowledge(1_000);
        assert!(s.can_repair());
        s.note_idr(12);
        assert!(!s.can_repair(), "an IDR flushes every long-term reference");
        assert!(
            s.take_action().mark_ltr,
            "and the next picture starts a new one"
        );
        // a receipt for a frame after the IDR does not bring back the slot marked before it
        encoded(&s, 1_500, 15);
        s.acknowledge(1_500);
        assert!(!s.can_repair());
        assert!(!s.request_repair());
        assert_eq!(s.take_action().use_ltr_bitmap, 0);

        s.note_marked(1, 20);
        encoded(&s, 2_000, 20);
        s.acknowledge(2_000);
        assert!(s.can_repair());
        // the encoder reopens with the same slot count and its frame numbering starts over
        s.set_ltr_slots(2);
        assert!(!s.can_repair());
        s.take_action();
        s.note_marked(0, 0);
        encoded(&s, 3_000, 0);
        s.acknowledge(3_000);
        assert!(
            s.can_repair(),
            "frame 0 of the new encoder is not behind the old numbering"
        );
    }

    #[test]
    fn a_receipt_racing_an_idr_never_re_arms_an_evicted_slot() {
        let s = RepairState::new();
        for _ in 0..2000 {
            s.set_ltr_slots(2);
            s.note_marked(0, 10);
            encoded(&s, 1_000, 10);
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    s.acknowledge(1_000);
                });
                scope.spawn(|| {
                    barrier.wait();
                    s.note_idr(12);
                });
            });
            let slots = s.slots.lock_recover().clone();
            let acked = s.acked_slots.load(Ordering::Relaxed);
            for (slot, held) in slots.iter().enumerate() {
                assert!(
                    held.is_some() || acked & (1 << slot) == 0,
                    "slot {slot} is empty but confirmed"
                );
            }
        }
    }

    #[test]
    fn a_reference_marked_on_the_idr_itself_survives_it() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        s.take_action();
        s.note_marked(0, 10);
        encoded(&s, 1_000, 10);
        s.acknowledge(1_000);
        s.take_action();
        s.note_marked(1, 12);
        encoded(&s, 1_200, 12);
        s.note_idr(12);
        assert!(
            !s.take_action().mark_ltr,
            "the IDR already is the new reference"
        );
        s.acknowledge(1_200);
        assert!(s.request_repair());
        assert_eq!(
            s.take_action().use_ltr_bitmap,
            0b10,
            "only the IDR's own slot, never the one it evicted"
        );
    }

    #[test]
    fn the_first_frame_of_a_session_can_be_acknowledged() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        assert!(s.take_action().mark_ltr);
        s.note_marked(0, 0);
        encoded(&s, 1_000, 0);
        s.acknowledge(1_000);
        assert!(s.can_repair(), "frame 0 is a reference like any other");
        s.acknowledge(1_000);
        assert!(s.take_action().mark_ltr);
        assert!(!s.take_action().mark_ltr, "a repeated receipt is not news");
    }

    #[test]
    fn dropping_to_zero_slots_forgets_what_was_held_and_what_was_asked() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        s.take_action();
        s.note_marked(1, 10);
        encoded(&s, 1_000, 10);
        s.acknowledge(1_000);
        assert!(s.request_repair());

        s.set_ltr_slots(0);
        assert!(!s.can_repair());
        assert_eq!(
            s.take_action(),
            RepairAction::default(),
            "no repair may name a slot the encoder no longer has"
        );
    }

    #[test]
    fn an_out_of_order_acknowledgement_never_walks_the_reference_backwards() {
        let s = RepairState::new();
        s.set_ltr_slots(2);
        s.take_action();
        s.note_marked(0, 10);
        encoded(&s, 1_000, 10);
        encoded(&s, 2_000, 20);
        s.acknowledge(2_000);
        s.take_action();
        s.acknowledge(1_000);
        assert!(
            !s.take_action().mark_ltr,
            "an older receipt is not new information"
        );
    }
}
