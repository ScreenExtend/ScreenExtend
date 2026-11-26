//! The recipe types `plan.rs` needs: the same serde shapes as macOS `vt_recipe`, so every plan
//! parses identically on both systems, plus the Windows `vendor` and `set` fields.

use serde::{Deserialize, Serialize};

/// One value. JSON spelling exactly as `vt_recipe::Val`: `null`, `true`, `60`, `600.0`,
/// `"text"`, `{"sym": "name"}` (or a list of alternatives), and `[a, b]`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Sym { sym: SymRef },
    List(Vec<Val>),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SymRef {
    One(String),
    Any(Vec<String>),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    #[default]
    H264,
    Hevc,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Recipe {
    pub name: String,
    #[serde(default)]
    pub codec: Codec,
    #[serde(default)]
    pub spec: Vec<(String, Val)>,
    #[serde(default)]
    pub props: Vec<(String, Val)>,
    #[serde(default)]
    pub frame_opts: Vec<(String, Val)>,
    #[serde(default)]
    pub flush_each_frame: bool,
    /// Windows: `nvenc` | `qsv` | `amf` | `x264`. Absent: the production ladder's choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Windows: encoder fields set after the production defaults and before the session opens,
    /// in order, each reported as applied / unknown / rejected by the lab.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<(String, Val)>,
    /// Windows lab axis: `bgra` (production; absent means this) or `nv12` (the production scaler
    /// run ahead of the deadline, for an encoder type that takes NV12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Windows lab axis: `SE_GPU_CLOCK_HOLD` for the run's child (absent: production's policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_clock_hold: Option<bool>,
    /// Windows: `SE_*` switches the child sets for itself before anything starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Windows `pipe`: one session per entry, all in one process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mix: Option<Vec<MixEntry>>,
    /// Windows `pipe`: offer the take at its own callback cadence and let the capture handler's
    /// gates decide each frame from its recorded flags; the workload's `fps:N` becomes the
    /// session rate instead of a reader decimation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gates: bool,
    /// Windows `pipe` with `gates`: the session's `autoFrameRate`, which arms the dirty-region gate.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_frame_rate: bool,
}

/// One session of a `pipe` mix. Absent fields come from the recipe and the workload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MixEntry {
    #[serde(default)]
    pub label: Option<String>,
    pub vendor: String,
    #[serde(default)]
    pub qp: Option<u8>,
    #[serde(default)]
    pub bitrate_bps: Option<u32>,
    #[serde(default)]
    pub set: Vec<(String, Val)>,
    #[serde(default)]
    pub transform: Option<String>,
    #[serde(default)]
    pub fps: Option<u32>,
    #[serde(default)]
    pub width: Option<usize>,
    #[serde(default)]
    pub height: Option<usize>,
    /// Where in the source this session starts, seconds (default: 7 s per entry).
    #[serde(default)]
    pub offset_s: Option<f64>,
}
