//! Named switches over Windows host behaviour, for A/B measurement inside one binary.
//!
//! * `SE_TUNE_OFF=<a,b,...>` turns off behaviour that ships ON ([`on`]).
//! * `SE_TUNE_ON=<a,b,...>` turns on candidates that ship OFF ([`candidate`]).
//!
//! Both are read once per process; with neither set every call site behaves as it ships.
//! A shipped build reads neither ([`crate::envcfg`]), so there every switch has its default.
//! Names are not registered anywhere: [`report`] returns the ones asked for, [`unknown`]
//! the ones the environment set that nothing asked about (a typo, or code that never ran).
//!
//! **Never call these per frame.** Each call takes a lock. Read the answer once, where the
//! session, encoder or thread is built, and keep the `bool`.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

struct Switches {
    off: Vec<String>,
    on: Vec<String>,
}

fn parse_list(var: &str) -> Vec<String> {
    crate::envcfg::var(var)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn switches() -> &'static Switches {
    static S: OnceLock<Switches> = OnceLock::new();
    S.get_or_init(|| {
        let off = parse_list("SE_TUNE_OFF");
        let on = parse_list("SE_TUNE_ON");
        if !off.is_empty() || !on.is_empty() {
            teprintln!(
                "[tune] A/B switches active: SE_TUNE_OFF=[{}] SE_TUNE_ON=[{}]",
                off.join(","),
                on.join(",")
            );
        }
        Switches { off, on }
    })
}

fn asked() -> &'static Mutex<BTreeMap<&'static str, (bool, bool)>> {
    static A: OnceLock<Mutex<BTreeMap<&'static str, (bool, bool)>>> = OnceLock::new();
    A.get_or_init(Default::default)
}

fn record(name: &'static str, default_on: bool, value: bool) -> bool {
    // a poisoned registry only loses the report; the answer is already decided
    let mut map = asked().lock().unwrap_or_else(|e| e.into_inner());
    map.insert(name, (default_on, value));
    value
}

/// Behaviour that ships on. False only when `SE_TUNE_OFF` names it.
pub fn on(name: &'static str) -> bool {
    let value = !switches().off.iter().any(|n| n == name);
    record(name, true, value)
}

/// A candidate that ships off. True only when `SE_TUNE_ON` names it.
pub fn candidate(name: &'static str) -> bool {
    let value = switches().on.iter().any(|n| n == name);
    record(name, false, value)
}

/// Every switch this process has asked about: `name -> {default, value}`.
pub fn report() -> serde_json::Value {
    let map = asked().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = serde_json::Map::new();
    for (name, (default_on, value)) in map.iter() {
        out.insert(
            (*name).to_string(),
            serde_json::json!({ "default": default_on, "value": value }),
        );
    }
    serde_json::Value::Object(out)
}

/// Names set in the environment that no call site has asked about so far.
pub fn unknown() -> Vec<String> {
    let map = asked().lock().unwrap_or_else(|e| e.into_inner());
    let s = switches();
    s.off
        .iter()
        .chain(s.on.iter())
        .filter(|n| !map.contains_key(n.as_str()))
        .cloned()
        .collect()
}

/// `SE_TEST_HOOKS=1`: the one switch every developer-only hook needs beside its own variable.
/// Read once. The A/B switches above do not need it; hooks that inject frames, replace a
/// source or override encoder settings do.
pub fn test_hooks() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::envcfg::var("SE_TEST_HOOKS").as_deref() == Ok("1"))
}

/// A developer-only variable, honoured only with `SE_TEST_HOOKS=1`. A lone one is ignored with
/// one log line; callers read each variable once.
pub fn hook_var(name: &str) -> Option<String> {
    let v = crate::envcfg::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())?;
    if !test_hooks() {
        teprintln!(
            "[tune] {name} is set but IGNORED: it is a developer hook and is honoured only \
             together with SE_TEST_HOOKS=1"
        );
        return None;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_hold_when_the_environment_names_nothing() {
        // the test process sets neither variable for these names, so each keeps its default
        assert!(on("tune_selftest_ships_on"));
        assert!(!candidate("tune_selftest_ships_off"));
        let report = report();
        assert_eq!(report["tune_selftest_ships_on"]["default"], true);
        assert_eq!(report["tune_selftest_ships_on"]["value"], true);
        assert_eq!(report["tune_selftest_ships_off"]["default"], false);
        assert_eq!(report["tune_selftest_ships_off"]["value"], false);
    }
}
