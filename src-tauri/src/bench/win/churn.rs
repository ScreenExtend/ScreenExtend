//! `churn` on Windows: encoder create -> K frames -> destroy, M cycles, per backend, with
//! `super::sys::resources()` every 10 cycles.
//!
//! STUB. Not implemented yet: every run returns `result: "skipped"`, which the runner records as
//! `outcome: skipped`.

use serde_json::Value as Json;

use super::super::plan::RunSpec;

pub fn run(spec: &RunSpec) -> Json {
    super::not_implemented(spec, "churn")
}
