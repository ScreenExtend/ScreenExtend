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
use volume_proxy::{VolumeEvent, VolumeListeners};

const MONITOR_RING_CAPACITY: usize = 1 << 16;

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
        let our_device = probe::device_present().ok_or_else(|| {
            AudioCaptureError::Unsupported("ScreenExtend Audio device not found".into())
        })?;

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

        let (mon_prod, mon_cons) = ring::ring(MONITOR_RING_CAPACITY);
        let mon_cons = Arc::new(mon_cons);

        let gain = MonitorGain::new();

        let mut router = Router::new(
            HalDefaultDevicePort,
            branding::DEVICE_UID.to_string(),
            routing::default_state_path(),
        );
        router
            .activate(routing::now_secs())
            .map_err(AudioCaptureError::Setup)?;

        routing::install_watchdog_agent();

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

        volume_proxy::apply(our_device, target, &gain);

        let (routing_tx, routing_rx) = crossbeam_channel::bounded::<RoutingEvent>(16);
        let (volume_tx, volume_rx) = crossbeam_channel::bounded::<VolumeEvent>(16);
        let routing_listeners = RoutingListeners::register(routing_tx.clone());
        let volume_listeners = VolumeListeners::register(our_device, volume_tx);
        let format_listener = routing::FormatListener::register(target, routing_tx.clone());

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

        volume_keys::bind_device(our_device);
        let key_mechanism = if volume_keys::is_active() {
            "event_tap_fallback"
        } else {
            "device_volume_control"
        };

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
        self.control = None;
        volume_keys::unbind();
    }

    fn backend_name(&self) -> &'static str {
        "virtual_device"
    }

    fn reacquire(&mut self) -> Result<(), AudioCaptureError> {
        Ok(())
    }

    fn nonsilent_samples(&self) -> u64 {
        self.nonsilent.load(Ordering::Relaxed)
    }
}

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
    format_listener: Option<routing::FormatListener>,
    routing_tx: crossbeam_channel::Sender<RoutingEvent>,
    routing_rx: crossbeam_channel::Receiver<RoutingEvent>,
    volume_rx: crossbeam_channel::Receiver<VolumeEvent>,
    stop_rx: crossbeam_channel::Receiver<()>,
}

/// How often the control thread reports playthrough underruns.
///
/// The IOProc can only bump two atomics; saying anything about them is this thread's job.
const UNDERRUN_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

impl ControlRuntime {
    fn run(mut self) {
        let mut reported_underruns = 0u32;
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
                default(UNDERRUN_REPORT_INTERVAL) => {
                    self.report_underruns(&mut reported_underruns);
                }
            }
        }
        self.router.restore();
        // A clean stop means there is nothing left for the post-crash agent to recover, so it goes
        // with us rather than outliving the app as a permanent background job.
        routing::uninstall_watchdog_agent();
        crate::tprintln!("audio(legacy): stopped; default output restored");
    }

    /// Say when the monitor ring has run dry, and what the drift loop paid for it.
    ///
    /// The playthrough setpoint is the host's own monitoring latency, so it starts at three output
    /// IO periods and is only raised by a measured underrun. A `bump` that keeps climbing is this
    /// machine telling us the floor is too low for it.
    fn report_underruns(&self, reported: &mut u32) {
        let Some(p) = self.playthrough.as_ref() else {
            return;
        };
        let (underruns, bump) = p.underruns();
        if underruns == *reported {
            return;
        }
        crate::tprintln!(
            "audio(legacy): playthrough underruns {underruns} (+{}), setpoint raised by {bump} \
             frames ({:.1} ms)",
            underruns - *reported,
            bump as f64 / 48.0
        );
        *reported = underruns;
    }

    fn on_routing(&mut self, event: RoutingEvent) {
        match event {
            RoutingEvent::DefaultOutput => match self.router.on_default_changed() {
                DefaultChange::Ignore => {}
                DefaultChange::UserSwitchedAway { new_uid } => {
                    crate::tprintln!(
                        "audio(legacy): user switched default output to {new_uid}; releasing"
                    );
                    self.playthrough = None;
                    self.playthrough_target = 0;
                    self.format_listener = None;
                }
            },
            RoutingEvent::DeviceList => {
                probe::invalidate(); // the driver device may have just appeared or gone
                self.repoint_playthrough();
            }
            RoutingEvent::PlaythroughFormat => {
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

    fn set_playthrough(&mut self, device: AudioObjectID) {
        self.playthrough = None; // drop stops the old IOProc first (RAII)
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
