// The startup gate: what the compatibility report means for whether ScreenExtend may start.
// A module of its own so `scripts/test-startup-gate.mjs` can run the policy directly, and
// because a component file that also exports plain functions loses fast refresh.
//
// The rule: never stop the user unless starting is impossible. A host that can create one
// virtual display and send video gets "Continue anyway" whatever else is missing; only a
// host that cannot is stopped, and only after the app has tried to fix what it can.

import type {
  CompatibilityReport,
  DisplayDriverHealth,
  DriverRestartOutcome,
  UnsupportedApi,
} from "./bindings";

// `lib.rs`'s `SEVERITY_*` constants. `blocking`: this machine cannot stream at all.
// `installable`: a row the app can fix itself.
export const SEVERITY_BLOCKING = "blocking";
export const SEVERITY_INSTALLABLE = "installable";

// `api_key::DISPLAY_DRIVER*` from `lib.rs`. Matched by key, not by prose, so rewording the
// copy cannot silently turn the remediation off.
export const DRIVER_KEY = "display-driver";
export const DRIVER_UNRESPONSIVE_KEY = "display-driver-unresponsive";
export const DRIVER_BUSY_KEY = "display-driver-busy";

export type CompatGate =
  | { kind: "ok" }
  // another ScreenExtend process holds the display driver: nothing is broken or installable
  | { kind: "busy"; rows: UnsupportedApi[] }
  // rows the app can fix itself: fix, re-probe, then decide
  | { kind: "remediate"; rows: UnsupportedApi[] }
  // cannot create a display or send video, remediation already tried: no way past this one
  | { kind: "blocked"; rows: UnsupportedApi[] }
  // something is missing but the host still streams: offer "Continue anyway"
  | { kind: "warn"; rows: UnsupportedApi[] };

export function classifyCompat(
  report: CompatibilityReport,
  opts: { remediated: boolean; dontShowAgain: boolean }
): CompatGate {
  // first, before any remediation: a driver held by another ScreenExtend process is not a
  // fault and must not be reinstalled, and nothing else about this host can be established
  // until that instance is gone
  const busy = report.unsupported_apis.filter(a => a.key === DRIVER_BUSY_KEY);
  if (busy.length > 0 && report.os_supported) return { kind: "busy", rows: busy };

  const installable = report.unsupported_apis.filter(a => a.severity === SEVERITY_INSTALLABLE);
  if (installable.length > 0 && !opts.remediated) {
    return { kind: "remediate", rows: installable };
  }
  // installable rows are hidden while the app can still fix them, shown once it has tried
  const visible = opts.remediated
    ? report.unsupported_apis
    : report.unsupported_apis.filter(a => a.severity !== SEVERITY_INSTALLABLE);
  // an installable row that survived its own remediation blocks: tried, and it still fails
  const blocked =
    !report.os_supported ||
    visible.some(a => a.severity === SEVERITY_BLOCKING) ||
    installable.length > 0;
  if (blocked) return { kind: "blocked", rows: visible };
  if (visible.length > 0 && !opts.dontShowAgain) return { kind: "warn", rows: visible };
  return { kind: "ok" };
}

/// Which rows this launch can act on, and how. A driver that is installed but not answering is
/// removed before it is installed again: installing over one that is already there changes
/// nothing.
///
/// A busy driver is deliberately absent: it has its own gate kind, and nothing here may reach
/// for the elevated installer on account of it.
export function driverRemediation(rows: UnsupportedApi[]): "reinstall" | "install" | null {
  if (rows.some(a => a.key === DRIVER_UNRESPONSIVE_KEY)) return "reinstall";
  if (rows.some(a => a.key === DRIVER_KEY)) return "install";
  return null;
}

/// The driver-memory gate, asked after the compatibility gate (so a missing driver has already
/// been installed) and before `setup()` (so no display of ours exists yet to be taken away).
///
/// The bundled display driver leaks memory into its host process with every display it creates
/// and only a device restart frees it. Past a threshold the only ways out are restart or exit:
/// there is deliberately no "continue anyway", because the alternative is every join timing
/// out. A failed health read, or a platform with no such driver, stops nobody.
export type DriverHealthGate =
  | { kind: "ok" }
  | { kind: "restart"; commitMb: number; threads: number };

export function classifyDriverHealth(health: DisplayDriverHealth | null): DriverHealthGate {
  if (!health || !health.supported || !health.needs_restart) return { kind: "ok" };
  return { kind: "restart", commitMb: health.commit_mb, threads: health.threads };
}

/// The i18n key for why a restart did not happen, keyed on `DriverRestartOutcome.reason`.
/// Anything unrecognised reads as the generic failure rather than as nothing.
export const RESTART_FAILURE_REASONS = [
  "declined",
  "timeout",
  "refused",
  "notElevated",
  "needsReboot",
  "notInstalled",
  "failed",
] as const;

export function restartFailureKey(outcome: DriverRestartOutcome | null): string {
  const reason = outcome?.reason ?? "failed";
  const known = (RESTART_FAILURE_REASONS as readonly string[]).includes(reason);
  return `bootstrap.driverRestart.failure.${known ? reason : "failed"}`;
}

/// What this machine has already tried to do about the display driver since it last booted.
///
/// Every field is a count with a budget of one: a second unrequested elevation prompt for the
/// same fault is worse than doing nothing, and a relaunch that does not fix the driver would
/// repeat on every launch. Scoped to `boot` because restarting the machine is the advice given
/// when the budget runs out, so a restart has to restore it.
export type DriverRepair = {
  boot: string;
  install: number;
  reinstall: number;
  relaunch: number;
};

export type RepairAttempt = "install" | "reinstall" | "relaunch";

export const NO_REPAIRS: DriverRepair = { boot: "", install: 0, reinstall: 0, relaunch: 0 };

/// The saved record, or a fresh one if it belongs to a previous boot.
///
/// An empty `boot` (every platform but Windows, or a failed derivation there) is treated as
/// "this boot", so the budget still holds within a launch. A restart does not reset it there,
/// which costs nothing: no other platform has a driver to install.
export function repairForBoot(saved: DriverRepair | null | undefined, boot: string): DriverRepair {
  if (!saved || saved.boot !== boot) return { ...NO_REPAIRS, boot };
  return {
    boot,
    install: saved.install || 0,
    reinstall: saved.reinstall || 0,
    relaunch: saved.relaunch || 0,
  };
}

export function mayAttempt(repair: DriverRepair, what: RepairAttempt): boolean {
  return repair[what] < 1;
}

export function recordAttempt(repair: DriverRepair, what: RepairAttempt): DriverRepair {
  return { ...repair, [what]: repair[what] + 1 };
}

/// First-run values the store still lacks. The repair record above can be written first, and a
/// store with any key no longer reads as a first run. `storedKeys` are the file's own keys;
/// hosted network credentials that are both empty count as never created.
export function firstRunGaps(
  storedKeys: readonly string[],
  hostedNetwork: { name: string; password: string } | null | undefined,
): { name: boolean; hostedNetwork: boolean } {
  return {
    name: !storedKeys.includes("name"),
    hostedNetwork:
      !storedKeys.includes("hostedNetworkCredentials")
      || !hostedNetwork
      || (hostedNetwork.name === "" && hostedNetwork.password === ""),
  };
}

/// Why startup stopped, which decides what the Setup Error dialog says.
///
/// - `setup`: the Rust `setup` command returned false or threw. Drivers, capture or the
///   encoder, and the only class that may offer "Install Drivers".
/// - `settings`: the saved configuration could not be read or replayed. Nothing is wrong with
///   the host.
/// - `internal`: anything else in the bootstrap itself. Not a verdict on the machine either.
export type SetupFailure = "setup" | "settings" | "internal";

export function classifySetupFailure(what: {
  setupFailed: boolean;
  settingsFailed: boolean;
}): SetupFailure {
  // `setup` outranks the rest: with no virtual display nothing downstream could have worked
  if (what.setupFailed) return "setup";
  if (what.settingsFailed) return "settings";
  return "internal";
}

/// The i18n key for the dialog's leading sentence; the rest of the body is shared.
export function setupErrorBodyKey(failure: SetupFailure): string {
  switch (failure) {
    case "settings":
      return "bootstrap.setupError.settingsBody";
    case "internal":
      return "bootstrap.setupError.internalBody";
    default:
      return "bootstrap.setupError.bodyBeforePrompt";
  }
}

/// Whether this failure may offer to install the display driver.
///
/// Only a failing `setup()` may: an elevated driver install cannot improve a JavaScript error
/// or a malformed settings file, and offering it there turns a config typo into a UAC prompt.
export function offersDriverInstallFor(
  failure: SetupFailure,
  report: CompatibilityReport | null
): boolean {
  return failure === "setup" && offersDriverInstall(report);
}

/// Whether the "Install drivers" control has anything to install: only a host carrying an
/// installable display-driver row has a driver (on macOS `install_drivers` does nothing).
export function offersDriverInstall(report: CompatibilityReport | null): boolean {
  return (report?.unsupported_apis ?? []).some(
    a => a.severity === SEVERITY_INSTALLABLE && a.key.startsWith(DRIVER_KEY)
  );
}
