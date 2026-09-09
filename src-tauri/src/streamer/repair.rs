//! Loss recovery without a keyframe: the state the transport and the encode thread share.
//!
//! Every recovery in this product used to be an IDR — the largest frame the encoder will ever
//! emit, sent on a link that has just demonstrated it could not carry the previous burst. The two
//! halves of the alternative existed separately and could not meet:
//!
//! - The **encoder** can mark a picture as a long-term reference and later predict only from
//!   marked pictures (`Encoder::mark_ltr` / `use_ltr`), and can drop a lost picture out of the
//!   reference set (`invalidate_reference`). It cannot decide *when*, because it never learns
//!   what arrived.
//! - The **transport** learns exactly what arrived. Transport-wide feedback names every sequence
//!   number the client did and did not receive, and the writer stamps those sequence numbers
//!   itself, so `congestion::SendHistory` can say which whole access units the client holds. It
//!   cannot touch the encoder, which lives on the encode thread.
//!
//! This is the seam. The transport writes receipts; the encode thread reads them at its next
//! submit and turns them into encoder calls. Nothing here blocks, allocates on the encode path
//! beyond a bounded ring, or takes a lock the real-time thread holds for longer than a push.
//!
//! The identity on both sides is the **host-clock capture instant** of the frame. The encoder has
//! its own monotonic frame index, and the transport has the capture stamp it already carries in
//! every `EncodedFrame`; [`RepairState::note_encoded`] is what joins them, so neither side has to
//! learn the other's numbering.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use super::lock::LockExt;

/// How many recent frames to keep a capture-instant → encoder-index mapping for.
///
/// Feedback arrives within a round trip, so this only has to outlive the longest feedback delay
/// worth acting on. At 60 fps this is four seconds.
const MAP_CAPACITY: usize = 256;

/// How many lost frames may queue for invalidation before the oldest is dropped.
///
/// A burst that exceeds this is not a case reference invalidation can repair anyway — the whole
/// reference chain is gone and the answer is a keyframe.
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
    /// The newest encoder frame index the client has confirmed whole.
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

    /// Declare how many long-term reference slots the encoder actually has.
    ///
    /// Called once when the encode thread opens its encoder. Zero — every encoder that does not
    /// support long-term references, which today is all of them except NVENC where the engine
    /// advertises it — leaves [`Self::can_repair`] false for the life of the session, so the
    /// transport keeps asking for keyframes exactly as it did before.
    pub fn set_ltr_slots(&self, slots: u32) {
        self.ltr_slots.store(slots, Ordering::Relaxed);
        let mut guard = self.slots.lock_recover();
        guard.clear();
        guard.resize(slots as usize, None);
        // The first picture after the encoder opens should be a reference, or there is nothing to
        // repair from until the cadence below happens to fire.
        if slots > 0 {
            self.mark_wanted.store(true, Ordering::Relaxed);
        }
    }

    /// Whether a repair can be anything other than a keyframe right now.
    ///
    /// Both halves are required: the encoder must have slots, and the client must have confirmed
    /// holding at least one of them. Asking for a repair against a reference the client never
    /// received produces a picture it cannot decode, which is worse than the keyframe it replaced.
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
        // Re-marking a slot invalidates whatever the client believed about it.
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
    /// Two things follow. Any long-term slot holding a frame at or before it is now known-held, so
    /// a repair may predict from it. And the marking cadence advances: the next picture becomes a
    /// reference, so the newest thing the client is known to hold keeps moving forward instead of
    /// the session repairing from an ever-staler picture.
    pub fn acknowledge(&self, capture_ns: u64) {
        let Some(index) = self.index_for(capture_ns) else {
            return;
        };
        let prev = self.acked_index.fetch_max(index, Ordering::Relaxed);
        if index <= prev {
            return;
        }
        let slots = self.slots.lock_recover().clone();
        let mut bits = 0u32;
        for (slot, held) in slots.iter().enumerate() {
            if held.is_some_and(|marked| marked <= index) {
                bits |= 1u32 << slot;
            }
        }
        if bits != 0 {
            self.acked_slots.fetch_or(bits, Ordering::Relaxed);
            // The reference the client is known to hold has moved forward, so take a new one.
            self.mark_wanted.store(true, Ordering::Relaxed);
        }
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
    /// the caller must fall back to its keyframe request, which is exactly today's behaviour.
    pub fn request_repair(&self) -> bool {
        if !self.can_repair() {
            return false;
        }
        self.repair_wanted.store(true, Ordering::Relaxed);
        self.keyframes_avoided.fetch_add(1, Ordering::Relaxed);
        true
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
        // The encoder opened, so the first picture is marked.
        let action = s.take_action();
        assert!(action.mark_ltr);
        s.note_marked(0, 10);
        encoded(&s, 1_000, 10);

        // Marked but not yet acknowledged: repairing against it would send the client a picture
        // predicted from something it may never have received.
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
        // A second acknowledgement of the same frame is not a new event.
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

        // Slot 0 now holds a picture nobody has confirmed.
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
        // The newest is still resolvable, which is the only one feedback can be about.
        s.set_ltr_slots(1);
        s.take_action();
        s.note_marked(0, MAP_CAPACITY as u64 * 3 - 1);
        s.acknowledge(MAP_CAPACITY as u64 * 3 - 1);
        assert!(s.can_repair());
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
