//! The plan: workloads, recipes, groups, ordering. All data; editing the JSON needs no code change.

use serde::{Deserialize, Serialize};

#[cfg(windows)]
use super::win::recipe::{Codec, Recipe, Val};
#[cfg(target_os = "macos")]
use crate::macos_utils::streamer::vt_recipe::{Codec, Recipe, Val};

pub const QUICK: &str = include_str!("plans/quick.json");
pub const DEFAULT: &str = include_str!("plans/default.json");
pub const ANCHORS: &str = include_str!("plans/anchors.json");
pub const FULL: &str = include_str!("plans/full.json");
/// Windows: every vendor at 1080p60, production recipes (`lab`).
#[cfg(windows)]
pub const WIN_QUICK: &str = include_str!("plans/win-quick.json");
/// Windows: the NVENC and Quick Sync settings sweep (`lab`).
#[cfg(windows)]
pub const WIN_ENCODERS: &str = include_str!("plans/win-encoders.json");
/// Windows: NVENC and Quick Sync through the production encode loop (`pipe`).
#[cfg(windows)]
pub const WIN_PIPE_QUICK: &str = include_str!("plans/win-pipe-quick.json");
/// Windows: several `pipe` sessions in one process.
#[cfg(windows)]
pub const WIN_PIPE_SESSIONS: &str = include_str!("plans/win-pipe-sessions.json");
/// Windows: the capture handler's gates driven by a take's recorded flags (`pipe`).
#[cfg(windows)]
pub const WIN_PIPE_GATES: &str = include_str!("plans/win-pipe-gates.json");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub name: String,
    /// Candidate properties for the probe stage. Absent: the default plan's catalogue.
    #[serde(default)]
    pub catalogue: Option<Catalogue>,
    /// Probe rows. Absent: the built-in matrix (codec x low-latency x hardware, plus one 4K row).
    #[serde(default)]
    pub probe_rows: Option<Vec<ProbeRow>>,
    #[serde(default)]
    pub workloads: Vec<Workload>,
    #[serde(default)]
    pub recipes: Vec<RecipeEntry>,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub order: Order,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Catalogue {
    /// `[key, [value, value, ...]]`: each (key, value) is set on a fresh session.
    #[serde(default)]
    pub trials: Vec<(String, Vec<Val>)>,
    /// Read-only keys copied from a fresh session.
    #[serde(default)]
    pub reads: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeRow {
    #[serde(default)]
    pub codec: Codec,
    pub low_latency: bool,
    /// `require` | `enable` | `software`
    pub hw: String,
    #[serde(default = "d_w")]
    pub width: u32,
    #[serde(default = "d_h")]
    pub height: u32,
}

impl ProbeRow {
    pub fn id(&self) -> String {
        format!(
            "{}.{}.{}.{}x{}",
            match self.codec {
                Codec::H264 => "h264",
                Codec::Hevc => "hevc",
            },
            if self.low_latency { "ll" } else { "std" },
            self.hw,
            self.width,
            self.height
        )
    }

    #[cfg(target_os = "macos")]
    pub fn default_rows() -> Vec<ProbeRow> {
        let mut rows = Vec::new();
        for codec in [Codec::H264, Codec::Hevc] {
            for low_latency in [true, false] {
                for hw in ["require", "enable", "software"] {
                    rows.push(ProbeRow {
                        codec,
                        low_latency,
                        hw: hw.into(),
                        width: 1920,
                        height: 1080,
                    });
                }
            }
        }
        rows.push(ProbeRow {
            codec: Codec::H264,
            low_latency: true,
            hw: "require".into(),
            width: 3840,
            height: 2160,
        });
        rows
    }

    /// Windows: one row per encoder vendor (`hw` names the vendor), H.264 only.
    #[cfg(windows)]
    pub fn default_rows() -> Vec<ProbeRow> {
        super::win::probe::default_rows()
    }
}

fn d_w() -> u32 {
    1920
}
fn d_h() -> u32 {
    1080
}
fn d_wu() -> usize {
    1920
}
fn d_hu() -> usize {
    1080
}
fn d_fps() -> u32 {
    60
}
fn d_secs() -> f64 {
    10.0
}
fn d_one_f() -> f64 {
    1.0
}
fn d_one() -> u32 {
    1
}
fn d_seed() -> u64 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workload {
    pub id: String,
    /// `recording` | `synthetic`
    pub kind: String,
    /// Synthetic only: `window_drag_stress` (default) | `static` | `scroll` | `motion` | `drag` |
    /// `mixed`.
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default = "d_wu")]
    pub width: usize,
    #[serde(default = "d_hu")]
    pub height: usize,
    #[serde(default = "d_fps")]
    pub fps: u32,
    #[serde(default = "d_secs")]
    pub seconds: f64,
    #[serde(default = "d_seed")]
    pub seed: u64,
    /// Pacing multiplier: 2.0 replays the same frames at twice the rate.
    #[serde(default = "d_one_f")]
    pub speed: f64,
    #[serde(default)]
    pub max_frames: Option<usize>,
    /// `tile2x2` | `crop:WxH`
    #[serde(default)]
    pub transform: Option<String>,
    /// `pipe` stage: withhold a frame whose pixels equal the previous one's, the way a capture
    /// backend delivers nothing for an unchanged screen (so the encode loop sees only its own
    /// keepalive). Off by default.
    #[serde(default)]
    pub skip_unchanged: bool,
}

impl Workload {
    /// The key a rendered clip is cached under: only what changes the pictures.
    pub fn clip_key(&self) -> String {
        let s = format!(
            "{}|{}|{}|{}|{}|{}",
            self.pattern.as_deref().unwrap_or("window_drag_stress"),
            self.width,
            self.height,
            self.fps,
            self.seconds,
            self.seed
        );
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        format!("{h:016x}")
    }
}

/// The encoder configuration for production replay (recipe `None`).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ProductionCfg {
    #[serde(default)]
    pub qp: Option<u8>,
    #[serde(default)]
    pub min_qp: Option<u8>,
    #[serde(default)]
    pub bitrate_bps: Option<u32>,
    /// `high` (default) | `main` | `baseline`
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub software: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecipeEntry {
    #[serde(flatten)]
    pub recipe: Recipe,
    /// Present: run the production `Encoder` path with this config and no recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub production: Option<ProductionCfg>,
    /// Hidden: `crash` | `abort` | `hang` | `panic`, to prove the runner survives a bad child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_fault: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub at_s: Option<f64>,
    #[serde(default)]
    pub at_frame: Option<u64>,
    /// Repeat every N seconds after the first firing.
    #[serde(default)]
    pub every_s: Option<f64>,
    /// `force_idr` | `set` (a session property) | `set_frame_opt` (a per-frame option from this
    /// frame on; `value: null` removes it). The `pipe` stage takes `force_idr` only.
    pub action: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub value: Option<Val>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Group {
    #[serde(default)]
    pub name: Option<String>,
    /// Recipe names, `*` globs allowed.
    pub recipes: Vec<String>,
    pub workloads: Vec<String>,
    #[serde(default = "d_one")]
    pub repeats: u32,
    #[serde(default = "d_one")]
    pub loops: u32,
    #[serde(default)]
    pub psnr: bool,
    #[serde(default)]
    pub per_frame: bool,
    #[serde(default)]
    pub events: Vec<Event>,
    /// `lab` (default): the bench's own submit-everything engine. `pipe`: the production encode
    /// loop (`pipeline::spawn_pipeline`) and RTP packetizer.
    #[serde(default = "d_stage")]
    pub stage: String,
    /// Encoders fed the same frames concurrently in one child, one per modelled client.
    #[serde(default = "d_one")]
    pub sessions: u32,
    /// `SE_VT_DENY` items for this group's children (see `vt_recipe::Deny`).
    #[serde(default)]
    pub deny: Vec<String>,
    /// `SE_TUNE_ON` / `SE_TUNE_OFF` items for this group's children (see `tune.rs`).
    #[serde(default)]
    pub tune_on: Vec<String>,
    #[serde(default)]
    pub tune_off: Vec<String>,
    /// Present: each run is a create/encode/destroy churn instead of a replay (no clip needed;
    /// the workload gives the size and frame rate).
    #[serde(default)]
    pub churn: Option<Churn>,
    /// Long-term reference harness (lab replay runs only).
    #[serde(default)]
    pub ltr: Option<Ltr>,
    /// `live` stage options (display, run length, capture switches).
    #[serde(default)]
    pub live: Option<LiveOpts>,
}

/// The `live` stage: a real virtual display, real capture, the real WHEP join and transport, and
/// an in-process client that decodes. Every field is optional; absent = the default noted.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct LiveOpts {
    /// Display the client asks for, in CSS pixels (default 1920 x 1080).
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// Refresh rate the client reports and the device override carries (default 60).
    #[serde(default)]
    pub refresh: Option<u32>,
    /// Device pixel ratio the client reports (default 1.0).
    #[serde(default)]
    pub dpr: Option<f64>,
    /// Encoder scale percent, the per-device `videoScale` (default 100).
    #[serde(default)]
    pub video_scale: Option<u32>,
    /// Measured span after the warm-up (default 12 s). The workload's own `seconds` is not used.
    #[serde(default)]
    pub seconds: Option<f64>,
    /// Workload frames before measuring starts (default 1.5 s).
    #[serde(default)]
    pub warmup_s: Option<f64>,
    /// Draw the frame counter and the barcode stamp the receiver reads (default true).
    #[serde(default)]
    pub stamp: Option<bool>,
    /// Test hook: behave as if Screen Recording were denied (default false).
    #[serde(default)]
    pub simulate_screen_recording_denied: bool,
    /// Skip the window and move the cursor over the display instead (default false).
    #[serde(default)]
    pub force_cursor_fallback: bool,
    /// Offer only Constrained Baseline (default false: High and Constrained Baseline, as Chrome).
    #[serde(default)]
    pub baseline_only: bool,
    /// Listener ports (default 28780 / 28743).
    #[serde(default)]
    pub http_port: Option<u16>,
    #[serde(default)]
    pub https_port: Option<u16>,
    /// Capture A/B switches, passed to the child as `SE_SCK_*` / `SE_CGDS_*`.
    #[serde(default)]
    pub capture: CaptureKnobs,
    /// The client sends the page's `OP::STATS` frame once a second on a `reliable` DataChannel
    /// (default true), so the host's client-side hold-push guard has what a real page gives it.
    /// `false` exercises the no-STATS rule (no immediate hold-push mode).
    #[serde(default)]
    pub client_stats: Option<bool>,
    /// After the join, move the display to the left of the main display permanently, so the
    /// display identity is remembered there (the stale-origin regression's precondition).
    #[serde(default)]
    pub teach_origin_left: bool,
    /// Display churn instead of a measurement: `joins` short real joins back to back on one host
    /// process, cycling `sizes`, each held `hold_s` and `gap_ms` apart (same device identity).
    #[serde(default)]
    pub churn: Option<ChurnOpts>,
    /// The client's UDP receive buffer in bytes (default 8 MiB; 0 = the OS default).
    #[serde(default)]
    pub rcvbuf_bytes: Option<u32>,
    /// The client's SRTP replay window in packets (default 8192; 0 = webrtc-rs's 64).
    #[serde(default)]
    pub srtp_window: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct ChurnOpts {
    pub joins: u32,
    /// `[[w, h], ...]`; default the single size of `width` / `height`.
    #[serde(default)]
    pub sizes: Vec<(u32, u32)>,
    #[serde(default)]
    pub hold_s: Option<f64>,
    #[serde(default)]
    pub gap_ms: Option<u64>,
    /// Wait this long before the first join: a host that has been up a while.
    #[serde(default)]
    pub first_wait_ms: Option<u64>,
    /// Control: never run the main run loop.
    #[serde(default)]
    pub block_main: bool,
    /// What the main thread does while the joins run on another thread: `block` (as `serve`; the
    /// default), `pump` (a CFRunLoop, as `serve` with `SE_MAIN_PUMP=1`), or `nsapp` (AppKit's
    /// event loop, as the desktop app).
    #[serde(default)]
    pub main: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct CaptureKnobs {
    /// `SE_SCK_QUEUE_DEPTH` (shipped: 8).
    #[serde(default)]
    pub sck_queue_depth: Option<i64>,
    /// `SE_SCK_MIN_INTERVAL`, a multiple of the frame period (shipped: 0.9; 0 = unthrottled).
    #[serde(default)]
    pub sck_min_interval: Option<f64>,
    /// `SE_SCK_PIXEL_FORMAT`: `420f` (shipped) | `420v`.
    #[serde(default)]
    pub sck_pixel_format: Option<String>,
    /// `SE_SCK_SHOWS_CURSOR` (shipped: true).
    #[serde(default)]
    pub sck_shows_cursor: Option<bool>,
    /// `SE_CGDS_QUEUE_DEPTH` (shipped: 5).
    #[serde(default)]
    pub cgds_queue_depth: Option<i64>,
    /// `SE_CGDS_MIN_FRAME_TIME` in seconds (shipped: not set).
    #[serde(default)]
    pub cgds_min_frame_time: Option<f64>,
}

impl CaptureKnobs {
    /// The child environment these switches become.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        if let Some(n) = self.sck_queue_depth {
            v.push(("SE_SCK_QUEUE_DEPTH", n.to_string()));
        }
        if let Some(f) = self.sck_min_interval {
            v.push(("SE_SCK_MIN_INTERVAL", f.to_string()));
        }
        if let Some(p) = &self.sck_pixel_format {
            v.push(("SE_SCK_PIXEL_FORMAT", p.clone()));
        }
        if let Some(b) = self.sck_shows_cursor {
            v.push(("SE_SCK_SHOWS_CURSOR", if b { "1" } else { "0" }.to_string()));
        }
        if let Some(n) = self.cgds_queue_depth {
            v.push(("SE_CGDS_QUEUE_DEPTH", n.to_string()));
        }
        if let Some(f) = self.cgds_min_frame_time {
            v.push(("SE_CGDS_MIN_FRAME_TIME", f.to_string()));
        }
        v
    }
}

impl LiveOpts {
    pub fn seconds(&self) -> f64 {
        self.seconds.unwrap_or(12.0).max(1.0)
    }
    pub fn warmup_s(&self) -> f64 {
        self.warmup_s.unwrap_or(1.5).max(0.5)
    }
}

/// Long-term reference frames in the lab: `EnableLTR` on every session, each emitted token read
/// from `kVTSampleAttachmentKey_RequireLTRAcknowledgementToken` and handed back through the
/// per-frame `AcknowledgedLTRTokens` option according to `ack`. The event `force_ltr_refresh`
/// sets `ForceLTRRefresh` on one frame.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Ltr {
    pub ack: LtrAck,
}

/// `"all"` (acknowledge each token on the next submit after it arrives), `"none"`, or
/// `{"delay_frames": N}` (a token from frame g is acknowledged from frame g + N on).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum LtrAck {
    Mode(String),
    Delay { delay_frames: u64 },
}

impl LtrAck {
    /// Frames a token waits before it is acknowledged; `None` = never.
    pub fn delay(&self) -> Result<Option<u64>, String> {
        match self {
            LtrAck::Mode(m) if m == "all" => Ok(Some(0)),
            LtrAck::Mode(m) if m == "none" => Ok(None),
            LtrAck::Mode(m) => Err(format!("ltr.ack '{m}' (all | none | {{delay_frames: N}})")),
            LtrAck::Delay { delay_frames } => Ok(Some(*delay_frames)),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Churn {
    /// Create, encode, destroy cycles.
    #[serde(default = "d_cycles")]
    pub cycles: u32,
    /// Frames encoded per cycle, at the workload's frame rate.
    #[serde(default = "d_churn_frames")]
    pub frames: u32,
}

fn d_cycles() -> u32 {
    50
}
fn d_churn_frames() -> u32 {
    30
}
fn d_stage() -> String {
    "lab".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Order {
    #[serde(default = "d_seed")]
    pub shuffle_seed: u64,
    #[serde(default = "d_true")]
    pub interleave_repeats: bool,
    #[serde(default = "d_cool")]
    pub cooldown_ms: u64,
    /// Watchdog = expected duration + this.
    #[serde(default = "d_slack")]
    pub watchdog_slack_s: u64,
}

fn d_true() -> bool {
    true
}
fn d_cool() -> u64 {
    2000
}
fn d_slack() -> u64 {
    60
}

impl Default for Order {
    fn default() -> Self {
        Order {
            shuffle_seed: 1,
            interleave_repeats: true,
            cooldown_ms: d_cool(),
            watchdog_slack_s: d_slack(),
        }
    }
}

/// One expanded run, exactly what a child needs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunSpec {
    pub id: String,
    pub group: usize,
    #[serde(default)]
    pub group_name: Option<String>,
    pub repeat: u32,
    pub recipe: RecipeEntry,
    pub workload: Workload,
    pub loops: u32,
    pub psnr: bool,
    pub per_frame: bool,
    pub events: Vec<Event>,
    #[serde(default = "d_stage")]
    pub stage: String,
    #[serde(default = "d_one")]
    pub sessions: u32,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub tune_on: Vec<String>,
    #[serde(default)]
    pub tune_off: Vec<String>,
    #[serde(default)]
    pub churn: Option<Churn>,
    #[serde(default)]
    pub ltr: Option<Ltr>,
    #[serde(default)]
    pub live: Option<LiveOpts>,
    /// Filled by the runner just before the child starts.
    #[serde(default)]
    pub clip_path: Option<String>,
    #[serde(default)]
    pub result_path: Option<String>,
    #[serde(default)]
    pub per_frame_path: Option<String>,
}

pub fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() {
            continue;
        }
        if i == 0 {
            if !rest.starts_with(p) {
                return false;
            }
            rest = &rest[p.len()..];
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            match rest.find(p) {
                Some(at) => rest = &rest[at + p.len()..],
                None => return false,
            }
        }
    }
    true
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Plan {
    /// Parse a plan. A recipe may name a `base` recipe: it starts from the base's fields, its own
    /// `spec` / `props` / `frame_opts` entries replace the base's entry with the same key in place
    /// (or are appended), `remove` lists keys to drop from any of the three, and every other
    /// field it states overrides the base.
    pub fn parse(text: &str) -> Result<Plan, String> {
        let mut raw: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("plan: {e}"))?;
        if let Some(list) = raw.get("recipes").and_then(|r| r.as_array()).cloned() {
            let mut resolved: Vec<serde_json::Value> = Vec::with_capacity(list.len());
            for r in &list {
                resolved.push(resolve_base(r, &list, 0)?);
            }
            raw["recipes"] = serde_json::Value::Array(resolved);
        }
        serde_json::from_value(raw).map_err(|e| format!("plan: {e}"))
    }

    pub fn builtin(name: &str) -> Option<&'static str> {
        match name {
            "quick" => Some(QUICK),
            "default" => Some(DEFAULT),
            "anchors" => Some(ANCHORS),
            "full" => Some(FULL),
            #[cfg(windows)]
            "win-quick" => Some(WIN_QUICK),
            #[cfg(windows)]
            "win-encoders" => Some(WIN_ENCODERS),
            #[cfg(windows)]
            "win-pipe-quick" => Some(WIN_PIPE_QUICK),
            #[cfg(windows)]
            "win-pipe-sessions" => Some(WIN_PIPE_SESSIONS),
            #[cfg(windows)]
            "win-pipe-gates" => Some(WIN_PIPE_GATES),
            _ => None,
        }
    }

    /// Every run, in execution order: workload-major (a rendered clip is needed by one block of
    /// runs and can be deleted after it), and within a workload the repeats are interleaved
    /// across configurations with a seeded shuffle per round, so drift lands on every config.
    pub fn expand(&self) -> Result<Vec<RunSpec>, String> {
        let mut runs = Vec::new();
        for (gi, g) in self.groups.iter().enumerate() {
            if !["lab", "pipe", "live"].contains(&g.stage.as_str()) {
                return Err(format!(
                    "group {gi}: unknown stage '{}' (lab | pipe | live)",
                    g.stage
                ));
            }
            if g.live.is_some() && g.stage != "live" {
                return Err(format!("group {gi}: live options belong to the live stage"));
            }
            if g.stage == "live" {
                if g.churn.is_some() || g.ltr.is_some() || g.sessions > 1 || g.psnr {
                    return Err(format!(
                        "group {gi}: a live run is one session; churn, ltr, sessions and psnr \
                         belong to the lab"
                    ));
                }
                if !g.events.is_empty() {
                    return Err(format!("group {gi}: the live stage takes no events"));
                }
                if let Some(p) = g
                    .live
                    .as_ref()
                    .and_then(|l| l.capture.sck_pixel_format.as_deref())
                {
                    if !["420f", "420v"].contains(&p) {
                        return Err(format!("group {gi}: sck_pixel_format '{p}' (420f | 420v)"));
                    }
                }
            }
            if g.churn.is_some() && g.stage == "pipe" {
                return Err(format!("group {gi}: churn runs belong to the lab stage"));
            }
            if let Some(l) = &g.ltr {
                if g.stage != "lab" || g.churn.is_some() {
                    return Err(format!("group {gi}: ltr belongs to lab replay runs"));
                }
                l.ack.delay().map_err(|e| format!("group {gi}: {e}"))?;
            }
            if !g.deny.is_empty() {
                #[cfg(target_os = "macos")]
                crate::macos_utils::streamer::vt_recipe::Deny::parse(&g.deny.join(","))
                    .map_err(|e| format!("group {gi}: deny: {e}"))?;
                #[cfg(windows)]
                super::win::validate_deny(&g.deny).map_err(|e| format!("group {gi}: deny: {e}"))?;
            }
            let mut recipes: Vec<&RecipeEntry> = Vec::new();
            for pat in &g.recipes {
                let hits: Vec<&RecipeEntry> = self
                    .recipes
                    .iter()
                    .filter(|r| glob(pat, &r.recipe.name))
                    .collect();
                if hits.is_empty() {
                    return Err(format!("group {gi}: no recipe matches '{pat}'"));
                }
                for h in hits {
                    if !recipes.iter().any(|r| r.recipe.name == h.recipe.name) {
                        recipes.push(h);
                    }
                }
            }
            for wid in &g.workloads {
                let w = self
                    .workloads
                    .iter()
                    .find(|w| &w.id == wid)
                    .ok_or_else(|| format!("group {gi}: unknown workload '{wid}'"))?;
                for r in &recipes {
                    for k in 0..g.repeats.max(1) {
                        runs.push(RunSpec {
                            id: format!(
                                "g{gi}.{}.{}.r{k}",
                                sanitize(&r.recipe.name),
                                sanitize(&w.id)
                            ),
                            group: gi,
                            group_name: g.name.clone(),
                            repeat: k,
                            recipe: (*r).clone(),
                            workload: w.clone(),
                            loops: g.loops.max(1),
                            psnr: g.psnr,
                            per_frame: g.per_frame,
                            events: g.events.clone(),
                            stage: g.stage.clone(),
                            sessions: g.sessions.max(1),
                            deny: g.deny.clone(),
                            tune_on: g.tune_on.clone(),
                            tune_off: g.tune_off.clone(),
                            churn: g.churn.clone(),
                            ltr: g.ltr.clone(),
                            live: if g.stage == "live" {
                                Some(g.live.clone().unwrap_or_default())
                            } else {
                                None
                            },
                            clip_path: None,
                            result_path: None,
                            per_frame_path: None,
                        });
                    }
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        for r in &runs {
            if !seen.insert(r.id.clone()) {
                return Err(format!("duplicate run id {}", r.id));
            }
        }

        // workload-major blocks in order of first appearance; workloads that share one rendered
        // clip (same pictures, different transform or speed) stay next to each other
        let mut order: Vec<String> = Vec::new();
        for r in &runs {
            if !order.contains(&r.workload.id) {
                order.push(r.workload.id.clone());
            }
        }
        let clip_group = |id: &String| -> String {
            let w = runs
                .iter()
                .find(|r| &r.workload.id == id)
                .map(|r| &r.workload);
            match w {
                Some(w) if w.kind == "synthetic" => format!("syn:{}", w.clip_key()),
                _ => format!("rec:{id}"),
            }
        };
        let groups: Vec<String> = order.iter().map(clip_group).collect();
        let mut first_of_group: Vec<String> = Vec::new();
        for g in &groups {
            if !first_of_group.contains(g) {
                first_of_group.push(g.clone());
            }
        }
        let mut keyed: Vec<(usize, usize, String)> = order
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let gi = first_of_group
                    .iter()
                    .position(|g| *g == groups[i])
                    .unwrap_or(0);
                (gi, i, id.clone())
            })
            .collect();
        keyed.sort();
        let order: Vec<String> = keyed.into_iter().map(|(_, _, id)| id).collect();
        let mut out = Vec::with_capacity(runs.len());
        for (bi, wid) in order.iter().enumerate() {
            let block: Vec<RunSpec> = runs
                .iter()
                .filter(|r| &r.workload.id == wid)
                .cloned()
                .collect();
            let seed = self.order.shuffle_seed ^ ((bi as u64 + 1) << 32);
            if self.order.interleave_repeats {
                let rounds = block.iter().map(|r| r.repeat).max().unwrap_or(0);
                for round in 0..=rounds {
                    let mut this: Vec<RunSpec> = block
                        .iter()
                        .filter(|r| r.repeat == round)
                        .cloned()
                        .collect();
                    shuffle(&mut this, seed ^ (round as u64).wrapping_mul(0x9E37_79B9));
                    out.extend(this);
                }
            } else {
                let mut b = block;
                shuffle(&mut b, seed);
                out.extend(b);
            }
        }
        Ok(out)
    }
}

fn resolve_base(
    r: &serde_json::Value,
    all: &[serde_json::Value],
    depth: usize,
) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    let name = r["name"].as_str().unwrap_or("?");
    let Some(base_name) = r.get("base").and_then(Value::as_str) else {
        let mut out = r.clone();
        if let Some(o) = out.as_object_mut() {
            o.remove("remove");
        }
        return Ok(out);
    };
    if depth > 16 {
        return Err(format!("recipe '{name}': base chain too deep (a cycle?)"));
    }
    let base = all
        .iter()
        .find(|b| b["name"].as_str() == Some(base_name))
        .ok_or_else(|| format!("recipe '{name}': unknown base '{base_name}'"))?;
    let mut out = resolve_base(base, all, depth + 1)?;
    let removed: Vec<String> = r
        .get("remove")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let obj = out.as_object_mut().ok_or("recipe is not an object")?;
    for list in ["spec", "props", "frame_opts"] {
        let mut entries: Vec<Value> = obj
            .get(list)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        entries.retain(|e| !removed.iter().any(|k| e[0].as_str() == Some(k)));
        if let Some(own) = r.get(list).and_then(Value::as_array) {
            for e in own {
                match entries.iter_mut().find(|x| x[0] == e[0]) {
                    Some(slot) => *slot = e.clone(),
                    None => entries.push(e.clone()),
                }
            }
        }
        obj.insert(list.into(), Value::Array(entries));
    }
    for (k, v) in r.as_object().ok_or("recipe is not an object")? {
        if !["spec", "props", "frame_opts", "base", "remove"].contains(&k.as_str()) {
            obj.insert(k.clone(), v.clone());
        }
    }
    obj.remove("base");
    obj.remove("remove");
    // a production entry carries no recipe body of its own
    if r.get("production").is_none() && base.get("production").is_some() {
        obj.remove("production");
    }
    Ok(out)
}

fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    for i in (1..v.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let j = (s % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_plans_parse_and_expand() {
        for (name, text) in [
            ("quick", QUICK),
            ("default", DEFAULT),
            ("anchors", ANCHORS),
            ("full", FULL),
        ] {
            let p = Plan::parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            let runs = p.expand().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!runs.is_empty(), "{name} has no runs");
        }
        let d = Plan::parse(DEFAULT).unwrap();
        assert!(
            d.catalogue.is_some(),
            "the default plan carries the probe catalogue"
        );
    }

    #[test]
    fn repeats_are_interleaved() {
        let p = Plan::parse(QUICK).unwrap();
        let runs = p.expand().unwrap();
        // within a workload block, every repeat-0 run precedes every repeat-1 run
        let first = &runs[0].workload.id;
        let block: Vec<&RunSpec> = runs.iter().filter(|r| &r.workload.id == first).collect();
        let last_r0 = block.iter().rposition(|r| r.repeat == 0).unwrap();
        let first_r1 = block.iter().position(|r| r.repeat == 1);
        if let Some(f) = first_r1 {
            assert!(last_r0 < f);
        }
    }

    #[test]
    fn a_recipe_inherits_and_overrides_its_base() {
        let p = Plan::parse(
            r#"{"recipes":[
                {"name":"a","spec":[["S",true]],"props":[["X",1],["Y",2]]},
                {"name":"b","base":"a","props":[["Y",3],["Z",4]],"remove":["X"]},
                {"name":"c","base":"b","codec":"hevc"}],
              "workloads":[],"groups":[]}"#,
        )
        .unwrap();
        let b = &p.recipes[1].recipe;
        assert_eq!(b.spec.len(), 1);
        let keys: Vec<&str> = b.props.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["Y", "Z"]);
        assert_eq!(b.props[0].1, Val::Int(3));
        assert_eq!(p.recipes[2].recipe.codec, Codec::Hevc);
        assert_eq!(p.recipes[2].recipe.name, "c");
    }

    #[test]
    fn m2_group_fields_reach_the_runs_and_bad_ones_are_refused() {
        let base = r#"{"recipes":[{"name":"p","production":{"qp":15}}],
            "workloads":[{"id":"w","kind":"synthetic","seconds":1}],"groups":[GROUP]}"#;
        let p = Plan::parse(&base.replace(
            "GROUP",
            r#"{"recipes":["p"],"workloads":["w"],"stage":"pipe","sessions":2,
                "deny":["create:lowlatency"],"tune_on":["drain_time_constraint"],
                "tune_off":["keep_awake"],
                "events":[{"at_s":0.5,"action":"set_frame_opt","key":"BaseFrameQP","value":30}]}"#,
        ))
        .unwrap();
        let r = &p.expand().unwrap()[0];
        assert_eq!(r.stage, "pipe");
        assert_eq!(r.sessions, 2);
        assert_eq!(r.deny, ["create:lowlatency"]);
        assert_eq!(r.tune_on, ["drain_time_constraint"]);
        assert_eq!(r.tune_off, ["keep_awake"]);
        assert_eq!(r.events[0].value, Some(Val::Int(30)));
        let c = Plan::parse(&base.replace(
            "GROUP",
            r#"{"recipes":["p"],"workloads":["w"],"churn":{"cycles":3},
                "events":[{"at_frame":5,"action":"set_frame_opt","key":"BaseFrameQP","value":null}]}"#,
        ))
        .unwrap();
        let r = &c.expand().unwrap()[0];
        assert_eq!(r.stage, "lab", "the default stage");
        assert_eq!(
            r.churn,
            Some(Churn {
                cycles: 3,
                frames: 30
            })
        );
        assert_eq!(r.events[0].value, None, "null removes the option");
        let l = Plan::parse(&base.replace(
            "GROUP",
            r#"{"recipes":["p"],"workloads":["w"],"ltr":{"ack":{"delay_frames":4}}}"#,
        ))
        .unwrap();
        let r = &l.expand().unwrap()[0];
        assert_eq!(r.ltr.as_ref().unwrap().ack.delay(), Ok(Some(4)));
        assert_eq!(LtrAck::Mode("all".into()).delay(), Ok(Some(0)));
        assert_eq!(LtrAck::Mode("none".into()).delay(), Ok(None));
        for bad in [
            r#"{"recipes":["p"],"workloads":["w"],"stage":"nope"}"#,
            r#"{"recipes":["p"],"workloads":["w"],"deny":["prop"]}"#,
            r#"{"recipes":["p"],"workloads":["w"],"stage":"pipe","churn":{}}"#,
            r#"{"recipes":["p"],"workloads":["w"],"ltr":{"ack":"some"}}"#,
            r#"{"recipes":["p"],"workloads":["w"],"stage":"pipe","ltr":{"ack":"all"}}"#,
        ] {
            let p = Plan::parse(&base.replace("GROUP", bad)).unwrap();
            assert!(p.expand().is_err(), "{bad}");
        }
    }

    #[test]
    fn live_groups_expand_and_bad_ones_are_refused() {
        let base = r#"{"recipes":[{"name":"p","production":{"qp":15}}],
            "workloads":[{"id":"w","kind":"synthetic","pattern":"scroll"}],"groups":[GROUP]}"#;
        let p = Plan::parse(&base.replace(
            "GROUP",
            r#"{"recipes":["p"],"workloads":["w"],"stage":"live",
                "live":{"seconds":5,"capture":{"sck_queue_depth":3,"sck_min_interval":0,
                "sck_pixel_format":"420v","sck_shows_cursor":false,"cgds_min_frame_time":0.0}}}"#,
        ))
        .unwrap();
        let r = &p.expand().unwrap()[0];
        let l = r.live.as_ref().unwrap();
        assert_eq!(l.seconds(), 5.0);
        assert_eq!(l.warmup_s(), 1.5);
        let env = l.capture.env();
        assert!(env.contains(&("SE_SCK_QUEUE_DEPTH", "3".into())));
        assert!(env.contains(&("SE_SCK_MIN_INTERVAL", "0".into())));
        assert!(env.contains(&("SE_SCK_PIXEL_FORMAT", "420v".into())));
        assert!(env.contains(&("SE_SCK_SHOWS_CURSOR", "0".into())));
        assert!(env.contains(&("SE_CGDS_MIN_FRAME_TIME", "0".into())));
        // no `live` object: defaults
        let p = Plan::parse(&base.replace(
            "GROUP",
            r#"{"recipes":["p"],"workloads":["w"],"stage":"live"}"#,
        ))
        .unwrap();
        assert_eq!(
            p.expand().unwrap()[0].live.as_ref().unwrap().seconds(),
            12.0
        );
        for bad in [
            r#"{"recipes":["p"],"workloads":["w"],"live":{}}"#,
            r#"{"recipes":["p"],"workloads":["w"],"stage":"live","sessions":2}"#,
            r#"{"recipes":["p"],"workloads":["w"],"stage":"live","events":[{"at_s":1,"action":"force_idr"}]}"#,
            r#"{"recipes":["p"],"workloads":["w"],"stage":"live","live":{"capture":{"sck_pixel_format":"bgra"}}}"#,
        ] {
            let p = Plan::parse(&base.replace("GROUP", bad)).unwrap();
            assert!(p.expand().is_err(), "{bad}");
        }
    }

    #[test]
    fn globs() {
        assert!(glob("ll-*", "ll-qp15"));
        assert!(glob("*-300M", "ll-qp15-300M"));
        assert!(glob("a*c*e", "abcde"));
        assert!(!glob("ll-*", "plain"));
        assert!(glob("exact", "exact"));
    }
}
