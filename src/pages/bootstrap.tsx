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
  completeFirstRun,
  createConfig,
  defaultConfig,
  getConfig,
  updateConfig,
  DEFAULT_ADAPTIVE_QUALITY,
  DEFAULT_AUTO_FRAME_RATE,
  DEFAULT_AUTO_ORIENTATION,
  DEFAULT_NATIVE_DPR,
  DEFAULT_POINTER_SCOPE,
  type Config,
} from "@/components/config-provider";
import { useGlobalSetters, useLoaded } from "@/components/global-provider";
import {
  commands,
  type CompatibilityReport,
  type DisplayDriverHealth,
  type DriverRestartOutcome,
  type PermissionStatus,
  type UnsupportedApi,
} from "@/lib/bindings";
import { generateOtp, readQrValues } from "@/lib/utils";
import {
  classifyCompat,
  classifyDriverHealth,
  restartFailureKey,
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
import { savedPlacements } from "@/lib/arrange";
import { scheduleConfigEditorPreload } from "@/lib/preload-editor";
import { loadDashboard, loadDevices, loadSettings } from "@/lib/route-chunks";
import { useTranslation } from "@/i18n";
import { useTheme, type Theme } from "@/components/theme-provider";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { open as openUrl } from "@tauri-apps/plugin-shell";

const DOWNLOAD_URL = "https://screenextend.app";
// the gate stays open for minutes while the user is in System Settings, so the poll backs
// off (about 26 TCC reads in the first minute against 300 at a flat 200 ms). The `focus`
// listener re-checks at once on return; the interval covers an app that never lost focus.
const PERMISSION_POLL_STEPS = [
  { until: 2000, every: 200 },
  { until: 10000, every: 1000 },
  { until: Infinity, every: 3000 },
];
const UPDATE_CHECK_DEADLINE_MS = 2000;

// marks a user-pressed Retry on the key `request_permission` takes. The host lets the first
// request for a permission raise the macOS prompt, makes every later automatic one a silent
// read, and asks macOS again only for a Retry.
// Keep in step with `RETRY_PREFIX` in `src-tauri/src/macos_utils/permissions.rs`.
const PERMISSION_RETRY_PREFIX = "retry:";

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
// "Go on regardless" on the permission gate skips Accessibility only, and only until the app
// quits, so the next launch asks again
let accessibilitySkipped = false;

function blocks(p: PermissionStatus): boolean {
  if (p.key === "accessibility" && accessibilitySkipped) return false;
  return p.required && p.state === "denied";
}

// second guard against a relaunch loop, beside `driverRepair` in the config store: the store
// is written asynchronously and a relaunch can beat the write to disk
const RELAUNCH_MEMO_KEY = "se.driverRelaunchAt";
const RELAUNCH_WINDOW_MS = 10 * 60 * 1000;

function relaunchAlreadyTried(): boolean {
  try {
    const at = Number(window.localStorage.getItem(RELAUNCH_MEMO_KEY) ?? "");
    return Number.isFinite(at) && Date.now() - at < RELAUNCH_WINDOW_MS;
  } catch {
    // storage unavailable reads as "already tried": a relaunch loop is worse than no relaunch
    return true;
  }
}

function rememberRelaunch(): void {
  try {
    window.localStorage.setItem(RELAUNCH_MEMO_KEY, String(Date.now()));
  } catch {
    // see relaunchAlreadyTried
  }
}

// wait after asking another instance to quit: it broadcasts BYE and removes its virtual
// displays on the way out, and the driver is ours to open only after that
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

  // why startup stopped, or null while it has not
  const [failure, setFailure] = useState<SetupFailure | null>(null);
  const [loading, setLoading] = useState(false);
  const [compatReport, setCompatReport] = useState<CompatibilityReport | null>(null);
  const [compatBlocking, setCompatBlocking] = useState(false);
  // last probe report, so the setup-error dialog can ask whether this host has a driver to install
  const [lastReport, setLastReport] = useState<CompatibilityReport | null>(null);
  const [remediating, setRemediating] = useState(false);
  // another ScreenExtend process holds the display driver: its pid (null when unknown), or
  // undefined while the dialog is closed
  const [busyHolder, setBusyHolder] = useState<number | null | undefined>(undefined);
  const [compatDontShowAgain, setCompatDontShowAgain] = useState(true);
  const [permReport, setPermReport] = useState<PermissionStatus[] | null>(null);
  const [updating, setUpdating] = useState(false);
  const [updateFailed, setUpdateFailed] = useState(false);
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);
  const [downloaded, setDownloaded] = useState(0);
  const [total, setTotal] = useState(0);
  const running = useRef(false);
  // the driver-memory gate (`classifyDriverHealth`): null while closed, otherwise the modal's
  // phase. `outcome` is the last failed attempt's answer.
  const [driverRestart, setDriverRestart] = useState<{
    phase: "prompt" | "restarting" | "failed";
    commitMb: number;
    outcome: DriverRestartOutcome | null;
  } | null>(null);
  // set once the gate has passed or a restart is verified, so a retry of `setup()` does not
  // ask again; and the argument `runSetup` had when the gate stopped it, to resume with
  const driverHealthCleared = useRef(false);
  const pendingTryInstall = useRef(true);
  // set when anything in the saved-settings phase fails. Opens no dialog on its own (a
  // rejected setting is no reason to refuse to start) but lets a later failure name the cause.
  const settingsFailed = useRef(false);

  // Reads the saved configuration, creating one on the first run. Never throws: an unreadable
  // config is not allowed to stop the app (`startup-gate.ts`), so it starts on the defaults
  // and logs that it did.
  const loadSavedSettings = async (): Promise<Config> => {
    try {
      let existing = await getConfig();
      if (!existing) {
        await createConfig({ name: await commands.getUsername(), theme });
        existing = await getConfig();
      } else if (await completeFirstRun(() => commands.getUsername())) {
        // a store the driver repair record reached first: it had keys, so it was never set up
        console.log("[bootstrap] the saved configuration lacked its first-run values; created them");
        existing = await getConfig();
      }
      if (existing) return existing;
      console.error("[bootstrap] the config store is still empty after setup; using the defaults");
    } catch (e) {
      console.error("[bootstrap] the saved configuration could not be read; using the defaults", e);
    }
    settingsFailed.current = true;
    // a copy: `defaultConfig` is module-level and shared
    return { ...defaultConfig };
  };

  // Pushes the saved configuration into the host. Each step may fail alone: one rejected
  // setting must not cost the user the others, or the dashboard.
  const replaySavedSettings = async (existing: Config): Promise<void> => {
    const replay: Promise<unknown>[] = [];
    const step = (what: string, run: () => Promise<unknown>) => {
      let started: Promise<unknown>;
      try {
        started = run();
      } catch (e) {
        // a synchronous throw, e.g. reading a field off a stored value of an unexpected shape
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
    if (existing.disableLinkGovernor) {
      step("disableLinkGovernor", () => commands.setDisableLinkGovernor(true));
    }
    if (existing.legacyVolumeKeyProxy) {
      step("legacyVolumeKeyProxy", () => commands.setLegacyVolumeKeyProxy(true));
    }
    const link = existing.permanentLink;
    if (link?.enabled && link.sessionId && link.otp) {
      step("permanentLink", async () => {
        const result = await commands.setPermanentLink(link.sessionId, link.otp);
        if (result.status === "error") throw new Error(result.error);
      });
    }
    step("devices.placement", () => commands.setDevicePlacements(savedPlacements(existing.devices)));
    // independent per device, so replayed together: with a dozen saved devices, one IPC
    // round-trip at a time is the longest stretch between setup() and the dashboard
    for (const device of existing.devices) {
      // keyed by the device token: a row with none names only an address, which is
      // recycled between machines, so it waits for that device to join again
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
      step(`devices[${device.token}].adaptiveModes`, () =>
        commands.setDeviceAdaptiveModes(
          device.token,
          device.adaptiveQuality ?? DEFAULT_ADAPTIVE_QUALITY,
          device.nativeDpr ?? DEFAULT_NATIVE_DPR,
          device.autoOrientation ?? DEFAULT_AUTO_ORIENTATION
        ));
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

  // catch-all: `setup()` classifies its own failure, so what reaches here is the settings
  // phase (which recorded itself) or the bootstrap itself
  const failStartup = () =>
    setFailure(classifySetupFailure({ setupFailed: false, settingsFailed: settingsFailed.current }));

  // a failed read says nothing about the machine, so it lets startup through: `setup()` is
  // the real test
  const readDriverHealth = async (): Promise<DisplayDriverHealth | null> => {
    try {
      return await commands.displayDriverHealth();
    } catch (e) {
      console.error("[bootstrap] the display driver health read failed; starting without it", e);
      return null;
    }
  };

  // the driver-memory gate: after the compatibility gate has installed a missing driver,
  // before `setup()` creates anything a restart would take away
  const driverHealthBlocks = async (tryInstall: boolean): Promise<boolean> => {
    if (loaded || driverHealthCleared.current) return false;
    const gate = classifyDriverHealth(await readDriverHealth());
    if (gate.kind === "restart") {
      pendingTryInstall.current = tryInstall;
      setDriverRestart({ phase: "prompt", commitMb: gate.commitMb, outcome: null });
      return true;
    }
    driverHealthCleared.current = true;
    return false;
  };

  // "Restart Driver": one UAC prompt, then on a verified restart the setup the gate
  // interrupted resumes. A failure keeps the modal open, with the reason.
  const restartDriver = async () => {
    setDriverRestart(prev => prev && { ...prev, phase: "restarting", outcome: null });
    let outcome: DriverRestartOutcome | null = null;
    try {
      outcome = await commands.restartDisplayDriver();
    } catch (e) {
      console.error("[bootstrap] restarting the display driver failed", e);
    }
    if (outcome?.ok) {
      console.log(`[bootstrap] ${outcome.detail}`);
      driverHealthCleared.current = true;
      setDriverRestart(null);
      try {
        await runSetup(pendingTryInstall.current);
      } catch {
        failStartup();
      }
      return;
    }
    console.error(`[bootstrap] the display driver was not restarted: ${outcome?.reason ?? "no answer"} ${outcome?.detail ?? ""}`);
    setDriverRestart(prev => prev && { ...prev, phase: "failed", outcome });
  };

  const runSetup = async (tryInstall: boolean) => {
    if (await driverHealthBlocks(tryInstall)) return;
    // whether `setup()` answered: a failure after that is the app's own, not a verdict on
    // the machine's drivers
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
        // a retry must not inherit the previous attempt's verdict about the settings
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
        // an empty QR list reads as "no networks were found", which is wrong when the adapter
        // read is what failed: say so
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
        void navigate("/dashboard");
        scheduleConfigEditorPreload();
        void loadDevices();
        void loadSettings();
      } else {
        // the last place an install can still help. No report says what went wrong here, so
        // the repair budget decides: without it this is a second elevation prompt.
        if (tryInstall && (await spendRepair("install"))) {
          await commands.installDrivers();
          await new Promise(resolve => setTimeout(resolve, 5000));
          await runSetup(false);
        } else {
          setFailure("setup");
        }
      }
    } catch (e) {
      // a throw before `setup()` answered is `setup`; anything later is the settings replay
      // or the bootstrap itself, and the dialog must say which
      console.error("setup failed", e);
      setFailure(classifySetupFailure({
        setupFailed: !setupAnswered,
        settingsFailed: settingsFailed.current,
      }));
    }
  };

  // `check()` is an HTTP round-trip that takes its full timeout on an offline or
  // captive-portal network, so it runs alongside startup and is awaited only where an update
  // would take the screen over. A check that misses the deadline is picked up next launch.
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

  // a check that failed is `unreadable`, not `ok`. Startup still proceeds on it (capture
  // reports its own failure) but the user is told the probe could not run.
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
    // the automatic request: the host lets it reach the macOS prompt only the first time a
    // permission is ever requested (persisted across relaunches). After that it is a silent
    // read, and only Retry asks macOS again.
    // Screen Recording first: macOS shows one permission alert at a time, and the host holds
    // the Accessibility prompt until the Screen Recording alert is gone
    const ordered = [...missing].sort((a, b) =>
      Number(b.key === "screen_recording") - Number(a.key === "screen_recording"));
    for (const p of ordered) {
      try {
        await commands.requestPermission(p.key);
      } catch (e) {
        console.error(`requesting the ${p.key} permission failed`, e);
      }
    }
    // the gate is (re)opening: the next way out may start `proceed()` once more
    gateLeft.current = false;
    try {
      setPermReport(await commands.checkPermissions());
    } catch {
      setPermReport(perms);
    }
    return "blocked";
  };

  // the poll, Re-check and a granted Retry can all close the permission gate, and two landing
  // together must not run setup twice: `proceed()` starts once per opening
  const gateLeft = useRef(false);
  const leavePermissionGate = () => {
    if (gateLeft.current) return;
    gateLeft.current = true;
    setPermReport(null);
    proceed().catch(() => failStartup());
  };

  // the only path that may re-raise a macOS permission prompt. On a definite denial the host
  // clears the TCC entry first (macOS will not ask over an existing one) and says so in the
  // row's detail.
  const [retrying, setRetrying] = useState<string | null>(null);
  // state disables the button on the next render; the ref covers the gap before it
  const retryInFlight = useRef(false);
  const retryPermission = async (key: string) => {
    if (retryInFlight.current) return;
    retryInFlight.current = true;
    setRetrying(key);
    try {
      await commands.requestPermission(PERMISSION_RETRY_PREFIX + key);
    } catch (e) {
      console.error(`retrying the ${key} permission failed`, e);
    } finally {
      retryInFlight.current = false;
      setRetrying(null);
    }
    await recheckPermissions();
  };

  const [rechecking, setRechecking] = useState(false);
  const recheckInFlight = useRef(false);
  const recheckPermissions = async () => {
    if (recheckInFlight.current) return;
    recheckInFlight.current = true;
    setRechecking(true);
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
    } finally {
      recheckInFlight.current = false;
      setRechecking(false);
    }
    setPermReport(perms);
    if (!perms.some(blocks)) leavePermissionGate();
  };

  // the boot the repair budget is scoped to, from the last report. Empty until one arrives or
  // where the platform reports none: `repairForBoot` treats both as "this boot".
  const bootId = useRef("");

  // Takes one attempt out of the budget, or refuses. Persisted before the attempt runs: a
  // relaunch never comes back to write it down, and an install cancelled at the UAC prompt
  // has still been offered once.
  const spendRepair = async (what: RepairAttempt): Promise<boolean> => {
    let repair: DriverRepair;
    try {
      repair = repairForBoot((await getConfig())?.driverRepair, bootId.current);
    } catch (e) {
      // store unreadable: refuse, since a loop of elevation prompts is what the budget prevents
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

  // Installs what can be installed. `false` means a relaunch has been started and this launch
  // is over: the caller must not fall through to the dashboard behind it.
  const remediate = async (rows: UnsupportedApi[]): Promise<boolean> => {
    const action = driverRemediation(rows);
    // an installable row nothing here can install: carry on, so the user is shown it instead
    // of a spinner
    if (!action) return true;
    // one per boot: without the budget every launch and relaunch asks for elevation again
    if (!(await spendRepair(action))) return true;
    setRemediating(true);
    try {
      if (action === "reinstall") {
        // installed but not answering: installing over it changes nothing, so remove it first.
        // A failed removal is not fatal.
        if (!(await commands.removeDrivers())) {
          console.log("[bootstrap] removing the unresponsive display driver did not complete");
        }
      }
      const installed = await commands.installDrivers();
      // the driver enumerates asynchronously: give it time before the next probe reads it
      await new Promise(resolve => setTimeout(resolve, 5000));
      if (!installed && action === "reinstall" && !relaunchAlreadyTried() && (await spendRepair("relaunch"))) {
        // a present but silent driver is often one this process opened before the reinstall,
        // so try a fresh process. Bounded twice over, so it cannot become a relaunch loop.
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

  // `quitDriverHolder` only reaches an instance that answers on the control socket, so a
  // `false` means "close it yourself"
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
        // a failed probe says nothing about the machine: start, `setup()` is the real test
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
      // log it: an updater failure (network, signature, malformed latest.json) is not "no update"
      console.error("the update check failed", e);
      return null;
    });
    // the dashboard is the next screen either way: fetch its chunk alongside the Rust setup
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
            leavePermissionGate();
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
        {remediating
          ? t("bootstrap.installingDriver")
          : driverRestart?.phase === "restarting"
            ? t("bootstrap.driverRestart.restarting")
            : t("bootstrap.starting")}
      </p>
      {/* the driver leaked past its threshold. No "continue anyway": every join would time out */}
      <AlertDialog open={driverRestart !== null}>
        <AlertDialogContent className="max-w-2xl">
          <AlertDialogHeader>
            <AlertDialogTitle>{t("bootstrap.driverRestart.title")}</AlertDialogTitle>
            <AlertDialogDescription asChild>
              <div className="space-y-3 text-left">
                <div>{t("bootstrap.driverRestart.body")}</div>
                {driverRestart && driverRestart.commitMb > 0 && (
                  <div>
                    {withBold(
                      t("bootstrap.driverRestart.usage"),
                      formatBytes(driverRestart.commitMb * 1024 * 1024)
                    )}
                  </div>
                )}
                {driverRestart?.phase === "failed" && (
                  <div className="flex items-start space-x-2 rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
                    <Info className="mt-0.5 shrink-0 text-amber-600" size={16} />
                    <div>
                      {t(restartFailureKey(driverRestart.outcome))}
                      {driverRestart.outcome?.detail && (
                        <div className="mt-1 text-muted-foreground">{driverRestart.outcome.detail}</div>
                      )}
                    </div>
                  </div>
                )}
              </div>
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogAction
              className="bg-secondary hover:bg-secondary/80 text-secondary-foreground disabled:cursor-not-allowed disabled:opacity-50"
              disabled={driverRestart?.phase === "restarting"}
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
              {t("bootstrap.driverRestart.exit")}
            </AlertDialogAction>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
              disabled={driverRestart?.phase === "restarting"}
              onClick={() => { void restartDriver(); }}
            >
              {driverRestart?.phase === "restarting" && <Loader2 className="animate-spin mr-2" size={16} />}
              {t("bootstrap.driverRestart.restart")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={failure !== null}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("bootstrap.setupError.title")}</AlertDialogTitle>
            {/* only the sentence naming the cause varies: a bad config.json must not blame the driver */}
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
            {/* installs only where there is a driver to install (on macOS `install_drivers` does
                nothing); otherwise a plain retry */}
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
      {/* another ScreenExtend holds the display driver: nothing is broken, so no install button */}
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
            {/* blocking is not only "this OS is too old": also a machine that still cannot create a
                display or send video after the install was tried */}
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
                          <b>{api.name}</b> - {api.description} {t("bootstrap.compatibility.apiRequires", { version: api.required_version })}
                        </li>
                      ))}
                    </ul>
                  </div>
                )}
                {/* a driver row that survived its repair: the budget for this boot is spent, so
                    say that Try Again and a restart are what is left */}
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
            {/* re-probes only: the repair budget rules out another elevation prompt */}
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
                        <div className="flex shrink-0 items-center space-x-2">
                          <button
                            className="rounded-md bg-secondary px-3 py-1.5 text-sm hover:bg-secondary/80 disabled:opacity-50"
                            disabled={retrying !== null}
                            title={t("bootstrap.permissions.retryHint")}
                            onClick={() => void retryPermission(p.key)}
                          >
                            {retrying === p.key
                              ? <Loader2 className="animate-spin" size={14} />
                              : t("bootstrap.permissions.retry")}
                          </button>
                          {/* opens the pane only: a request from here would raise the macOS dialog
                              over the pane it just opened */}
                          <button
                            className="rounded-md bg-secondary px-3 py-1.5 text-sm hover:bg-secondary/80"
                            onClick={() => {
                              commands.openPermissionSettings(p.key)
                                .catch((e: unknown) => { console.error("openPermissionSettings failed", e); });
                            }}
                          >
                            {t("bootstrap.permissions.openSettings")}
                          </button>
                        </div>
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
            {(permReport ?? []).some(p => p.key === "accessibility" && blocks(p))
              && !(permReport ?? []).some(p => p.key !== "accessibility" && blocks(p)) && (
              <button
                className="inline-flex h-10 items-center justify-center rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground hover:bg-secondary/80"
                title={t("bootstrap.permissions.skipForNowHint")}
                onClick={() => {
                  accessibilitySkipped = true;
                  leavePermissionGate();
                }}
              >
                {t("bootstrap.permissions.skipForNow")}
              </button>
            )}
            <button
              className="inline-flex h-10 items-center justify-center rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground hover:bg-secondary/80 disabled:opacity-50"
              disabled={rechecking}
              onClick={() => void recheckPermissions()}
            >
              {t("bootstrap.permissions.recheck")}
            </button>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={() => {
                // through LaunchServices on macOS, so the new process is its own TCC subject,
                // not an heir of this dying one
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
