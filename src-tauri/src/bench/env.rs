//! Env stage: the machine, the OS, the binary, and the encoders VideoToolbox lists.

use std::path::Path;
use std::process::Command;

use serde_json::{json, Value as Json};

use super::cf::{self, sysctl_int, sysctl_string};

fn cmd(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Variables that change what the encoder does. The runner clears them for every child, and this
/// records what the parent had so a result can say so.
pub const CLEARED_VARS: &[&str] = &[
    "SE_VT_RECIPE",
    "SE_VT_QP_BPS",
    "SE_VT_SIM_HOLD",
    "SE_HOLD_PUSH_START",
    "SE_TEST_SOURCE",
    "SE_PERF_HOLD",
    "SE_PERF_HOLD_LINGER_MS",
    "SE_VT_SIM_HOLD_DELAY_MS",
    "SE_VT_SIM_HOLD_COST_MS",
    "SE_PLI_HEALTHY_CAP_MS",
    "SE_QP_CEILING_MBPS",
    "SE_BENCH_CLIENT_FRAME_MS",
    "SE_RAW_RECORD",
    "SE_FRAME_TRACE",
    "SE_INTRA_REFRESH",
    "SE_REQUIRE_HW",
    // set per group from the plan (`deny`, `tune_on`, `tune_off`), never inherited
    "SE_VT_DENY",
    "SE_TEST_HOOKS",
    "SE_TUNE_OFF",
    "SE_TUNE_ON",
    // live stage: capture switches, the Screen Recording test hook and the private data dir
    "SE_SCK_QUEUE_DEPTH",
    "SE_SCK_MIN_INTERVAL",
    "SE_SCK_PIXEL_FORMAT",
    "SE_SCK_SHOWS_CURSOR",
    "SE_CGDS_QUEUE_DEPTH",
    "SE_CGDS_MIN_FRAME_TIME",
    "SE_BENCH_DENY_SCREEN_RECORDING",
    "SCREENEXTEND_DATA_DIR",
];

pub fn collect(out_dir: &Path, parent_env: &Json) -> Json {
    let translated = sysctl_int("sysctl.proc_translated");
    let mut load = [0f64; 3];
    let n = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    json!({
        "hostname": cf::hostname(),
        "collected_at": cf::local_stamp(),
        "os": {
            "product_version": sysctl_string("kern.osproductversion")
                .or_else(|| cmd("sw_vers", &["-productVersion"])),
            "build": sysctl_string("kern.osversion"),
            "kernel": sysctl_string("kern.version"),
        },
        "hw": {
            "model": sysctl_string("hw.model"),
            "cpu_brand": sysctl_string("machdep.cpu.brand_string"),
            "physical_cpu": sysctl_int("hw.physicalcpu"),
            "logical_cpu": sysctl_int("hw.logicalcpu"),
            "perflevel0_physical": sysctl_int("hw.perflevel0.physicalcpu"),
            "perflevel1_physical": sysctl_int("hw.perflevel1.physicalcpu"),
            "memsize_bytes": sysctl_int("hw.memsize"),
        },
        "process": {
            "arch": std::env::consts::ARCH,
            // absent on a Mac with no Rosetta (and on 10.15 Intel): reported as 0 there
            "translated": translated.unwrap_or(0),
            "translated_sysctl_present": translated.is_some(),
        },
        "power": {
            "pmset_ps": cmd("pmset", &["-g", "ps"]),
            "low_power_mode": cf::low_power_mode(),
            "thermal_state": cf::thermal_state(),
            "loadavg": if n == 3 { json!(load) } else { Json::Null },
        },
        "disk_free_bytes": cf::free_disk_bytes(out_dir),
        "build": {
            "git_describe": option_env!("SE_BENCH_GIT_DESCRIBE"),
            "package_version": env!("CARGO_PKG_VERSION"),
            "features": {
                "bench": cfg!(feature = "bench"),
                "probe": cfg!(feature = "probe"),
            },
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "target_arch": std::env::consts::ARCH,
            "executable": std::env::current_exe().ok().map(|p| p.display().to_string()),
        },
        "parent_env_cleared_for_children": parent_env,
        "video_encoder_list": cf::video_encoder_list(),
    })
}
