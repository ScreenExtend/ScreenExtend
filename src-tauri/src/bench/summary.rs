//! `SUMMARY.txt`: a short human-readable digest of a results file.

use std::collections::BTreeMap;
use std::fmt::Write;

use serde_json::Value as Json;

fn med(mut v: Vec<f64>) -> Option<f64> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

fn f(v: Option<f64>, digits: usize) -> String {
    v.map_or("-".into(), |x| format!("{x:.digits$}"))
}

pub fn render(r: &Json) -> String {
    let mut s = String::new();
    let env = &r["env"];
    if env.is_null() {
        let _ = writeln!(
            s,
            "ScreenExtend bench - {} - env stage not run in this directory",
            r["host"].as_str().unwrap_or("?")
        );
    } else if cfg!(windows) {
        let _ = writeln!(
            s,
            "ScreenExtend bench | {} | {} {} | {} {} (build {}.{}) | arch {}",
            r["host"].as_str().unwrap_or("?"),
            env["hw"]["model"].as_str().unwrap_or("?"),
            env["hw"]["cpu_brand"].as_str().unwrap_or("?"),
            env["os"]["product_name"].as_str().unwrap_or("Windows"),
            env["os"]["display_version"].as_str().unwrap_or("?"),
            env["os"]["build"]
                .as_str()
                .map_or_else(|| env["os"]["build"].to_string(), str::to_string),
            env["os"]["ubr"],
            env["process"]["arch"].as_str().unwrap_or("?"),
        );
    } else {
        let _ = writeln!(
            s,
            "ScreenExtend bench - {} - {} {} - macOS {} ({}) - arch {} translated={}",
            r["host"].as_str().unwrap_or("?"),
            env["hw"]["model"].as_str().unwrap_or("?"),
            env["hw"]["cpu_brand"].as_str().unwrap_or("?"),
            env["os"]["product_version"].as_str().unwrap_or("?"),
            env["os"]["build"].as_str().unwrap_or("?"),
            env["process"]["arch"].as_str().unwrap_or("?"),
            env["process"]["translated"],
        );
    }
    let _ = writeln!(s, "plan: {}", r["plan_source"].as_str().unwrap_or("?"));

    if let Some(probe) = r["probe"].as_array().filter(|p| !p.is_empty()) {
        let _ = writeln!(s, "\nPROBE  row / create / encoder / hardware / supported keys / trials set|rejected|missing");
        for p in probe {
            let trials = p["trials"].as_array().cloned().unwrap_or_default();
            let count = |o: &str| trials.iter().filter(|t| t["outcome"] == o).count();
            let _ = writeln!(
                s,
                "  {:<32} {:>6} {:<44} {:<5} {:>3}  {}|{}|{}{}",
                p["id"].as_str().unwrap_or("?"),
                p["session_create_status"],
                p["identity"]["encoder_id"].as_str().unwrap_or("-"),
                p["identity"]["hardware"],
                p["supported_key_count"],
                count("set"),
                count("rejected"),
                count("missing_symbol"),
                if p["outcome"] != "ok" {
                    format!("  [{}]", p["outcome"].as_str().unwrap_or("?"))
                } else {
                    String::new()
                }
            );
        }
    }

    let runs = r["runs"].as_array().cloned().unwrap_or_default();
    let mut groups: BTreeMap<(String, String), Vec<&Json>> = BTreeMap::new();
    for run in runs.iter().filter(|x| x["stage"] != "live") {
        let recipe = run["spec"]["recipe"]["name"]
            .as_str()
            .unwrap_or("?")
            .to_string();
        let wl = run["spec"]["workload"]["id"]
            .as_str()
            .unwrap_or("?")
            .to_string();
        groups.entry((wl, recipe)).or_default().push(run);
    }
    let _ = writeln!(
        s,
        "\nLAB (medians over repeats)\n  {:<22} {:<34} {:>5} {:>7} {:>6} {:>7} {:>7} {:>7} {:>7} {:>5} {:>6}",
        "workload", "recipe", "ok/n", "drop%", "run", "lat50", "lat99", "lat999", "Mbps", "lvl", "psnr"
    );
    for ((wl, recipe), rs) in &groups {
        let ok: Vec<&&Json> = rs
            .iter()
            .filter(|x| x["outcome"] == "ok" && x["result"] == "completed")
            .collect();
        let valid = ok.iter().filter(|x| x["valid"] == true).count();
        let num = |k: &dyn Fn(&Json) -> Option<f64>| med(ok.iter().filter_map(|x| k(x)).collect());
        let level = ok.iter().find_map(|x| {
            x["sps"]["level_idc"]
                .as_u64()
                .or(x["sps"]["general_level_idc"].as_u64())
        });
        let _ = writeln!(
            s,
            "  {:<22} {:<34} {:>2}/{:<2} {:>7} {:>6} {:>7} {:>7} {:>7} {:>7} {:>5} {:>6}",
            wl,
            recipe,
            valid,
            rs.len(),
            f(num(&|x| x["dropped_pct"].as_f64()), 2),
            f(num(&|x| x["longest_drop_run"].as_f64()), 0),
            f(num(&|x| x["latency_ms"]["p50"].as_f64()), 2),
            f(num(&|x| x["latency_ms"]["p99"].as_f64()), 2),
            f(num(&|x| x["latency_ms"]["p999"].as_f64()), 2),
            f(num(&|x| x["avg_mbps"].as_f64()), 2),
            level.map_or("-".into(), |l| l.to_string()),
            f(num(&|x| x["psnr"]["sharp"]["mean"].as_f64()), 2),
        );
    }
    let live: Vec<&Json> = runs.iter().filter(|x| x["stage"] == "live").collect();
    if !live.is_empty() {
        let _ = writeln!(
            s,
            "\nLIVE (one row per run; ms p50/p99)\n  {:<44} {:<8} {:<16} {:>5} {:>5} {:>5} {:>5} {:>13} {:>11} {:>13} {:>7}",
            "run", "mode", "capture", "pres", "capt", "recv", "dec", "cap->arrival", "decode", "cap->decoded", "Mbps"
        );
        for x in &live {
            let fr = &x["frames"];
            let r = &x["receiver_ms"];
            let pp = |k: &str| {
                format!(
                    "{}/{}",
                    f(r[k]["p50"].as_f64(), 2),
                    f(r[k]["p99"].as_f64(), 2)
                )
            };
            let _ = writeln!(
                s,
                "  {:<44} {:<8} {:<16} {:>5} {:>5} {:>5} {:>5} {:>13} {:>11} {:>13} {:>7}{}",
                x["id"].as_str().unwrap_or("?"),
                x["workload_mode"].as_str().unwrap_or("-"),
                x["capture_backend"].as_str().unwrap_or("-"),
                fr["presented_in_window"],
                fr["captured_fresh_in_window"],
                fr["received"],
                fr["decoded"],
                pp("capture_to_arrival"),
                pp("decode"),
                pp("capture_to_decoded"),
                f(x["avg_mbps"].as_f64(), 1),
                if x["valid"] == true { "" } else { "  INVALID" },
            );
        }
    }
    let bad: Vec<&Json> = runs
        .iter()
        .filter(|x| x["outcome"] != "ok" || x["result"] != "completed" || x["valid"] != true)
        .collect();
    if !bad.is_empty() {
        let _ = writeln!(s, "\nNOT OK / INVALID");
        for b in bad {
            let why = b["reason"]
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    b["invalid_reasons"].as_array().map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                })
                .unwrap_or_default();
            let _ = writeln!(
                s,
                "  {:<60} outcome={} result={} create={} {}",
                b["id"].as_str().unwrap_or("?"),
                b["outcome"].as_str().unwrap_or("?"),
                b["result"].as_str().unwrap_or("-"),
                b["session_create_status"],
                why
            );
        }
    }
    s
}
