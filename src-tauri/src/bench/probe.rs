//! Probe stage: what each encoder session *is* and which properties it takes.
//!
//! One child process per row. A row creates a session for (codec, low-latency spec, hardware
//! policy, size), reads its identity and its full supported-property dictionary (every key with
//! its metadata), copies a list of read-only keys, then trial-sets every catalogue entry on a
//! *fresh* session so an accepted or rejected property cannot contaminate the next.

use std::ffi::c_void;
use std::ptr;
use std::ptr::NonNull;
use std::time::Instant;

use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::macos_utils::streamer::vt_recipe::{
    self, cf_to_json, dlsym_ptr, KeyKind, ProbeSession, Val,
};

use super::plan::{Catalogue, ProbeRow};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeSpec {
    pub row: ProbeRow,
    pub catalogue: Catalogue,
    pub result_path: String,
}

pub fn row_spec(row: &ProbeRow) -> Vec<(String, Val)> {
    let mut spec = Vec::new();
    match row.hw.as_str() {
        "require" => spec.push((
            "RequireHardwareAcceleratedVideoEncoder".into(),
            Val::Bool(true),
        )),
        "enable" => spec.push((
            "EnableHardwareAcceleratedVideoEncoder".into(),
            Val::Bool(true),
        )),
        _ => spec.push((
            "EnableHardwareAcceleratedVideoEncoder".into(),
            Val::Bool(false),
        )),
    }
    if row.low_latency {
        spec.push(("EnableLowLatencyRateControl".into(), Val::Bool(true)));
    }
    spec
}

/// `VTCopySupportedPropertyDictionaryForEncoder` (10.13, dlsym'd): the encoder VideoToolbox
/// *would* pick for this spec, without creating a session.
fn encoder_dictionary(row: &ProbeRow, spec: Option<&CFDictionary>) -> Json {
    type F = unsafe extern "C" fn(
        i32,
        i32,
        u32,
        *const c_void,
        *mut *const CFString,
        *mut *const CFDictionary,
    ) -> i32;
    let Some(p) = dlsym_ptr("VTCopySupportedPropertyDictionaryForEncoder") else {
        return json!({ "outcome": "missing_symbol" });
    };
    let f: F = unsafe { std::mem::transmute::<*mut c_void, F>(p) };
    let (mut id, mut dict) = (ptr::null(), ptr::null());
    let st = unsafe {
        f(
            row.width as i32,
            row.height as i32,
            row.codec.fourcc(),
            spec.map_or(ptr::null(), |d| (d as *const CFDictionary).cast()),
            &mut id,
            &mut dict,
        )
    };
    let id = NonNull::new(id as *mut CFString).map(|p| unsafe { CFRetained::from_raw(p) });
    let dict = NonNull::new(dict as *mut CFDictionary).map(|p| unsafe { CFRetained::from_raw(p) });
    json!({
        "status": st,
        "encoder_id": id.map(|s| s.to_string()),
        "key_count": dict.as_ref().map(|d| d.count()),
    })
}

pub fn run(spec: &ProbeSpec) -> Json {
    let t = Instant::now();
    let row = &spec.row;
    let spec_pairs = row_spec(row);
    let (spec_dict, _) = vt_recipe::build_dict(KeyKind::Spec, &spec_pairs, &[]);
    let (session, spec_report) =
        ProbeSession::create(row.width, row.height, row.codec, &spec_pairs);
    let mut out = json!({
        "id": row.id(),
        "row": row,
        "spec": spec_report,
        "encoder_dictionary": encoder_dictionary(row, spec_dict.as_deref()),
    });
    let s = match session {
        Ok(s) => s,
        Err(st) => {
            out["session_create_status"] = json!(st);
            out["wall_s"] = json!(t.elapsed().as_secs_f64());
            return out;
        }
    };
    out["session_create_status"] = json!(0);
    let id = vt_recipe::read_prop(s.vt(), "EncoderID");
    let hw = vt_recipe::read_prop(s.vt(), "UsingHardwareAcceleratedVideoEncoder");
    let gpu = vt_recipe::read_prop(s.vt(), "UsingGPURegistryID");
    out["identity"] = json!({
        "encoder_id": id.readback,
        "hardware": hw.readback, "hardware_copy_status": hw.readback_status,
        "gpu_registry_id": gpu.readback, "gpu_copy_status": gpu.readback_status,
    });
    let supported = match vt_recipe::supported_dictionary(s.vt()) {
        Ok(d) => cf_to_json(&d),
        Err(st) => json!({ "copy_status": st }),
    };
    let advertised: std::collections::HashSet<String> = supported
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    out["supported_key_count"] = json!(advertised.len());
    out["supported"] = supported;
    out["reads"] = json!(spec
        .catalogue
        .reads
        .iter()
        .map(|k| vt_recipe::read_prop(s.vt(), k))
        .collect::<Vec<_>>());
    drop(s);

    let mut trials = Vec::new();
    for (key, values) in &spec.catalogue.trials {
        for v in values {
            let (fresh, _) = ProbeSession::create(row.width, row.height, row.codec, &spec_pairs);
            match fresh {
                Ok(fs) => trials.push(json!(vt_recipe::set_prop(
                    fs.vt(),
                    key,
                    v,
                    Some(&advertised),
                    true
                ))),
                Err(st) => {
                    trials.push(json!({ "key": key, "value": v, "session_create_status": st }))
                }
            }
        }
    }
    out["trials"] = json!(trials);
    out["wall_s"] = json!(t.elapsed().as_secs_f64());
    out
}
