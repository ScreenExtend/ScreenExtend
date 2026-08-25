//! Legacy virtual-device system-audio backend for macOS 10.15–12.x
//! (PRD-macos-legacy-audio.md). This is the tier below the native ScreenCaptureKit (13.0+) and
//! Process Tap (14.2+) backends, for the OS range with no native system-audio API at all.
//!
//! It plugs into the existing [`AudioSource`](crate::macos_utils::audio::AudioSource) interface, so
//! the encoder / DataChannel / client pipeline above it is untouched and shared with every other
//! tier — zero new Opus or client code (PRD §8.2). [`LegacyVirtualDeviceSource::start`] wires up:
//!
//!   * **routing** ([`routing`]) — save the user's default output, make `ScreenExtend Audio` the
//!     default, restore it on stop / quit / crash;
//!   * **transport** ([`shm_reader`]) — read the driver-captured PCM (shared memory, or HAL-input
//!     fallback) into the encoder ring + the monitor ring;
//!   * **playthrough** ([`playthrough`]) — play the monitor ring to the real output device with a
//!     gain stage we control;
//!   * **volume proxy** ([`volume_proxy`]) — mirror our device's volume/mute to that gain and to
//!     the real device, so the volume keys keep working (§6.2 layer 2);
//!   * a dedicated **control thread** that owns those and reacts to default-device / device-list /
//!     volume notifications off the real-time path (§8.3).

pub mod branding;
pub mod hal;
pub mod installer;
pub mod playthrough;
pub mod probe;
pub mod routing;
pub mod shm_reader;
pub mod volume_keys;
pub mod volume_proxy;

#[cfg(test)]
mod test;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use objc2_core_audio::AudioObjectID;

use super::ring;
use super::{AudioCaptureError, AudioFrameSink, AudioSource};
use playthrough::{MonitorGain, Playthrough};
use routing::{DefaultChange, HalDefaultDevicePort, Router, RoutingEvent, RoutingListeners};
use shm_reader::{CaptureTargets, Reader, Transport};
use volume_keys::VolumeKeyTap;
use volume_proxy::{VolumeEvent, VolumeListeners};

/// Monitor ring capacity in f32 samples (power-of-two rounded by `ring`). ~0.68 s of 48 kHz stereo
/// — deep enough to decouple the driver's clock from the real device's without adding stream-path
/// latency (it only backs the local playthrough).
const MONITOR_RING_CAPACITY: usize = 1 << 16;

/// Handle to the running control thread; dropping it stops and joins (restoring routing).
struct ControlHandle {
    stop_tx: crossbeam_channel::Sender<()>,
    join: Option<JoinHandle<()>>,
    transport: Transport,
}

impl Drop for ControlHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub struct LegacyVirtualDeviceSource {
    control: Option<ControlHandle>,
    nonsilent: Arc<AtomicU64>,
}

impl LegacyVirtualDeviceSource {
    pub fn new() -> Self {
        Self {
            control: None,
            nonsilent: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Default for LegacyVirtualDeviceSource {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSource for LegacyVirtualDeviceSource {
    fn start(&mut self, sink: AudioFrameSink) -> Result<(), AudioCaptureError> {
        // Our device must be present and healthy (capable_backends already gated on this).
        let our_device = probe::device_present().ok_or_else(|| {
            AudioCaptureError::Unsupported("ScreenExtend Audio device not found".into())
        })?;

        // Drive the capture buffer period down to the minimum the device accepts (§5.3): target
        // 128 frames, clamped into the reported range. Lower buffer → lower capture latency.
        if let Some((lo, hi)) = hal::buffer_frame_size_range(our_device) {
            let target = 128u32.clamp(lo, hi);
            let st = hal::set_buffer_frame_size(our_device, target);
            let got = hal::buffer_frame_size(our_device).unwrap_or(target);
            crate::tprintln!(
                "audio(legacy): buffer frame size range [{lo},{hi}] → requested {target}, got {got} \
                 ({:.2} ms @ 48 kHz), st={st}",
                got as f64 / 48.0
            );
        }

        // Monitor ring: shm/HAL reader is the producer, the playthrough IOProc is the consumer.
        let (mon_prod, mon_cons) = ring::ring(MONITOR_RING_CAPACITY);
        let mon_cons = Arc::new(mon_cons);

        let gain = MonitorGain::new();

        // Take over the default output device (persist the old one first).
        let mut router = Router::new(
            HalDefaultDevicePort,
            branding::DEVICE_UID.to_string(),
            routing::default_state_path(),
        );
        router
            .activate(routing::now_secs())
            .map_err(AudioCaptureError::Setup)?;

        // Install the crash-recovery watchdog launch agent (idempotent, best-effort) so a crash is
        // undone even if the user never relaunches ScreenExtend (§8.3).
        routing::install_watchdog_agent();

        // Start the capture transport (shm, else HAL input).
        let targets = CaptureTargets {
            encoder: Arc::clone(&sink.producer),
            monitor: Arc::new(mon_prod),
            diagnostics: Arc::clone(&sink.diagnostics),
            control_tx: sink.control_tx.clone(),
            nonsilent: Arc::clone(&self.nonsilent),
        };
        let reader = shm_reader::start(our_device, targets);
        let transport = reader.transport();

        sink.diagnostics
            .sample_rate
            .store(48_000, Ordering::Relaxed);
        sink.diagnostics.channels.store(2, Ordering::Relaxed);

        // Pick the real output device for playthrough (the saved device if still present).
        let target =
            routing::preferred_playthrough_device(branding::DEVICE_UID, router.saved_uid())
                .unwrap_or(0);
        let playthrough = Playthrough::start(target, Arc::clone(&mon_cons), Arc::clone(&gain));
        if playthrough.is_none() {
            crate::teprintln!(
                "audio(legacy): no usable output device for playthrough — capture continues, but \
                 local monitoring is silent until a device appears"
            );
        }

        // Prime the gain from the device's current volume/mute, then watch for changes.
        volume_proxy::apply(our_device, target, &gain);

        let (routing_tx, routing_rx) = crossbeam_channel::bounded::<RoutingEvent>(16);
        let (volume_tx, volume_rx) = crossbeam_channel::bounded::<VolumeEvent>(16);
        let routing_listeners = RoutingListeners::register(routing_tx.clone());
        let volume_listeners = VolumeListeners::register(our_device, volume_tx);
        // Watch the current playthrough device for an in-place sample-rate change (§8.3).
        let format_listener = routing::FormatListener::register(target, routing_tx.clone());

        // Volume keys (§6.2/§6.3). Layer 1 — the device's own Volume/Mute controls — is what makes
        // macOS handle the F10/F11/F12 keys natively, *including the on-screen HUD*; verified working
        // on 10.15. So Layer 1 is the default and we do nothing here. The §6.3 event-tap backstop
        // drives our device's volume/mute directly and **consumes** the keys — it guarantees the keys
        // work even where Layer 1 fails to re-enable OS handling, but consuming the event suppresses
        // the native HUD. It is therefore opt-in (set SCREENEXTEND_LEGACY_VOLUME_TAP=1) for any OS
        // where Layer 1 turns out not to work, rather than always-on. Absent Accessibility permission
        // the tap is `None` regardless, and we fall back to Layer 1.
        let volume_keys = if std::env::var_os("SCREENEXTEND_LEGACY_VOLUME_TAP").is_some() {
            VolumeKeyTap::start(our_device)
        } else {
            None
        };
        let key_mechanism = if volume_keys.is_some() {
            "event_tap_fallback"
        } else {
            "device_volume_control"
        };

        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);

        let runtime = ControlRuntime {
            router,
            reader,
            playthrough,
            playthrough_target: target,
            our_device,
            gain,
            mon_cons,
            _routing_listeners: routing_listeners,
            _volume_listeners: volume_listeners,
            _volume_keys: volume_keys,
            format_listener,
            routing_tx,
            routing_rx,
            volume_rx,
            stop_rx,
        };

        let join = std::thread::Builder::new()
            .name("se-audio-legacy-ctl".into())
            .spawn(move || runtime.run())
            .map_err(|e| AudioCaptureError::Setup(format!("control thread spawn failed: {e}")))?;

        crate::tprintln!(
            "audio(legacy): virtual device active (transport={}, playthrough_dev={target}, \
             volume_keys={key_mechanism})",
            transport.as_str()
        );

        self.control = Some(ControlHandle {
            stop_tx,
            join: Some(join),
            transport,
        });
        Ok(())
    }

    fn stop(&mut self) {
        // Dropping the control handle signals stop, joins the thread, and (inside the thread)
        // restores the user's default output before the resources are dropped.
        self.control = None;
    }

    fn backend_name(&self) -> &'static str {
        "virtual_device"
    }

    fn reacquire(&mut self) -> Result<(), AudioCaptureError> {
        // Device re-acquisition on driver restart / default-device change is handled by the control
        // thread's HAL listeners; the worker-level reacquire is only a discontinuity hint here.
        Ok(())
    }

    fn nonsilent_samples(&self) -> u64 {
        self.nonsilent.load(Ordering::Relaxed)
    }
}

/// Everything the control thread owns for the session. Runs off the real-time path; reacts to
/// device + volume notifications and tears everything down (restoring routing) on stop.
struct ControlRuntime {
    router: Router<HalDefaultDevicePort>,
    reader: Reader,
    playthrough: Option<Playthrough>,
    playthrough_target: AudioObjectID,
    our_device: AudioObjectID,
    gain: Arc<MonitorGain>,
    mon_cons: Arc<ring::Consumer>,
    _routing_listeners: RoutingListeners,
    _volume_listeners: VolumeListeners,
    /// Kept alive for the session; its Drop stops the run loop + joins the tap thread.
    _volume_keys: Option<VolumeKeyTap>,
    /// Per-device sample-rate listener on the current playthrough device (re-registered on re-point).
    format_listener: Option<routing::FormatListener>,
    /// Cloned to re-register the format listener when the playthrough device changes.
    routing_tx: crossbeam_channel::Sender<RoutingEvent>,
    routing_rx: crossbeam_channel::Receiver<RoutingEvent>,
    volume_rx: crossbeam_channel::Receiver<VolumeEvent>,
    stop_rx: crossbeam_channel::Receiver<()>,
}

impl ControlRuntime {
    fn run(mut self) {
        loop {
            crossbeam_channel::select! {
                recv(self.stop_rx) -> _ => break,
                recv(self.routing_rx) -> msg => {
                    if let Ok(event) = msg {
                        self.on_routing(event);
                    }
                }
                recv(self.volume_rx) -> msg => {
                    if msg.is_ok() {
                        volume_proxy::apply(self.our_device, self.playthrough_target, &self.gain);
                    }
                }
            }
        }
        // Restore the user's default output before dropping the reader/playthrough (RAII).
        self.router.restore();
        crate::tprintln!("audio(legacy): stopped; default output restored");
    }

    fn on_routing(&mut self, event: RoutingEvent) {
        match event {
            RoutingEvent::DefaultOutput => match self.router.on_default_changed() {
                DefaultChange::Ignore => {}
                DefaultChange::UserSwitchedAway { new_uid } => {
                    // Don't fight the user: stop monitoring and let capture go silent (we're no
                    // longer the default output). We do NOT re-assert ourselves (PRD §8.3).
                    crate::tprintln!(
                        "audio(legacy): user switched default output to {new_uid}; releasing"
                    );
                    self.playthrough = None;
                    self.playthrough_target = 0;
                    self.format_listener = None;
                }
            },
            RoutingEvent::DeviceList => self.repoint_playthrough(),
            RoutingEvent::PlaythroughFormat => {
                // Same device, new sample rate (AirPods call mode): restart the IOProc so it re-reads
                // the rate and re-computes the resample ratio, rather than glitching (§8.3).
                if self.router.is_active() && self.playthrough_target != 0 {
                    crate::tprintln!(
                        "audio(legacy): playthrough device {} changed format; re-syncing",
                        self.playthrough_target
                    );
                    self.set_playthrough(self.playthrough_target);
                }
            }
        }
    }

    /// A device appeared/disappeared (headphones, Bluetooth/AirPods): re-point playthrough at the
    /// newly-preferred device without dropping the stream (PRD §8.3).
    fn repoint_playthrough(&mut self) {
        if !self.router.is_active() {
            return;
        }
        let preferred =
            routing::preferred_playthrough_device(branding::DEVICE_UID, self.router.saved_uid())
                .unwrap_or(0);
        if preferred == self.playthrough_target && self.playthrough.is_some() {
            return; // no change
        }
        self.set_playthrough(preferred);
    }

    /// (Re)start playthrough on `device`, re-registering the format listener for it. `device == 0`
    /// tears playthrough down. Restarting re-reads the device's sample rate.
    fn set_playthrough(&mut self, device: AudioObjectID) {
        self.playthrough = None; // stop the old IOProc first (RAII)
        self.format_listener = None; // and its format listener
        self.playthrough_target = device;
        if device != 0 {
            self.playthrough =
                Playthrough::start(device, Arc::clone(&self.mon_cons), Arc::clone(&self.gain));
            self.format_listener =
                routing::FormatListener::register(device, self.routing_tx.clone());
            volume_proxy::apply(self.our_device, device, &self.gain);
            crate::tprintln!("audio(legacy): playthrough → device {device}");
        }
    }
}
