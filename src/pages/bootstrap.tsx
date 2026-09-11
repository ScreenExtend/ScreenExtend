import { useEffect, useRef, useState, type ReactNode } from "react";
import { useNavigate } from "react-router-dom";

import { Check, Info, Loader2, X } from "lucide-react";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle
} from "@/components/ui/alert-dialog";
import { Checkbox } from "@/components/ui/checkbox";
import { toast } from "@/components/ui/use-toast";

import {
  createConfig,
  defaultConfig,
  getConfig,
  updateConfig,
  DEFAULT_AUTO_FRAME_RATE,
  DEFAULT_POINTER_SCOPE,
  type Config,
} from "@/components/config-provider";
import { useGlobalSetters, useLoaded } from "@/components/global-provider";
import { commands, type CompatibilityReport, type PermissionStatus, type UnsupportedApi } from "@/lib/bindings";
import { generateOtp, readQrValues } from "@/lib/utils";
import {
  classifyCompat,
  classifySetupFailure,
  driverRemediation,
  mayAttempt,
  offersDriverInstall,
  offersDriverInstallFor,
  recordAttempt,
  repairForBoot,
  setupErrorBodyKey,
  type DriverRepair,
  type RepairAttempt,
  type SetupFailure,
} from "@/lib/startup-gate";
import { scheduleConfigEditorPreload } from "@/lib/preload-editor";
import { loadDashboard, loadDevices, loadSettings } from "@/lib/route-chunks";
import { useTranslation } from "@/i18n";
import { useTheme, type Theme } from "@/components/theme-provider";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { open as openUrl } from "@tauri-apps/plugin-shell";

const DOWNLOAD_URL = "https://screenextend.app";
// The gate is open while the user is in System Settings, which is minutes, not seconds.
// 200 ms was five IPC round-trips a second into TCC for the whole of it. The `focus`
// listener below is what actually makes the dialog feel instant -- returning to the app
// re-checks immediately -- so the interval only has to cover the case where the app never
// loses focus. First two seconds at 200 ms, then 1 s, then 3 s: about 26 calls in the
// first minute against roughly 300.
const PERMISSION_POLL_STEPS = [
  { until: 2000, every: 200 },
  { until: 10000, every: 1000 },
  { until: Infinity, every: 3000 },
];
const UPDATE_CHECK_DEADLINE_MS = 2000;

function samePermissions(a: PermissionStatus[] | null, b: PermissionStatus[]): boolean {
  return a !== null
    && a.length === b.length
    && a.every((p, i) => p.key === b[i].key
      && p.state === b[i].state
      && p.relaunch_required === b[i].relaunch_required
      && p.detail === b[i].detail);
}

// "unknown" is a probe that could not learn the answer, not a refusal. Only a
// definite "denied" is allowed to hold up startup.
function blocks(p: PermissionStatus): boolean {
  return p.required && p.state === "denied";
}

// A repair that does not fix the driver would be tried again on the next launch, forever, and
// two of the three ask for elevation. `driverRepair` in the config store is the record that
// stops that; this is a second, cheaper belt across one launch, because the store is written
// asynchronously and a relaunch can beat the write to disk.
const RELAUNCH_MEMO_KEY = "se.driverRelaunchAt";
const RELAUNCH_WINDOW_MS = 10 * 60 * 1000;

function relaunchAlreadyTried(): boolean {
  try {
    const at = Number(window.localStorage.getItem(RELAUNCH_MEMO_KEY) ?? "");
    return Number.isFinite(at) && Date.now() - at < RELAUNCH_WINDOW_MS;
  } catch {
    // Storage can be unavailable. Treat that as "already tried": a relaunch loop is worse than
    // a relaunch that never happens, and every other route out of this state still works.
    return true;
  }
}

function rememberRelaunch(): void {
  try {
    window.localStorage.setItem(RELAUNCH_MEMO_KEY, String(Date.now()));
  } catch {
    /* see relaunchAlreadyTried */
  }
}

// How long to wait after asking another instance to quit before probing again. It closes its
// window, broadcasts BYE to every connected device and removes their virtual displays on the
// way out, which is what has to finish before the driver is ours to open.
const HOLDER_QUIT_SETTLE_MS = 2500;

function needsRelaunch(perms: PermissionStatus[] | null): boolean {
  return (perms ?? []).some(p => p.relaunch_required);
}

function withBold(template: string, value: ReactNode): ReactNode {
  const [before, after = ""] = template.split(/\{\w+\}/);
  return (
    <>
      {before}
      <b>{value}</b>
      {after}
    </>
  );
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(1)} ${units[unit]}`;
}

export default function Bootstrap() {
  const navigate = useNavigate();
  const { theme, setTheme } = useTheme();
  const { t } = useTranslation();
  const loaded = useLoaded();
  const { setLoaded, setOtp, setHostedNetworkOn, setSessionId, setQrValues, setPublicSessionsEnabled } = useGlobalSetters();

  // Why startup stopped, or null while it has not. It was a bare boolean, and the dialog it
  // opened said "core drivers or libraries" whatever had gone wrong -- including a `TypeError`
  // out of the saved-settings replay reading a malformed `config.json`.
  const [failure, setFailure] = useState<SetupFailure | null>(null);
  const [loading, setLoading] = useState(false);
  const [compatReport, setCompatReport] = useState<CompatibilityReport | null>(null);
  const [compatBlocking, setCompatBlocking] = useState(false);
  // The last report the probe produced, kept so the setup-error dialog can ask it whether this
  // host has a driver to install instead of guessing from the platform name.
  const [lastReport, setLastReport] = useState<CompatibilityReport | null>(null);
  const [remediating, setRemediating] = useState(false);
  // The display driver is held by another ScreenExtend process. Its own dialog, because the
  // only two useful actions -- quit that process, try again -- are neither "continue" nor
  // "exit", and because nothing here may install anything. The pid is the whole payload: the
  // host's row says the same thing in prose, and one number is easier to translate around.
  const [busyHolder, setBusyHolder] = useState<number | null | undefined>(undefined);
  const [compatDontShowAgain, setCompatDontShowAgain] = useState(true);
  const [permReport, setPermReport] = useState<PermissionStatus[] | null>(null);
  const [updating, setUpdating] = useState(false);
  const [updateFailed, setUpdateFailed] = useState(false);
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);
  const [downloaded, setDownloaded] = useState(0);
  const [total, setTotal] = useState(0);
  const running = useRef(false);
  // Set by anything in the saved-settings phase that did not work. It does not open a dialog on
  // its own -- a setting the host would not take is not a reason to refuse to start -- but it is
  // what lets the dialog name the right cause if startup then fails for another reason.
  const settingsFailed = useRef(false);

  // Read the saved configuration, creating one on the first run.
  //
  // Never throws. A configuration that cannot be read is not one of the three things allowed to
  // stop the app (`startup-gate.ts`): the host can still create a display, capture and encode,
  // so it starts on the defaults and says in the log that it did. The old version threw
  // `config store unreadable after setup` into the catch that opened the driver dialog.
  const loadSavedSettings = async (): Promise<Config> => {
    try {
      let existing = await getConfig();
      if (!existing) {
        await createConfig({ name: await commands.getUsername(), theme });
        existing = await getConfig();
      }
      if (existing) return existing;
      console.error("[bootstrap] the config store is still empty after setup; using the defaults");
    } catch (e) {
      console.error("[bootstrap] the saved configuration could not be read; using the defaults", e);
    }
    settingsFailed.current = true;
    // A copy: `defaultConfig` is module-level and shared with every other reader of it.
    return { ...defaultConfig };
  };

  // Push the saved configuration into the host. Every step is individually survivable: one
  // setting the host rejects must not cost the user the other settings, and it must never cost
  // them the dashboard. What it costs is a line in the log and the toast below.
  const replaySavedSettings = async (existing: Config): Promise<void> => {
    const replay: Promise<unknown>[] = [];
    const step = (what: string, run: () => Promise<unknown>) => {
      let started: Promise<unknown>;
      try {
        started = run();
      } catch (e) {
        // A synchronous throw out of a call site, e.g. reading a field off a value the store
        // held in some shape nothing here anticipated.
        settingsFailed.current = true;
        console.error(`[bootstrap] applying ${what} failed`, e);
        return;
      }
      replay.push(started.catch((e: unknown) => {
        settingsFailed.current = true;
        console.error(`[bootstrap] applying ${what} failed`, e);
      }));
    };

    step("theme", () => setTheme(existing.theme as Theme));
    const savedPorts = existing.serverPorts;
    if (savedPorts) {
      step("serverPorts", () => commands.setServerPorts(savedPorts.http, savedPorts.https));
    }
    if (existing.disableGpuEncode) {
      step("disableGpuEncode", () => commands.setDisableGpuEncode(true));
    }
    if (existing.legacyVolumeKeyProxy) {
      step("legacyVolumeKeyProxy", () => commands.setLegacyVolumeKeyProxy(true));
    }
    // Independent per device, so replay them together rather than one IPC round-trip
    // at a time: with a dozen saved devices the serial version was the longest single
    // stretch between setup() returning and the dashboard appearing.
    for (const device of existing.devices) {
      // Keyed by the device token. A row with none names only an address, which is
      // recycled between machines, so it waits for that device to join again.
      if (!device.token) {
        console.log(`[migrate] saved device ${device.ip} has no token; its settings wait for it to join once more`);
        continue;
      }
      step(`devices[${device.token}]`, () => commands.setDeviceOverride(
        device.token,
        device.scale,
        device.orientation,
        device.refreshRate,
        device.videoScale,
        device.videoQuality,
        device.remoteControl ?? false,
        device.dpr ?? device.maxDpr ?? 1,
        device.systemAudio ?? false
      ));
      step(`devices[${device.token}].pointerScope`, () =>
        commands.setDevicePointerScope(device.token, device.pointerScope ?? DEFAULT_POINTER_SCOPE));
      step(`devices[${device.token}].autoFrameRate`, () =>
        commands.setDeviceAutoFrameRate(device.token, device.autoFrameRate ?? DEFAULT_AUTO_FRAME_RATE));
    }
    for (const known of existing.knownDevices) {
      const token = known.token ?? "";
      if (known.banned) {
        step(`knownDevices[${token || known.ip}]`, () => commands.setDeviceBanned(token, known.ip, true));
      } else if (token) {
        step(`knownDevices[${token}]`, () => commands.setDeviceApproved(token, true));
      } else {
        console.log(`[migrate] known device ${known.ip} has no trust token; it will need to re-enter the code once`);
      }
    }
    const turn = existing.turnConfig;
    if (turn?.urls) {
      step("turnConfig", () =>
        commands.setTurnConfig(turn.urls, turn.username ?? "", turn.credential ?? ""));
    }
    await Promise.all(replay);
  };

  // The catch-all classifier. `setup()` has its own answer above; everything reaching here is
  // either a settings phase that already recorded itself or the bootstrap failing on its own.
  const failStartup = () =>
    setFailure(classifySetupFailure({ setupFailed: false, settingsFailed: settingsFailed.current }));

  const runSetup = async (tryInstall: boolean) => {
    // Whether `setup()` got as far as answering. Everything after it is the app's own work, and
    // a failure there is not a verdict on the machine's drivers.
    let setupAnswered = false;
    try {
      let success;
      if (!loaded) {
        success = await commands.setup();
        setLoaded(success);
      } else {
        success = loaded;
      }
      setupAnswered = true;
      if (success) {
        // A retry must not inherit the previous attempt's verdict about the settings.
        settingsFailed.current = false;
        const existing = await loadSavedSettings();
        const savedPorts = existing.serverPorts;
        await replaySavedSettings(existing);
        if (settingsFailed.current) {
          toast({
            variant: "destructive",
            title: t("toasts.savedSettings.replayFailedTitle"),
            description: t("toasts.savedSettings.replayFailedDescription"),
          });
        }
        const publicSessionsEnabled = existing.publicSessionsEnabled !== false;
        setPublicSessionsEnabled(publicSessionsEnabled);

        await commands.watchForNetworkChanges();
        const newSessionId = Array.from(crypto.getRandomValues(new Uint8Array(12)), b => '23456789ABCDEFGHJKLMNPQRSTUVWXYZ'[b % 32]).join('');
        const newOtp = generateOtp();
        setSessionId(newSessionId);
        setOtp(newOtp);
        await commands.setSessionCredentials(newSessionId, newOtp);
        if (publicSessionsEnabled) {
          commands.registerCloudSession(newSessionId).catch((e: unknown) => {
            console.error("registerCloudSession failed", e);
            toast({
              variant: "destructive",
              title: t("toasts.cloudSession.failureTitle"),
              description: t("toasts.cloudSession.failureDescription"),
            });
          });
        } else {
          commands.unregisterCloudSession().catch((e: unknown) => { console.error("unregisterCloudSession failed", e); });
        }
        // An empty QR list reads on the dashboard as "no networks were found". When the adapter
        // read is what failed, that is the wrong thing to tell someone whose host is serving
        // fine -- so the reason is named here rather than being rendered as an absence.
        const qr = await readQrValues(newSessionId, savedPorts?.http);
        if (qr.adaptersUnavailable) {
          toast({
            variant: "destructive",
            title: t("toasts.networkAdapters.failureTitle"),
            description: t("toasts.networkAdapters.failureDescription"),
          });
        }
        setQrValues(qr.entries);
        setHostedNetworkOn(false);
        const update = await pendingUpdate();
        if (update) {
          await installUpdate(update);
          return;
        }
        // Was `document.getElementById("dashlink")!.click()`. When the element was not
        // in the document at that moment the TypeError landed in the catch below and the
        // user got the driver-install dialog for what was a routing failure -- and its
        // only action retried the same thing.
        void navigate("/dashboard");
        scheduleConfigEditorPreload();
        void loadDevices();
        void loadSettings();
      } else {
        // `setup()` failing is the last place an install can still help, and the only one where
        // the budget is consulted rather than the gate's own classification -- there is no
        // report here to say what went wrong. Without the budget this was a second elevation
        // prompt on top of whatever the gate had already spent.
        if (tryInstall && (await spendRepair("install"))) {
          await commands.installDrivers();
          await new Promise(resolve => setTimeout(resolve, 5000));
          await runSetup(false);
        } else {
          setFailure("setup");
        }
      }
    } catch (e) {
      // `setup()` itself is awaited above and its own failure is `setup`; anything else in here
      // is either the settings replay (which records itself and does not throw) or the bootstrap
      // going wrong on its own. The dialog now says which, instead of naming drivers for all
      // three -- the throw the owner hit was `object is not iterable` out of a malformed
      // `config.json`, on a machine whose drivers were fine.
      console.error("setup failed", e);
      setFailure(classifySetupFailure({
        setupFailed: !setupAnswered,
        settingsFailed: settingsFailed.current,
      }));
    }
  };

  // `check()` is an HTTP round-trip to the release endpoint, and on an offline or
  // captive-portal network it takes the full timeout. It used to gate everything: no
  // permission probe, no compatibility report, no `setup()` and so no virtual display
  // until it settled. It is kicked off here instead and only awaited at the one point
  // where an update would take the screen over, so it overlaps the whole startup path.
  // The deadline bounds the pathological case; a check that misses it is picked up on
  // the next launch.
  const updateCheck = useRef<Promise<Update | null> | null>(null);

  const pendingUpdate = async (): Promise<Update | null> => {
    const check = updateCheck.current;
    if (!check) return null;
    let timer: number | undefined;
    const deadline = new Promise<null>(resolve => {
      timer = window.setTimeout(() => resolve(null), UPDATE_CHECK_DEADLINE_MS);
    });
    try {
      return await Promise.race([check, deadline]);
    } finally {
      if (timer !== undefined) window.clearTimeout(timer);
    }
  };

  const installUpdate = async (update: Update): Promise<void> => {
    setUpdateVersion(update.version);
    setUpdating(true);
    try {
      let received = 0;
      await update.downloadAndInstall(event => {
        switch (event.event) {
          case "Started":
            setTotal(event.data.contentLength ?? 0);
            break;
          case "Progress":
            received += event.data.chunkLength;
            setDownloaded(received);
            break;
          case "Finished":
            break;
        }
      });
      await relaunch();
    } catch {
      setUpdating(false);
      setUpdateFailed(true);
    }
  };

  // A check that *failed* is not the same as "nothing was missing". Both used to return the
  // same `false`, so an unreadable probe was reported to the user as all-granted. Startup still
  // proceeds on `unreadable` -- refusing to start over a probe we could not run would be worse,
  // and capture reports its own failure -- but the user is told which of the two happened.
  const ensurePermissions = async (): Promise<"ok" | "blocked" | "unreadable"> => {
    let perms: PermissionStatus[];
    try {
      perms = await commands.checkPermissions();
    } catch (e) {
      console.error("the permission check failed; starting without knowing what was granted", e);
      toast({
        variant: "destructive",
        title: t("toasts.permissions.checkUnreadableTitle"),
        description: t("toasts.permissions.checkUnreadableDescription"),
      });
      return "unreadable";
    }
    const missing = perms.filter(blocks);
    if (missing.length === 0) return "ok";
    for (const p of missing) {
      try {
        await commands.requestPermission(p.key);
      } catch (e) {
        console.error(`requesting the ${p.key} permission failed`, e);
      }
    }
    try {
      setPermReport(await commands.checkPermissions());
    } catch {
      setPermReport(perms);
    }
    return "blocked";
  };

  const recheckPermissions = async () => {
    let perms: PermissionStatus[];
    try {
      perms = await commands.checkPermissions();
    } catch {
      toast({
        variant: "destructive",
        title: t("toasts.permissions.checkFailedTitle"),
        description: t("toasts.permissions.checkFailedDescription"),
      });
      return;
    }
    setPermReport(perms);
    if (!perms.some(blocks)) {
      setPermReport(null);
      proceed().catch(() => failStartup());
    }
  };

  // The boot the repair budget is scoped to, from the last report. Empty until one arrives, and
  // empty on the platforms that do not report one -- `repairForBoot` treats both as "this boot".
  const bootId = useRef("");

  // Take one attempt out of the budget, or refuse. Persisted before the attempt runs, not
  // after: a relaunch never comes back to write it down, and an install the user cancels at the
  // UAC prompt has still been offered once.
  const spendRepair = async (what: RepairAttempt): Promise<boolean> => {
    let repair: DriverRepair;
    try {
      repair = repairForBoot((await getConfig())?.driverRepair, bootId.current);
    } catch (e) {
      // The store is unreadable. Refuse: an unbounded loop of elevation prompts is the failure
      // this budget exists to prevent, and every route out of the modal still works.
      console.error("[bootstrap] the driver repair record is unreadable; not repairing", e);
      return false;
    }
    if (!mayAttempt(repair, what)) {
      console.log(`[bootstrap] the display driver has already been given one ${what} this boot`);
      return false;
    }
    try {
      await updateConfig({ driverRepair: recordAttempt(repair, what) });
    } catch (e) {
      console.error("[bootstrap] the driver repair record could not be written", e);
      return false;
    }
    return true;
  };

  // Install what can be installed, then say whether the caller should carry on deciding.
  // `false` means this launch is over -- the only route to it is a relaunch that has already
  // been started, which is why the caller must not fall through to the dashboard behind it.
  const remediate = async (rows: UnsupportedApi[]): Promise<boolean> => {
    const action = driverRemediation(rows);
    // An installable row nothing here knows how to install. Carry on and decide: it survives
    // the pass, so the user is shown it rather than left on a spinner forever.
    if (!action) return true;
    // One per boot, and the modal that follows says to restart. Without this the same pass ran
    // again on the next launch, and again after the relaunch it started, each time asking for
    // elevation twice -- against a driver that was never the problem.
    if (!(await spendRepair(action))) return true;
    setRemediating(true);
    try {
      if (action === "reinstall") {
        // Installed but not answering. Installing over it changes nothing -- the driver is
        // already there -- so it comes out first. A removal that fails is not fatal: the
        // install below is still the flow that used to run on its own.
        if (!(await commands.removeDrivers())) {
          console.log("[bootstrap] removing the unresponsive display driver did not complete");
        }
      }
      const installed = await commands.installDrivers();
      // The driver enumerates asynchronously; the probe below reads the same thing the
      // installer just changed, so it has to be given the moment the old flow gave it.
      await new Promise(resolve => setTimeout(resolve, 5000));
      if (!installed && action === "reinstall" && !relaunchAlreadyTried() && (await spendRepair("relaunch"))) {
        // A driver that is present but silent is often one this process opened before the
        // reinstall. A fresh process is the cheapest thing left to try, and it is bounded
        // twice over so it cannot become a relaunch loop.
        rememberRelaunch();
        console.log("[bootstrap] relaunching so the reinstalled display driver is opened fresh");
        await commands.relaunchApp();
        return false;
      }
      return true;
    } catch (e) {
      console.error("installing the display driver failed", e);
      toast({
        variant: "destructive",
        title: t("toasts.driverInstall.failureTitle"),
        description: t("toasts.driverInstall.failureDescription"),
      });
      return true;
    } finally {
      setRemediating(false);
    }
  };

  // Ask the instance holding the driver to quit, then probe again. `quitDriverHolder` only ever
  // reaches one that answers on the control socket, so a `false` means "close it yourself".
  const quitHolderAndRetry = async () => {
    let quit = false;
    try {
      quit = await commands.quitDriverHolder();
    } catch (e) {
      console.error("asking the other instance to quit failed", e);
    }
    if (!quit) {
      toast({
        variant: "destructive",
        title: t("toasts.driverBusy.quitFailedTitle"),
        description: t("toasts.driverBusy.quitFailedDescription"),
      });
      return;
    }
    setBusyHolder(undefined);
    await new Promise(resolve => setTimeout(resolve, HOLDER_QUIT_SETTLE_MS));
    await proceed();
  };

  const proceed = async (remediated = false) => {
    try {
      if ((await ensurePermissions()) === "blocked") return;
      let report: CompatibilityReport;
      try {
        report = await commands.checkSystemRequirements();
      } catch (e) {
        // The probe itself failed, which says nothing about the machine. Start; `setup()` is
        // the real test and reports its own failure.
        console.error("the compatibility probe failed; starting anyway", e);
        await runSetup(true);
        return;
      }
      setLastReport(report);
      bootId.current = report.boot_id;
      const dontShowAgain = (await getConfig())?.dontShowAgain?.compatibility === true;
      const gate = classifyCompat(report, { remediated, dontShowAgain });
      switch (gate.kind) {
        case "busy":
          setBusyHolder(report.display_driver_holder_pid);
          return;
        case "remediate":
          if (await remediate(gate.rows)) {
            await proceed(true);
          }
          return;
        case "blocked":
        case "warn":
          setCompatReport({ ...report, unsupported_apis: gate.rows });
          setCompatBlocking(gate.kind === "blocked");
          return;
        case "ok":
          await runSetup(true);
          return;
      }
    } catch (e) {
      console.error("startup failed", e);
      failStartup();
    }
  };

  const start = async () => {
    updateCheck.current = check().catch(e => {
      // An updater failure -- network, signature, malformed latest.json -- used to be
      // indistinguishable from "no update available".
      console.error("the update check failed", e);
      return null;
    });
    // The dashboard is the next screen either way; fetching its chunk alongside the Rust
    // setup keeps it off the entry chunk without making the navigation wait for it.
    void loadDashboard();
    await proceed();
  };

  useEffect(() => {
    if (running.current) return;
    running.current = true;
    start().catch(() => failStartup());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const permissionGateOpen = permReport !== null;
  useEffect(() => {
    if (!permissionGateOpen) return;
    let stopped = false;
    let inFlight = false;
    const tick = () => {
      if (stopped || inFlight) return;
      inFlight = true;
      commands.checkPermissions()
        .then(perms => {
          if (stopped) return;
          setPermReport(prev => samePermissions(prev, perms) ? prev : perms);
          if (!perms.some(blocks)) {
            stopped = true;
            setPermReport(null);
            proceed().catch(() => failStartup());
          }
        })
        .catch(() => { /* transient; the next tick tries again */ })
        .finally(() => { inFlight = false; });
    };
    const opened = Date.now();
    let id: number | undefined;
    const schedule = () => {
      if (stopped) return;
      const elapsed = Date.now() - opened;
      const step = PERMISSION_POLL_STEPS.find(s => elapsed < s.until) ?? PERMISSION_POLL_STEPS[PERMISSION_POLL_STEPS.length - 1];
      id = window.setTimeout(() => {
        tick();
        schedule();
      }, step.every);
    };
    schedule();
    window.addEventListener("focus", tick);
    return () => {
      stopped = true;
      if (id !== undefined) window.clearTimeout(id);
      window.removeEventListener("focus", tick);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [permissionGateOpen]);

  const updatePct = total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : null;

  return (
    <div className="h-screen w-full flex flex-col items-center justify-center">
      <AlertDialog open={updating}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("bootstrap.update.title")}</AlertDialogTitle>
            <AlertDialogDescription>
              {updateVersion && (
                <>{withBold(t("bootstrap.update.versionNote"), `v${updateVersion}`)} </>
              )}
              {t("bootstrap.update.restartNote")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <div className="space-y-2">
            <div className="h-2 w-full overflow-hidden rounded-full bg-secondary">
              <div
                className={updatePct === null
                  ? "h-full w-0 rounded-full bg-blue-600 animate-pulse"
                  : "h-full rounded-full bg-blue-600 transition-all duration-200"}
                style={updatePct === null ? undefined : { width: `${updatePct}%` }}
              />
            </div>
            <p className="text-center text-sm text-muted-foreground">
              {updatePct === null
                ? (downloaded > 0 ? t("bootstrap.update.downloaded", { size: formatBytes(downloaded) }) : t("bootstrap.update.preparing"))
                : t("bootstrap.update.progress", { pct: updatePct, downloaded: formatBytes(downloaded), total: formatBytes(total) })}
            </p>
          </div>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={updateFailed}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("bootstrap.updateFailed.title")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("bootstrap.updateFailed.bodyBeforeLink")}
              <a
                href={DOWNLOAD_URL}
                onClick={e => {
                  e.preventDefault();
                  openUrl(DOWNLOAD_URL).catch(() => {
                    toast({
                      variant: "destructive",
                      title: t("toasts.openLink.failureTitle"),
                      description: t("toasts.openLink.failureDescription"),
                    });
                  });
                }}
                style={{ textDecoration: "underline" }}
              >
                {t("bootstrap.updateFailed.link")}
              </a>
              {t("bootstrap.updateFailed.bodyAfterLink")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogAction
              className="bg-secondary hover:bg-secondary/80 text-secondary-foreground"
              onClick={() => {
                commands.exitApp().catch(() => {
                  toast({
                    variant: "destructive",
                    title: t("toasts.exitApp.failureTitle"),
                    description: t("toasts.exitApp.failureDescription"),
                  });
                });
              }}
            >
              {t("common.quit")}
            </AlertDialogAction>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={async () => {
                setUpdateFailed(false);
                try {
                  await proceed();
                } catch {
                  failStartup();
                }
              }}
            >
              {t("common.continue")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <Loader2 className="animate-spin mb-4" size={48} />
      <p className="text-xl font-semibold">
        {remediating ? t("bootstrap.installingDriver") : t("bootstrap.starting")}
      </p>
      <AlertDialog open={failure !== null}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("bootstrap.setupError.title")}</AlertDialogTitle>
            {/* Same dialog, same layout, same two buttons -- only the sentence that names the
                cause moves. Blaming the display driver for a malformed `config.json` is what
                sent the owner reinstalling a driver that was working. */}
            <AlertDialogDescription>
              {t(setupErrorBodyKey(failure ?? "setup"))}
              {offersDriverInstallFor(failure ?? "setup", lastReport) && <b>{t("bootstrap.setupError.installPrompt")}</b>}
              {t("bootstrap.setupError.bodyAfterPrompt")}<a href={`mailto:${t("common.supportEmail")}`} target="_blank" style={{ textDecoration: "underline" }}>{t("common.supportEmail")}</a>{t("bootstrap.setupError.contactSuffix")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogAction
              className="bg-secondary hover:bg-secondary/80 text-secondary-foreground"
              onClick={() => {
                commands.exitApp().catch(() => {
                  toast({
                    variant: "destructive",
                    title: t("toasts.exitApp.failureTitle"),
                    description: t("toasts.exitApp.failureDescription"),
                  });
                });
              }}
            >
              {t("common.quit")}
            </AlertDialogAction>
            {/* Shown only where there is a driver to install. It used to be shown on macOS
                too, where `install_drivers` returns `true` without doing anything, so the
                copy told the user to press a button that could not help. */}
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
              onClick={async () => {
                setLoading(true);
                try {
                  if (offersDriverInstallFor(failure ?? "setup", lastReport)) {
                    await commands.installDrivers();
                    await new Promise(resolve => setTimeout(resolve, 5000));
                  }
                } catch (e) {
                  console.error("driver install failed", e);
                  toast({
                    variant: "destructive",
                    title: t("toasts.driverInstall.failureTitle"),
                    description: t("toasts.driverInstall.failureDescription"),
                  });
                  return;
                } finally {
                  setLoading(false);
                }
                setFailure(null);
                await runSetup(false);
              }}
              disabled={loading}
            >
              {offersDriverInstallFor(failure ?? "setup", lastReport)
                ? t("bootstrap.setupError.install")
                : t("bootstrap.setupError.retry")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      {/* Another ScreenExtend has the display driver. Nothing is broken, so there is no install
          button and no "Can't Start on This Computer" -- just the two things that help. */}
      <AlertDialog open={busyHolder !== undefined}>
        <AlertDialogContent className="max-w-2xl">
          <AlertDialogHeader>
            <AlertDialogTitle>{t("bootstrap.driverBusy.title")}</AlertDialogTitle>
            <AlertDialogDescription asChild>
              <div className="space-y-3 text-left">
                <div>
                  {busyHolder === null || busyHolder === undefined
                    ? t("bootstrap.driverBusy.body")
                    : withBold(t("bootstrap.driverBusy.bodyWithPid"), String(busyHolder))}
                </div>
                <div>{t("bootstrap.driverBusy.hint")}</div>
              </div>
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogAction
              className="bg-secondary hover:bg-secondary/80 text-secondary-foreground"
              onClick={() => {
                commands.exitApp().catch(() => {
                  toast({
                    variant: "destructive",
                    title: t("toasts.exitApp.failureTitle"),
                    description: t("toasts.exitApp.failureDescription"),
                  });
                });
              }}
            >
              {t("common.quit")}
            </AlertDialogAction>
            <button
              className="inline-flex h-10 items-center justify-center rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground hover:bg-secondary/80"
              onClick={() => { void quitHolderAndRetry(); }}
            >
              {t("bootstrap.driverBusy.quitOther")}
            </button>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={async () => {
                setBusyHolder(undefined);
                try {
                  await proceed();
                } catch {
                  failStartup();
                }
              }}
            >
              {t("common.retry")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={compatReport !== null}>
        <AlertDialogContent className="max-w-2xl">
          <AlertDialogHeader>
            {/* Blocking no longer only means "this OS is too old". It also means the app tried
                to install what was missing and the machine still cannot create a display or
                send video, which is not a verdict on the operating system. */}
            <AlertDialogTitle>
              {!compatBlocking
                ? t("bootstrap.compatibility.limitedTitle")
                : compatReport?.os_supported
                  ? t("bootstrap.compatibility.cannotStartTitle")
                  : t("bootstrap.compatibility.blockingTitle")}
            </AlertDialogTitle>
            <AlertDialogDescription asChild>
              <div className="space-y-3 text-left">
                <div>
                  {t("bootstrap.compatibility.detectedLabel")} <b>{compatReport?.os_version}</b>
                  <br />
                  {t("bootstrap.compatibility.minimumLabel")} <b>{compatReport?.min_os_version}</b>
                </div>
                {compatReport && compatReport.unsupported_apis.length > 0 && (
                  <div>
                    {!compatBlocking
                      ? t("bootstrap.compatibility.limitedIntro")
                      : compatReport.os_supported
                        ? t("bootstrap.compatibility.cannotStartIntro")
                        : t("bootstrap.compatibility.blockingIntro")}
                    <ul className="list-disc pl-5 mt-2 space-y-1">
                      {compatReport.unsupported_apis.map(api => (
                        <li key={api.name}>
                          <b>{api.name}</b> — {api.description} {t("bootstrap.compatibility.apiRequires", { version: api.required_version })}
                        </li>
                      ))}
                    </ul>
                  </div>
                )}
                {/* A driver row that survived its own repair. The app has spent its one
                    install and its one relaunch for this boot and will not ask for elevation
                    again, so the two things left are Try Again and a restart -- and the user
                    has to be told that, or the modal reads as a dead end. */}
                {compatBlocking && offersDriverInstall(compatReport) && (
                  <div>{t("bootstrap.compatibility.driverRestartHint")}</div>
                )}
                <div>
                  {t("bootstrap.compatibility.upgradeBefore")}
                  <a href={`mailto:${t("common.supportEmail")}`} target="_blank" style={{ textDecoration: "underline" }}>
                    {t("common.supportEmail")}
                  </a>{t("bootstrap.compatibility.upgradeSuffix")}
                </div>
              </div>
            </AlertDialogDescription>
          </AlertDialogHeader>
          {!compatBlocking && (
            <div className="flex items-center space-x-2 mb-4">
              <Checkbox
                id="compatDontShowAgain"
                checked={compatDontShowAgain}
                onCheckedChange={checked => setCompatDontShowAgain(checked === true)}
              />
              <label
                htmlFor="compatDontShowAgain"
                className="text-sm text-muted-foreground cursor-pointer"
              >
                {t("common.dontShowAgain")}
              </label>
            </div>
          )}
          <AlertDialogFooter>
            {/* The one action a blocked driver row still has. It re-probes, and the repair
                budget guarantees it cannot turn into another elevation prompt -- so it is safe
                to offer, which the old modal's single Exit button was not. */}
            {compatBlocking && offersDriverInstall(compatReport) && (
              <button
                className="inline-flex h-10 items-center justify-center rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground hover:bg-secondary/80"
                onClick={() => {
                  setCompatReport(null);
                  proceed().catch(() => failStartup());
                }}
              >
                {t("common.retry")}
              </button>
            )}
            {!compatBlocking && (
              <AlertDialogAction
                className="bg-blue-600 hover:bg-blue-700 text-white"
                onClick={async () => {
                  setCompatReport(null);
                  try {
                    await runSetup(true);
                  } catch {
                    failStartup();
                    return;
                  }
                  try {
                    const current = await getConfig();
                    if (current) {
                      await updateConfig({dontShowAgain: {...current.dontShowAgain, compatibility: compatDontShowAgain}});
                    }
                  } catch (e) {
                    console.error("saving compatibility preference failed", e);
                    toast({
                      variant: "destructive",
                      title: t("toasts.compatibility.saveFailedTitle"),
                      description: t("toasts.compatibility.saveFailedDescription"),
                    });
                  }
                }}
              >
                {t("common.continue")}
              </AlertDialogAction>
            )}
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={() => {
                commands.exitApp().catch(() => {
                  toast({
                    variant: "destructive",
                    title: t("toasts.exitApp.failureTitle"),
                    description: t("toasts.exitApp.failureDescription"),
                  });
                });
              }}
            >
              {t("common.exit")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={permReport !== null}>
        <AlertDialogContent className="max-w-2xl">
          <AlertDialogHeader>
            <AlertDialogTitle>{t("bootstrap.permissions.title")}</AlertDialogTitle>
            <AlertDialogDescription asChild>
              <div className="space-y-3 text-left">
                <div>
                  {t("bootstrap.permissions.body")}
                </div>
                <div className="flex items-start space-x-2 rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
                  <Info className="mt-0.5 shrink-0 text-amber-600" size={16} />
                  <div>
                    <span className="font-semibold">
                      {needsRelaunch(permReport)
                        ? t("bootstrap.permissions.relaunchTitle")
                        : t("bootstrap.permissions.tipTitle")}
                    </span>{" "}
                    {needsRelaunch(permReport)
                      ? t("bootstrap.permissions.relaunchBody")
                      : t("bootstrap.permissions.tipBody")}
                  </div>
                </div>
                <ul className="space-y-2">
                  {permReport?.map(p => (
                    <li
                      key={p.key}
                      className="flex items-start justify-between space-x-3 rounded-md border p-3"
                    >
                      <div>
                        <div className="font-semibold flex items-center space-x-2">
                          {p.state === "granted"
                            ? <Check className="text-green-600 mr-2" size={16} />
                            : p.state === "denied"
                              ? <X className="text-red-600 mr-2" size={16} />
                              : <Info className="text-amber-600 mr-2" size={16} />}
                          {p.name}
                        </div>
                        <div className="text-sm text-muted-foreground">{p.detail || p.description}</div>
                      </div>
                      {p.state !== "granted" && (
                        <button
                          className="shrink-0 rounded-md bg-secondary px-3 py-1.5 text-sm hover:bg-secondary/80"
                          onClick={() => {
                            commands.openPermissionSettings(p.key)
                              .catch((e: unknown) => { console.error("openPermissionSettings failed", e); });
                          }}
                        >
                          Open Settings
                        </button>
                      )}
                    </li>
                  ))}
                </ul>
              </div>
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <div className="mr-auto flex items-center text-sm text-muted-foreground">
              <Loader2 className="animate-spin mr-2" size={14} />
              {t("bootstrap.permissions.autoChecking")}
            </div>
            <AlertDialogAction
              className="bg-secondary hover:bg-secondary/80 text-secondary-foreground"
              onClick={() => {
                commands.exitApp().catch(() => {
                  toast({
                    variant: "destructive",
                    title: t("toasts.exitApp.failureTitle"),
                    description: t("toasts.exitApp.failureDescription"),
                  });
                });
              }}
            >
              {t("common.quit")}
            </AlertDialogAction>
            <button
              className="inline-flex h-10 items-center justify-center rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground hover:bg-secondary/80"
              onClick={() => void recheckPermissions()}
            >
              {t("bootstrap.permissions.recheck")}
            </button>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={() => {
                // Goes through LaunchServices on macOS so the new process is its
                // own TCC subject rather than an heir of this dying one.
                commands.relaunchApp().catch(() => {
                  toast({
                    variant: "destructive",
                    title: t("toasts.relaunch.failureTitle"),
                    description: t("toasts.relaunch.failureDescription"),
                  });
                });
              }}
            >
              {t("bootstrap.permissions.relaunch")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
