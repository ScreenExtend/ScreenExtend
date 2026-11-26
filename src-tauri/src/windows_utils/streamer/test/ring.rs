//! The async encode ring's decisions, driven one event at a time against a fake encoder.
//!
//! Declared from `pipeline.rs` (not `test/mod.rs`) so it can reach the encode thread's private
//! state. The decisions are all made in `RingWorker`, with `RingEngine` as the seam, so only
//! the `keyed_stage` tests, which stage into a real cross-device keyed-mutex texture, need a GPU.

use super::*;
use crate::streamer::repair::RepairState;
use crate::streamer::telemetry::SessionTelemetry;

/// An IDR access unit: SPS, PPS, then an IDR slice.
const IDR_AU: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 0, 1, 0x65, 0x88,
    0x84,
];
const P_AU: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];

#[derive(Default)]
struct Fake {
    frame_index: u64,
    ltr_slots: u32,
    /// `invalidates_references()`.
    invalidates: bool,
    /// What `invalidate_reference` answers when it is asked.
    invalidation: Option<RefInvalidation>,
    /// Every encode fails with an encode error while this is non-zero (decremented per failure).
    fail_encodes: u32,
    device_lost: bool,
    /// The next `encode_slot` reports its slot poisoned.
    poison_next: bool,
    /// `use_ltr` answers false: the slots it names are gone (AMF after an IDR or a re-init).
    refuse_ltr: bool,
    /// Sleep inside `encode_repeat_slot`, so a stamp taken after the encode is visibly late.
    repeat_delay: Duration,
    shared_idr: Option<Arc<AtomicBool>>,
    idr_requested: bool,
    /// Observes the free pool from inside an encode, to see what the stage could take meanwhile.
    free_probe: Option<crossbeam_channel::Receiver<usize>>,
    /// What `set_bitrate` answers; `Applied` when unset.
    bitrate_answer: Option<BitrateApplied>,
    // what happened
    encoded: Vec<(usize, bool)>,
    repeats: Vec<(usize, bool)>,
    repeat_entered: Vec<Instant>,
    used_ltr: Vec<u32>,
    marked: u32,
    invalidated: Vec<u64>,
    idr_requests: u32,
    free_during_encode: Vec<usize>,
    /// Counts the encoder's drop, which is where a real one closes its hardware session.
    retired: Retired,
}

#[derive(Default)]
struct Retired(Option<Arc<std::sync::atomic::AtomicUsize>>);

impl Drop for Retired {
    fn drop(&mut self) {
        if let Some(n) = &self.0 {
            n.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Fake {
    fn take_idr(&mut self, force: bool) -> bool {
        let shared = self
            .shared_idr
            .as_ref()
            .is_some_and(|f| f.swap(false, Ordering::Relaxed));
        let own = std::mem::take(&mut self.idr_requested);
        force || own || shared
    }

    fn au(idr: bool) -> Vec<u8> {
        if idr { IDR_AU } else { P_AU }.to_vec()
    }
}

impl RingEngine for Fake {
    fn name(&self) -> &'static str {
        "fake-ring"
    }
    fn attach_idr_flag(&mut self, flag: Arc<AtomicBool>) {
        self.shared_idr = Some(flag);
    }
    fn holds_gpu_clock(&self) -> bool {
        false
    }
    fn request_idr(&mut self) {
        self.idr_requests += 1;
        self.idr_requested = true;
    }
    fn set_bitrate(&mut self, _bps: u32) -> Result<BitrateApplied> {
        Ok(self.bitrate_answer.unwrap_or(BitrateApplied::Applied))
    }
    fn ltr_slots(&self) -> u32 {
        self.ltr_slots
    }
    fn next_frame_index(&self) -> u64 {
        self.frame_index
    }
    fn mark_ltr(&mut self) -> Option<u32> {
        let slot = self.marked % self.ltr_slots.max(1);
        self.marked += 1;
        (self.ltr_slots > 0).then_some(slot)
    }
    fn use_ltr(&mut self, bitmap: u32) -> bool {
        self.used_ltr.push(bitmap);
        !self.refuse_ltr
    }
    fn invalidates_references(&self) -> bool {
        self.invalidates
    }
    fn invalidate_reference(&mut self, frame_index: u64) -> RefInvalidation {
        self.invalidated.push(frame_index);
        self.invalidation.unwrap_or(RefInvalidation::Unsupported)
    }
    fn encode_slot(&mut self, slot: usize, force_idr: bool) -> Result<Vec<u8>, SlotEncodeError> {
        if let Some(rx) = &self.free_probe {
            self.free_during_encode.push(rx.len());
        }
        if std::mem::take(&mut self.poison_next) {
            return Err(SlotEncodeError::Poisoned(anyhow!(
                "fake: slot {slot} wedged"
            )));
        }
        if self.fail_encodes > 0 {
            self.fail_encodes -= 1;
            return Err(SlotEncodeError::Encode(anyhow!("fake: encode failed")));
        }
        let idr = self.take_idr(force_idr);
        self.encoded.push((slot, idr));
        self.frame_index += 1;
        Ok(Self::au(idr))
    }
    fn encode_repeat_slot(&mut self, slot: usize) -> Result<Vec<u8>> {
        self.repeat_entered.push(Instant::now());
        if !self.repeat_delay.is_zero() {
            std::thread::sleep(self.repeat_delay);
        }
        if self.fail_encodes > 0 {
            self.fail_encodes -= 1;
            bail!("fake: repeat failed");
        }
        let idr = self.take_idr(false);
        self.repeats.push((slot, idr));
        self.frame_index += 1;
        Ok(Self::au(idr))
    }
    fn device_lost(&self) -> bool {
        self.device_lost
    }
}

const ALL_ON: RingSwitches = RingSwitches {
    latest_wins: true,
    yield_to_frame: true,
    stamp_contract: true,
    give_up_on_streak: true,
};
const SHIPPED: RingSwitches = RingSwitches {
    latest_wins: false,
    yield_to_frame: true,
    stamp_contract: true,
    give_up_on_streak: false,
};
/// What ships once the WGC handler propagates `RingFailed`.
const GIVE_UP: RingSwitches = RingSwitches {
    give_up_on_streak: true,
    ..SHIPPED
};
const ALL_OFF: RingSwitches = RingSwitches {
    latest_wins: false,
    yield_to_frame: false,
    stamp_contract: false,
    give_up_on_streak: false,
};

struct Rig {
    worker: RingWorker<Fake>,
    free_rx: crossbeam_channel::Receiver<usize>,
    ready_tx: crossbeam_channel::Sender<ReadyMsg>,
    out: broadcast::Receiver<EncodedFrame>,
    repair: Arc<RepairState>,
    telemetry: SharedTelemetry,
    idr: Arc<AtomicBool>,
    poisoned: Arc<PoisonedSlots>,
    unencoded: Arc<UnencodedSlots>,
}

fn rig(mut fake: Fake, switches: RingSwitches) -> Rig {
    // unbounded so nothing here ever blocks on a send; the tests drive slots explicitly
    let (free_tx, free_rx) = crossbeam_channel::unbounded::<usize>();
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<ReadyMsg>(3);
    let (_wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let (tx, out) = broadcast::channel(64);
    let repair = Arc::new(RepairState::new());
    let telemetry = SessionTelemetry::new_shared();
    let idr = Arc::new(AtomicBool::new(false));
    let poisoned = Arc::new(PoisonedSlots::default());
    let unencoded = Arc::new(UnencodedSlots::default());
    fake.attach_idr_flag(Arc::clone(&idr));
    // `RepairState::acknowledge` can never acknowledge frame index 0 (its high-water mark starts
    // there), so the fake numbers its pictures from 1
    fake.frame_index = fake.frame_index.max(1);
    if fake.free_probe.is_none() {
        fake.free_probe = Some(free_rx.clone());
    }
    let worker = RingWorker::new(RingEncodeArgs {
        encoder: fake,
        cursor: None,
        ready_rx,
        free_tx,
        tx,
        target_bitrate: Arc::new(AtomicU32::new(0)),
        initial_bitrate: 6_000_000,
        idr_request: Arc::clone(&idr),
        repair: Arc::clone(&repair),
        wake_rx,
        frame_duration: Duration::from_millis(16),
        stop: Arc::new(AtomicBool::new(false)),
        failed: Arc::new(AtomicBool::new(false)),
        poisoned: Arc::clone(&poisoned),
        unencoded: Arc::clone(&unencoded),
        slots: 3,
        telemetry: Arc::clone(&telemetry),
        switches,
    });
    Rig {
        worker,
        free_rx,
        ready_tx,
        out,
        repair,
        telemetry,
        idr,
        poisoned,
        unencoded,
    }
}

fn frame(slot: usize, force_idr: bool) -> ReadyMsg {
    ReadyMsg {
        slot,
        force_idr,
        capture: Instant::now(),
        timing: FrameTiming::entering_capture(),
    }
}

fn ns(i: Instant) -> u64 {
    crate::streamer::audio::host_instant_to_ns(i)
}

/// A tick that is well inside the keepalive period, so only an IDR, a cursor move or a repair
/// may make it emit.
fn quiet_tick() -> Instant {
    Instant::now()
}

/// A tick past the keepalive.
fn keepalive_tick() -> Instant {
    Instant::now() + Duration::from_secs(1)
}

fn drain_free(rx: &crossbeam_channel::Receiver<usize>) -> Vec<usize> {
    rx.try_iter().collect()
}

impl Rig {
    fn encode(&mut self, slot: usize, force_idr: bool) -> EncodedFrame {
        assert_eq!(
            self.worker.on_frame(frame(slot, force_idr)),
            RingStep::Continue
        );
        self.out.try_recv().expect("an encoded frame was broadcast")
    }
}

#[test]
fn a_repair_taken_on_a_still_desktop_reaches_a_picture() {
    let mut r = rig(
        Fake {
            ltr_slots: 2,
            invalidates: true,
            invalidation: Some(RefInvalidation::Applied),
            ..Fake::default()
        },
        SHIPPED,
    );
    let first = r.encode(0, true);
    assert_eq!(r.worker.encoder.marked, 1, "the first picture is marked");
    r.repair.acknowledge(ns(first.capture));
    assert!(r.repair.can_repair());

    // a loss on a still desktop: the transport takes a repair instead of an IDR, and nothing
    // is captured after it
    assert!(r.repair.request_repair());
    assert_eq!(r.worker.on_idle(quiet_tick()), RingStep::Continue);

    assert_eq!(
        r.worker.encoder.used_ltr,
        vec![0b1],
        "the repair must be put on a picture although no frame arrived"
    );
    assert_eq!(r.worker.encoder.repeats.len(), 1, "and that picture sent");
    assert!(r.out.try_recv().is_ok());
    assert_eq!(r.repair.repairs(), 1);
}

#[test]
fn a_quiet_tick_with_nothing_due_emits_nothing_and_keeps_a_mark() {
    let mut r = rig(
        Fake {
            ltr_slots: 2,
            invalidates: true,
            invalidation: Some(RefInvalidation::Applied),
            ..Fake::default()
        },
        SHIPPED,
    );
    let first = r.encode(0, true);
    // the ack asks for a new mark: a quiet tick reads it but must not lose it
    r.repair.acknowledge(ns(first.capture));
    assert_eq!(r.worker.on_idle(quiet_tick()), RingStep::Continue);
    assert!(r.worker.encoder.repeats.is_empty());
    assert!(r.out.try_recv().is_err());
    r.encode(1, false);
    assert_eq!(
        r.worker.encoder.marked, 2,
        "the mark read on the quiet tick lands on the next picture"
    );
}

#[test]
fn an_engine_that_repairs_by_bitfield_keeps_repairing_after_a_loss() {
    // AMF's shape: long-term slots, no reference invalidation
    let mut r = rig(
        Fake {
            ltr_slots: 2,
            invalidates: false,
            ..Fake::default()
        },
        SHIPPED,
    );
    let first = r.encode(0, true);
    r.repair.acknowledge(ns(first.capture));
    let second = r.encode(1, false);
    r.repair.note_lost(ns(second.capture));
    r.encode(2, false);

    assert!(
        r.worker.encoder.invalidated.is_empty(),
        "an engine without invalidation is never asked"
    );
    assert!(
        r.repair.can_repair(),
        "one lost frame must not switch the forced-bitfield repair off for the session"
    );
    assert!(r.repair.request_repair());
    r.encode(0, false);
    assert_eq!(r.worker.encoder.used_ltr, vec![0b1]);
}

#[test]
fn an_engine_that_cannot_invalidate_stops_repairing_and_drops_what_was_pending() {
    // NVENC's shape when the invalidation entry point is missing
    let mut r = rig(
        Fake {
            ltr_slots: 2,
            invalidates: true,
            invalidation: Some(RefInvalidation::Unsupported),
            ..Fake::default()
        },
        SHIPPED,
    );
    let first = r.encode(0, true);
    r.repair.acknowledge(ns(first.capture));
    assert!(r.repair.request_repair());
    let second = r.encode(1, false);
    assert_eq!(r.worker.encoder.used_ltr, vec![0b1]);
    r.repair.note_lost(ns(second.capture));
    // the repair wanted here is read in the same submit that learns the engine cannot repair
    assert!(r.repair.request_repair());
    r.encode(2, false);
    assert!(!r.repair.can_repair());
    assert_eq!(
        r.worker.encoder.used_ltr,
        vec![0b1],
        "no long-term constraint after the slots were declared gone"
    );
}

/// A repair the encoder refuses (AMF has forgotten its long-term slots on an IDR or a QP re-init)
/// is answered with a keyframe on the very picture it was meant for, not left unanswered.
#[test]
fn a_repair_the_encoder_refuses_becomes_a_keyframe() {
    let mut r = rig(
        Fake {
            ltr_slots: 2,
            invalidates: false,
            ..Fake::default()
        },
        SHIPPED,
    );
    let first = r.encode(0, true);
    r.repair.acknowledge(ns(first.capture));
    r.encode(1, false);
    r.worker.encoder.refuse_ltr = true;
    assert!(r.repair.request_repair());
    let before = r.worker.encoder.idr_requests;
    r.encode(2, false);
    assert_eq!(
        r.worker.encoder.used_ltr,
        vec![0b1],
        "the repair was asked for"
    );
    assert_eq!(
        r.worker.encoder.idr_requests,
        before + 1,
        "and answered with an IDR"
    );
    assert_eq!(
        r.worker.encoder.encoded.last(),
        Some(&(2, true)),
        "on the same picture"
    );
}

#[test]
fn a_keepalive_is_in_the_repair_map() {
    let mut r = rig(Fake::default(), SHIPPED);
    r.encode(0, true);
    assert_eq!(r.worker.on_idle(keepalive_tick()), RingStep::Continue);
    let repeat = r.out.try_recv().expect("the keepalive went out");
    r.repair.note_lost(ns(repeat.capture));
    let mut lost = Vec::new();
    r.repair.take_lost(&mut lost);
    assert_eq!(
        lost,
        vec![2],
        "a lost keepalive must resolve to the encoder frame index it consumed"
    );
}

#[test]
fn a_repeat_is_stamped_at_its_submit_not_after_its_encode() {
    let mut r = rig(
        Fake {
            repeat_delay: Duration::from_millis(5),
            ..Fake::default()
        },
        SHIPPED,
    );
    r.encode(0, true);
    r.worker.on_idle(keepalive_tick());
    let repeat = r.out.try_recv().expect("the keepalive went out");
    let entered = r.worker.encoder.repeat_entered[0];
    assert!(
        repeat.capture <= entered,
        "stamped {:?} after the encode was entered",
        repeat.capture.saturating_duration_since(entered)
    );
}

#[test]
fn with_the_stamp_switch_off_a_repeat_is_stamped_after_its_encode_as_before() {
    let mut r = rig(
        Fake {
            repeat_delay: Duration::from_millis(5),
            ..Fake::default()
        },
        ALL_OFF,
    );
    r.encode(0, true);
    r.worker.on_idle(keepalive_tick());
    let repeat = r.out.try_recv().expect("the keepalive went out");
    assert!(repeat.capture >= r.worker.encoder.repeat_entered[0] + Duration::from_millis(5));
}

#[test]
fn a_host_clock_stamp_converts_to_the_instant_it_names() {
    let then = crate::streamer::audio::host_now_ns();
    std::thread::sleep(Duration::from_millis(2));
    let instant = instant_at_host_ns(then);
    assert_eq!(ns(instant), then, "exact, not approximately");
    assert!(instant < Instant::now());
    // no stamp, or one from the future, is "now"
    let before = Instant::now();
    assert!(instant_at_host_ns(0) >= before);
    assert!(instant_at_host_ns(u64::MAX) >= before);
}

#[test]
fn latest_wins_encodes_the_newest_queued_frame_and_frees_two_slots_while_encoding() {
    let mut r = rig(Fake::default(), ALL_ON);
    r.encode(0, true);
    assert!(drain_free(&r.free_rx).is_empty());
    r.worker.encoder.free_during_encode.clear();

    // overload: slot 1 was queued, and slot 2 was staged behind it before the encoder got there
    r.ready_tx.send(frame(2, false)).unwrap();
    assert_eq!(r.worker.on_frame(frame(1, true)), RingStep::Continue);

    assert_eq!(
        r.worker.encoder.encoded.last(),
        Some(&(2, true)),
        "the newest frame is encoded, and it inherits the older one's keyframe demand"
    );
    assert_eq!(
        r.worker.encoder.free_during_encode,
        vec![2],
        "the skipped slot and the previously held slot are both free while the encoder works"
    );
    let mut free = drain_free(&r.free_rx);
    free.sort_unstable();
    assert_eq!(free, vec![0, 1]);
    assert!(
        r.unencoded.take(1),
        "the skipped slot is flagged so the stage re-acquires it at the encoder's key"
    );
    assert!(
        !r.unencoded.take(0),
        "the held slot went through the encoder"
    );
    assert_eq!(r.worker.held_slot, Some(2));
    assert_eq!(
        r.telemetry
            .degradation
            .frames_dropped_ring
            .load(Ordering::Relaxed),
        1
    );
}

#[test]
fn with_latest_wins_off_the_ring_encodes_in_order_as_before() {
    let mut r = rig(Fake::default(), SHIPPED);
    r.encode(0, true);
    r.worker.encoder.free_during_encode.clear();
    r.ready_tx.send(frame(2, false)).unwrap();
    assert_eq!(r.worker.on_frame(frame(1, false)), RingStep::Continue);
    assert_eq!(r.worker.encoder.encoded.last(), Some(&(1, false)));
    assert_eq!(r.worker.encoder.free_during_encode, vec![0]);
    assert_eq!(drain_free(&r.free_rx), vec![0]);
    assert_eq!(r.worker.held_slot, Some(1));
}

#[test]
fn an_unencoded_slot_flag_is_read_once() {
    let s = UnencodedSlots::default();
    assert!(!s.take(1));
    s.mark(1);
    s.mark(40); // out of range: ignored, never panics
    assert!(!s.take(0));
    assert!(s.take(1));
    assert!(!s.take(1));
    assert!(!s.take(40));
}

#[test]
fn a_wake_does_not_reencode_the_held_slot_ahead_of_a_queued_frame() {
    let mut r = rig(Fake::default(), SHIPPED);
    r.encode(0, true);
    r.idr.store(true, Ordering::Relaxed);
    r.ready_tx.send(frame(1, false)).unwrap();
    assert_eq!(r.worker.on_idle(keepalive_tick()), RingStep::Continue);
    assert!(
        r.worker.encoder.repeats.is_empty(),
        "no keyframe of the old picture"
    );
    assert!(
        r.idr.load(Ordering::Relaxed),
        "the request is left for the queued frame to take"
    );
    let queued = r.worker.ready_rx.try_recv().unwrap();
    r.worker.on_frame(queued);
    assert_eq!(r.worker.encoder.encoded.last(), Some(&(1, true)));
}

#[test]
fn with_the_yield_off_a_wake_reencodes_the_held_slot_as_before() {
    let mut r = rig(Fake::default(), ALL_OFF);
    r.encode(0, true);
    r.idr.store(true, Ordering::Relaxed);
    r.ready_tx.send(frame(1, false)).unwrap();
    r.worker.on_idle(keepalive_tick());
    assert_eq!(r.worker.encoder.repeats, vec![(0, true)]);
}

#[test]
fn idrs_are_counted_from_what_the_encoder_emitted() {
    let mut r = rig(Fake::default(), SHIPPED);
    let idrs = |r: &Rig| r.telemetry.degradation.idrs_sent.load(Ordering::Relaxed);
    r.encode(0, true);
    assert_eq!(idrs(&r), 1);
    r.encode(1, false);
    assert_eq!(idrs(&r), 1);
    // a PLI lands between the stage and the submit: the stage said P, the encoder reads the
    // shared flag at submit and emits an IDR
    r.idr.store(true, Ordering::Relaxed);
    r.encode(2, false);
    assert_eq!(r.worker.encoder.encoded.last(), Some(&(2, true)));
    assert_eq!(
        idrs(&r),
        2,
        "an IDR the stage did not decide is still an IDR"
    );
}

#[test]
fn an_access_unit_is_an_idr_only_if_its_first_slice_is() {
    assert!(access_unit_is_idr(IDR_AU));
    assert!(!access_unit_is_idr(P_AU));
    // three-byte start codes and an SEI ahead of the slice
    assert!(access_unit_is_idr(&[
        0, 0, 1, 0x06, 5, 1, 0x80, 0, 0, 1, 0x65, 0x88
    ]));
    assert!(!access_unit_is_idr(&[
        0, 0, 1, 0x06, 5, 1, 0x80, 0, 0, 1, 0x41, 0x9a
    ]));
    // an IDR-looking header after a start-code-like run inside a P slice's payload is never
    // reached: the scan stops at the first slice
    assert!(!access_unit_is_idr(&[0, 0, 1, 0x41, 0, 0, 3, 1, 0x65]));
    assert!(!access_unit_is_idr(&[]));
    assert!(!access_unit_is_idr(&[0, 0, 1]));
}

#[test]
fn a_failed_encode_makes_the_next_picture_a_keyframe() {
    let mut r = rig(Fake::default(), SHIPPED);
    r.encode(0, true);
    // the picture after a failed encode must be an IDR: the failed one may already be a
    // reference the client will never receive
    r.worker.encoder.fail_encodes = 1;
    assert_eq!(r.worker.on_frame(frame(1, false)), RingStep::Continue);
    assert!(
        r.out.try_recv().is_err(),
        "nothing was sent for the failure"
    );
    assert_eq!(r.worker.held_slot, Some(1), "the slot is still recycled");
    r.encode(2, false);
    assert_eq!(r.worker.encoder.encoded.last(), Some(&(2, true)));
}

#[test]
fn an_encoder_that_keeps_failing_ends_the_ring() {
    let mut r = rig(Fake::default(), GIVE_UP);
    r.encode(0, true);
    r.worker.encoder.fail_encodes = u32::MAX;
    for i in 1..MAX_TRANSIENT_ENCODE_DROPS {
        assert_eq!(
            r.worker.on_frame(frame((i % 3) as usize, false)),
            RingStep::Continue,
            "failure {i} is still transient"
        );
    }
    assert!(matches!(
        r.worker.on_frame(frame(0, false)),
        RingStep::Fail(_)
    ));
}

#[test]
fn keepalive_failures_count_toward_the_same_bound() {
    let mut r = rig(Fake::default(), GIVE_UP);
    r.encode(0, true);
    r.worker.encoder.fail_encodes = u32::MAX;
    for _ in 1..MAX_TRANSIENT_ENCODE_DROPS {
        assert_eq!(r.worker.on_idle(keepalive_tick()), RingStep::Continue);
    }
    assert!(matches!(
        r.worker.on_idle(keepalive_tick()),
        RingStep::Fail(_)
    ));
}

#[test]
fn with_the_give_up_switch_off_a_failing_encoder_keeps_retrying_and_recovers() {
    // until the handler halts the capture on `RingFailed`, a ring that ends stays dead for the
    // session, while one that retries comes back when the errors stop
    let mut r = rig(Fake::default(), SHIPPED);
    r.encode(0, true);
    r.worker.encoder.fail_encodes = 4 * MAX_TRANSIENT_ENCODE_DROPS;
    for i in 0..4 * MAX_TRANSIENT_ENCODE_DROPS {
        assert_eq!(
            r.worker.on_frame(frame((i % 3) as usize, false)),
            RingStep::Continue,
            "failure {i} must not end the ring"
        );
    }
    r.encode(1, false);
    assert_eq!(
        r.worker.encoder.encoded.last(),
        Some(&(1, true)),
        "the first picture after the failures is a keyframe"
    );
}

#[test]
fn a_removed_device_ends_the_ring_at_the_first_failure() {
    let mut r = rig(Fake::default(), SHIPPED);
    r.encode(0, true);
    r.worker.encoder.fail_encodes = 1;
    r.worker.encoder.device_lost = true;
    assert!(matches!(
        r.worker.on_frame(frame(1, false)),
        RingStep::Fail(_)
    ));
}

#[test]
fn a_success_resets_the_failure_streak() {
    let mut r = rig(Fake::default(), SHIPPED);
    r.encode(0, true);
    for _ in 0..3 {
        r.worker.encoder.fail_encodes = MAX_TRANSIENT_ENCODE_DROPS - 1;
        for i in 0..MAX_TRANSIENT_ENCODE_DROPS - 1 {
            assert_eq!(
                r.worker.on_frame(frame((i % 3) as usize, false)),
                RingStep::Continue
            );
        }
        r.encode(1, false);
    }
}

#[test]
fn one_poisoned_slot_does_not_end_the_ring_two_do() {
    let mut r = rig(Fake::default(), SHIPPED);
    // the very first frame's slot is wedged, so there is no held slot yet: that must not stop
    // the encode thread
    r.worker.encoder.poison_next = true;
    assert_eq!(r.worker.on_frame(frame(0, true)), RingStep::Continue);
    assert!(r.poisoned.contains(0));
    r.encode(1, true);
    assert_eq!(r.worker.held_slot, Some(1));
    // a second wedged slot leaves one usable slot, which the encode thread holds: the ring can
    // never stage again
    r.worker.encoder.poison_next = true;
    assert!(matches!(
        r.worker.on_frame(frame(2, false)),
        RingStep::Fail(_)
    ));
}

#[test]
fn a_frame_for_a_quarantined_slot_is_dropped() {
    let mut r = rig(Fake::default(), SHIPPED);
    r.poisoned.mark(2);
    assert_eq!(r.worker.on_frame(frame(2, true)), RingStep::Continue);
    assert!(r.worker.encoder.encoded.is_empty());
}

mod keyed_stage {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
        D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::Dxgi::{
        IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
    };

    const W: u32 = 64;
    const H: u32 = 64;

    fn device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
        let levels = [D3D_FEATURE_LEVEL_11_0];
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        Ok((device.unwrap(), context.unwrap()))
    }

    fn desc(misc: u32) -> D3D11_TEXTURE2D_DESC {
        D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc,
        }
    }

    fn texture(device: &ID3D11Device, misc: u32) -> windows::core::Result<ID3D11Texture2D> {
        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc(misc), None, Some(&mut tex))? };
        Ok(tex.unwrap())
    }

    /// One slot: created on the "encoder" device `a`, opened on the "capture" device `b`.
    /// Returns `a`'s keyed mutex and `b`'s texture and keyed mutex.
    fn shared_slot(
        a: &ID3D11Device,
        b: &ID3D11Device,
    ) -> windows::core::Result<(IDXGIKeyedMutex, ID3D11Texture2D, IDXGIKeyedMutex)> {
        let misc = (D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
            as u32;
        let tex_a = texture(a, misc)?;
        let resource: IDXGIResource1 = tex_a.cast()?;
        let handle = unsafe {
            resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                PCWSTR::null(),
            )?
        };
        let b1: ID3D11Device1 = b.cast()?;
        let opened: windows::core::Result<ID3D11Texture2D> =
            unsafe { b1.OpenSharedResource1(handle) };
        unsafe {
            let _ = CloseHandle(handle);
        }
        let tex_b = opened?;
        Ok((tex_a.cast()?, tex_b.clone(), tex_b.cast()?))
    }

    struct Setup {
        ring: AsyncEncodeRing,
        raw: ID3D11Texture2D,
        a_mutexes: Vec<IDXGIKeyedMutex>,
        b_mutexes: Vec<IDXGIKeyedMutex>,
        poisoned: Arc<PoisonedSlots>,
    }

    fn setup() -> windows::core::Result<Setup> {
        setup_with(Fake::default())
    }

    fn setup_with(encoder: Fake) -> windows::core::Result<Setup> {
        let (a, _a_ctx) = device()?;
        let (b, b_ctx) = device()?;
        let mut a_mutexes = Vec::new();
        let mut textures = Vec::new();
        let mut b_mutexes = Vec::new();
        for _ in 0..3 {
            let (ma, tb, mb) = shared_slot(&a, &b)?;
            a_mutexes.push(ma);
            textures.push(tb);
            b_mutexes.push(mb);
        }
        let raw = texture(&b, 0)?;
        let (tx, _rx) = broadcast::channel(16);
        let (_wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
        let ring = AsyncEncodeRing::new(
            encoder,
            RingTextures::Shared {
                textures,
                mutexes: b_mutexes.clone(),
            },
            b_ctx,
            None,
            None,
            tx,
            Arc::new(AtomicU32::new(0)),
            6_000_000,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(RepairState::new()),
            wake_rx,
            Duration::from_millis(16),
            Instant::now(),
            SessionTelemetry::new_shared(),
        );
        let poisoned = Arc::clone(&ring.poisoned);
        Ok(Setup {
            ring,
            raw,
            a_mutexes,
            b_mutexes,
            poisoned,
        })
    }

    /// A capture that is opened again must not overlap the one it replaces: the ring's drop (what
    /// a stopped capture's handler runs) returns only once its encode thread has ended and the
    /// encoder, with its hardware session, is gone.
    #[test]
    fn a_dropped_ring_has_retired_its_encoder_before_the_drop_returns() {
        let retired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = match setup_with(Fake {
            retired: Retired(Some(Arc::clone(&retired))),
            ..Fake::default()
        }) {
            Ok(s) => s,
            Err(e) => {
                crate::test_support::skip("ring drop against a shared keyed mutex", e);
                return;
            }
        };
        assert_eq!(retired.load(Ordering::SeqCst), 0, "the encoder is alive");
        drop(s.ring);
        assert_eq!(
            retired.load(Ordering::SeqCst),
            1,
            "the encoder was dropped, once, before the ring's drop returned"
        );
    }

    #[test]
    fn a_keyed_mutex_timeout_is_a_dropped_frame_and_a_quarantined_slot() {
        let mut s = match setup() {
            Ok(s) => s,
            Err(e) => {
                crate::test_support::skip("ring stage against a shared keyed mutex", e);
                return;
            }
        };
        // someone else (a hung encoder) owns slot 0 at the writer's key and never lets go
        match crate::windows_utils::streamer::keyed::acquire(&s.a_mutexes[0], KEY_WRITER, 0) {
            Ok(crate::windows_utils::streamer::keyed::Acquired::Owned) => {}
            other => {
                crate::test_support::skip("ring stage: could not hold slot 0", other);
                return;
            }
        }

        let staged = s
            .ring
            .stage(&s.raw, FrameTiming::entering_capture())
            .expect("a wedged slot is a dropped frame, not a dead ring");
        assert!(!staged, "the frame in the wedged slot is dropped");
        assert!(s.poisoned.contains(0), "and the slot is quarantined");

        // the next frame takes the next slot, which nobody holds, and goes through
        let staged = s
            .ring
            .stage(&s.raw, FrameTiming::entering_capture())
            .expect("the ring is still alive");
        assert!(staged);

        // the stage never released a key it did not own: slot 0 is still the holder's
        unsafe { s.a_mutexes[0].ReleaseSync(KEY_WRITER) }
            .expect("the original owner still owns slot 0");
        drop(s.b_mutexes);
        s.ring.stop();
    }
}

#[test]
fn an_engine_that_cannot_change_its_bitrate_is_not_asked_again() {
    let mut fake = Fake {
        bitrate_answer: Some(BitrateApplied::Unsupported),
        ..Default::default()
    };
    let target = AtomicU32::new(3_000_000);
    let (mut current, mut warned) = (8_000_000, false);
    apply_pending_bitrate_atomic(&mut fake, &target, &mut current, &mut warned);
    assert_eq!(target.load(Ordering::Relaxed), 0, "not parked for a retry");
    assert_eq!(current, 3_000_000);
    assert!(warned, "said once that adaptation is inert");
}

#[test]
fn a_bitrate_held_to_the_encoders_limit_is_not_retried() {
    let mut fake = Fake {
        bitrate_answer: Some(BitrateApplied::Clamped {
            applied_bps: 65_000_000,
        }),
        ..Default::default()
    };
    let target = AtomicU32::new(90_000_000);
    let (mut current, mut warned) = (40_000_000, false);
    apply_pending_bitrate_atomic(&mut fake, &target, &mut current, &mut warned);
    assert_eq!(target.load(Ordering::Relaxed), 0);
    assert_eq!(current, 90_000_000);
    assert!(!warned, "a clamp is not an inert encoder");
}

#[test]
fn a_released_pipeline_handle_lets_the_channel_close_with_the_capture() {
    let (capture_tx, _) = broadcast::channel::<EncodedFrame>(4);
    let (wake, _wake_rx) = crossbeam_channel::bounded(1);
    let mut pipeline = Pipeline {
        tx: capture_tx.clone(),
        frame_duration: Duration::from_millis(16),
        telemetry: SessionTelemetry::new_shared(),
        idr_request: Arc::new(AtomicBool::new(false)),
        target_bitrate: Arc::new(AtomicU32::new(0)),
        wake,
        max_bitrate_bps: 8_000_000,
        h264_profile: Default::default(),
        repair: Arc::new(RepairState::new()),
        frames: None,
    };
    let mut held = pipeline.subscribe();
    pipeline.release_sender();
    let mut rx = pipeline.subscribe();
    // the capture's own sender goes with the capture
    drop(capture_tx);
    assert!(matches!(
        rx.try_recv(),
        Err(broadcast::error::TryRecvError::Closed)
    ));
    assert!(matches!(
        held.try_recv(),
        Err(broadcast::error::TryRecvError::Closed)
    ));
}
