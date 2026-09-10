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

export type CompatGate =
  // Nothing in the way; start.
  | { kind: "ok" }
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
export function driverRemediation(rows: UnsupportedApi[]): "reinstall" | "install" | null {
  if (rows.some(a => a.key === DRIVER_UNRESPONSIVE_KEY)) return "reinstall";
  if (rows.some(a => a.key === DRIVER_KEY)) return "install";
  return null;
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
