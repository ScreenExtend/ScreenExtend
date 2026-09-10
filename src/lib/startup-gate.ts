// The startup gate: what the compatibility report means for whether ScreenExtend may start.
//
// Its own module rather than three functions inside `bootstrap.tsx` for two reasons. It is a
// policy, and a policy that is only exercised by launching the app on a broken machine is a
// policy nobody has checked — `scripts/test-startup-gate.mjs` runs it directly. And a component
// file that also exports plain functions loses fast refresh for the whole file.
//
// The rule, from the owner: never stop the user unless it is genuinely impossible. A host that
// can create one virtual display and send video is fine, whatever else is missing — no hosted
// network, no audio, no Accessibility, no hardware encoder — and gets "Continue anyway". Only a
// host that cannot do the bare minimum is stopped, and only after the app has tried to fix
// whatever it can fix itself.

import type { CompatibilityReport, UnsupportedApi } from "./bindings";

// The severities the host classifies each missing capability with. `blocking` is the only one
// that means "this machine cannot stream at all"; `installable` is a row the app can fix itself.
// They are `lib.rs`'s `SEVERITY_*` constants.
export const SEVERITY_BLOCKING = "blocking";
export const SEVERITY_INSTALLABLE = "installable";

// `api_key::DISPLAY_DRIVER*` from `lib.rs`. Matching on these rather than on the prose means
// the copy can be reworded without silently turning the remediation off.
export const DRIVER_KEY = "display-driver";
export const DRIVER_UNRESPONSIVE_KEY = "display-driver-unresponsive";
export const DRIVER_BUSY_KEY = "display-driver-busy";

export type CompatGate =
  // Nothing in the way; start.
  | { kind: "ok" }
  // Another ScreenExtend process has the display driver. Nothing is broken and nothing may be
  // installed; the only way on is for that process to go.
  | { kind: "busy"; rows: UnsupportedApi[] }
  // Rows the app can fix by itself. Fix them, re-probe, then decide.
  | { kind: "remediate"; rows: UnsupportedApi[] }
  // The host cannot create a display or send video by any route, and remediation has already
  // been tried. This is the only state with no way past it.
  | { kind: "blocked"; rows: UnsupportedApi[] }
  // Something is missing and the host still streams. Say so, offer "Continue anyway".
  | { kind: "warn"; rows: UnsupportedApi[] };

export function classifyCompat(
  report: CompatibilityReport,
  opts: { remediated: boolean; dontShowAgain: boolean }
): CompatGate {
  // Before anything else, and before any remediation. A driver held by one of our own processes
  // is not a fault, is not installable and cannot be fixed by installing anything — the old flow
  // read it as an unresponsive driver, reinstalled a healthy one behind a UAC prompt, re-probed
  // into the same state and went round. It outranks the optional rows underneath it because
  // until the other instance is gone nothing else about this host can be established.
  const busy = report.unsupported_apis.filter(a => a.key === DRIVER_BUSY_KEY);
  if (busy.length > 0 && report.os_supported) return { kind: "busy", rows: busy };

  const installable = report.unsupported_apis.filter(a => a.severity === SEVERITY_INSTALLABLE);
  if (installable.length > 0 && !opts.remediated) {
    return { kind: "remediate", rows: installable };
  }
  // Installable rows are not shown while they are still the app's problem to solve; once
  // remediation has been tried they are exactly what the user needs to see.
  const visible = opts.remediated
    ? report.unsupported_apis
    : report.unsupported_apis.filter(a => a.severity !== SEVERITY_INSTALLABLE);
  // An installable row that survived its own remediation is what "we tried and it still does
  // not work" looks like — the one case the owner named for a modal with no way past it.
  const blocked =
    !report.os_supported ||
    visible.some(a => a.severity === SEVERITY_BLOCKING) ||
    installable.length > 0;
  if (blocked) return { kind: "blocked", rows: visible };
  if (visible.length > 0 && !opts.dontShowAgain) return { kind: "warn", rows: visible };
  return { kind: "ok" };
}

/// Which rows this launch can act on, and how. A driver that is installed but not answering is
/// removed before it is installed again — installing over a driver that is already there is
/// what the old flow did, and it changed nothing.
///
/// A busy driver is deliberately absent: it has its own key and its own gate kind, and nothing
/// here may ever reach for the elevated installer on account of it.
export function driverRemediation(rows: UnsupportedApi[]): "reinstall" | "install" | null {
  if (rows.some(a => a.key === DRIVER_UNRESPONSIVE_KEY)) return "reinstall";
  if (rows.some(a => a.key === DRIVER_KEY)) return "install";
  return null;
}

/// What this machine has already tried to do about the display driver since it last booted.
///
/// Every field is a count, and the budget for each is one. That is the whole design: an
/// elevation prompt the user did not ask for is expensive, a *second* one for the same fault is
/// worse than doing nothing, and a relaunch that does not fix the driver relaunches again on the
/// next launch forever. Scoped to `boot` because "restart this machine" is the advice given when
/// the budget runs out, so restarting has to be what restores it.
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
/// An empty `boot` — every platform but Windows, and Windows if the derivation ever fails — is
/// treated as "this boot", so the budget still holds within a launch. It cannot survive a
/// restart there, which costs nothing: no other platform has a driver to install.
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

/// Whether the "Install drivers" control has anything to install.
///
/// It used to be offered on every platform, including macOS, where `install_drivers` returns
/// `true` without doing anything — so the dialog told the user to press a button that could not
/// help. The report knows: only a host carrying an installable display-driver row has a driver.
export function offersDriverInstall(report: CompatibilityReport | null): boolean {
  return (report?.unsupported_apis ?? []).some(
    a => a.severity === SEVERITY_INSTALLABLE && a.key.startsWith(DRIVER_KEY)
  );
}
