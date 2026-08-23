import { useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";

import { Check, Loader2, X } from "lucide-react";
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

import { createConfig, getConfig, updateConfig } from "@/components/config-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { commands, type CompatibilityReport, type PermissionStatus } from "@/lib/bindings";
import { buildQrValues, generateOtp } from "@/lib/utils";
import { useTranslation } from "@/i18n";
import { useTheme, type Theme } from "@/components/theme-provider";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { open as openUrl } from "@tauri-apps/plugin-shell";

const DOWNLOAD_URL = "https://screenextend.app";

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
  const { theme, setTheme } = useTheme();
  const { t } = useTranslation();
  const { windowLoaded: [loaded, setLoaded], windowOtp: [, setOtp], windowHostedNetworkOn: [, setHostedNetworkOn], windowSessionId: [, setSessionId], windowQrValues: [, setQrValues], windowPublicSessionsEnabled: [, setPublicSessionsEnabled] } = useContext(GlobalProviderContext);

  const [error, setError] = useState(false);
  const [loading, setLoading] = useState(false);
  const [compatReport, setCompatReport] = useState<CompatibilityReport | null>(null);
  const [compatBlocking, setCompatBlocking] = useState(false);
  const [compatDontShowAgain, setCompatDontShowAgain] = useState(true);
  const [permReport, setPermReport] = useState<PermissionStatus[] | null>(null);
  const [updating, setUpdating] = useState(false);
  const [updateFailed, setUpdateFailed] = useState(false);
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);
  const [downloaded, setDownloaded] = useState(0);
  const [total, setTotal] = useState(0);
  const running = useRef(false);

  const runSetup = async (tryInstall: boolean) => {
    let success;
    if (!loaded) {
      success = await commands.setup();
      setLoaded(success);
    } else {
      success = loaded;
    }
    if (success) {
      if (!(await getConfig())) {
        await createConfig({ name: await commands.getUsername(), theme });
      }
      const existing = (await getConfig())!;
      setTheme(existing.theme as Theme);
      const savedPorts = existing?.serverPorts;
      if (savedPorts) {
        await commands.setServerPorts(savedPorts.http, savedPorts.https);
      }
      if (existing?.disableGpuEncode) {
        await commands.setDisableGpuEncode(true);
      }
      for (const device of existing?.devices ?? []) {
        await commands.setDeviceOverride(
          device.ip,
          device.scale,
          device.orientation,
          device.refreshRate,
          device.videoScale,
          device.videoQuality,
          device.remoteControl ?? false,
          device.systemAudio ?? false
        );
      }
      for (const known of existing?.knownDevices ?? []) {
        const token = known.token ?? "";
        if (known.banned) {
          await commands.setDeviceBanned(token, known.ip, true);
        } else if (token) {
          await commands.setDeviceApproved(token, true);
        } else {
          console.log(`[migrate] known device ${known.ip} has no trust token; it will need to re-enter the code once`);
        }
      }
      const publicSessionsEnabled = existing?.publicSessionsEnabled !== false;
      setPublicSessionsEnabled(publicSessionsEnabled);

      await commands.watchForNetworkChanges();
      const newSessionId = Array.from(crypto.getRandomValues(new Uint8Array(12)), b => '23456789ABCDEFGHJKLMNPQRSTUVWXYZ'[b % 32]).join('');
      const newOtp = generateOtp();
      setSessionId(newSessionId);
      setOtp(newOtp);
      await commands.setSessionCredentials(newSessionId, newOtp);
      if (publicSessionsEnabled) {
        void commands.registerCloudSession(newSessionId);
      } else {
        void commands.unregisterCloudSession();
      }
      setQrValues(await buildQrValues(newSessionId, savedPorts?.http));
      setHostedNetworkOn(false);
      const turn = (await getConfig())?.turnConfig;
      if (turn?.urls) {
        await commands.setTurnConfig(turn.urls, turn.username ?? "", turn.credential ?? "");
      }
      document.getElementById("dashlink")!.click();
    } else {
      if (tryInstall) {
        await commands.installDrivers();
        await new Promise(resolve => setTimeout(resolve, 5000));
        runSetup(false);
      } else {
        setError(true);
      }
    }
  };

  const runUpdate = async (): Promise<boolean> => {
    let update;
    try {
      update = await check();
    } catch {
      return false;
    }
    if (!update) return false;
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
      return true;
    } catch {
      setUpdating(false);
      setUpdateFailed(true);
      return true;
    }
  };

  const ensurePermissions = async (): Promise<boolean> => {
    let perms: PermissionStatus[];
    try {
      perms = await commands.checkPermissions();
    } catch {
      return false;
    }
    const missing = perms.filter(p => p.required && !p.granted);
    if (missing.length === 0) return false;
    for (const p of missing) {
      try { await commands.requestPermission(p.key); } catch { /* ignore */ }
    }
    try {
      setPermReport(await commands.checkPermissions());
    } catch {
      setPermReport(perms);
    }
    return true;
  };

  const recheckPermissions = async () => {
    let perms: PermissionStatus[];
    try {
      perms = await commands.checkPermissions();
    } catch {
      return;
    }
    setPermReport(perms);
    if (perms.every(p => !p.required || p.granted)) {
      setPermReport(null);
      void proceed();
    }
  };

  const proceed = async () => {
    if (await ensurePermissions()) return;
    let report: CompatibilityReport;
    try {
      report = await commands.checkSystemRequirements();
    } catch {
      await runSetup(true);
      return;
    }
    const hasBlocking =
      !report.os_supported ||
      report.unsupported_apis.some(api => api.severity === "blocking");
    if (hasBlocking) {
      setCompatReport(report);
      setCompatBlocking(true);
      return;
    }
    if (report.unsupported_apis.length > 0 && !(await getConfig())?.dontShowAgain.compatibility) {
      setCompatReport(report);
      setCompatBlocking(false);
      return;
    }
    await runSetup(true);
  };

  const start = async () => {
    if (await runUpdate()) return;
    await proceed();
  };

  useEffect(() => {
    if (running.current) return;
    running.current = true;
    void start();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const updatePct = total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : null;

  return (
    <div className="h-screen w-full flex flex-col items-center justify-center">
      <Link to="/dashboard" id="dashlink"></Link>
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
                onClick={e => { e.preventDefault(); void openUrl(DOWNLOAD_URL); }}
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
              onClick={() => commands.exitApp()}
            >
              {t("common.quit")}
            </AlertDialogAction>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={async () => {
                setUpdateFailed(false);
                void proceed();
              }}
            >
              {t("common.continue")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <Loader2 className="animate-spin mb-4" size={48} />
      <p className="text-xl font-semibold">{t("bootstrap.starting")}</p>
      <AlertDialog open={error}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("bootstrap.setupError.title")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("bootstrap.setupError.bodyBeforePrompt")}<b>{t("bootstrap.setupError.installPrompt")}</b>{t("bootstrap.setupError.bodyAfterPrompt")}<a href={`mailto:${t("common.supportEmail")}`} target="_blank" style={{ textDecoration: "underline" }}>{t("common.supportEmail")}</a>{t("bootstrap.setupError.contactSuffix")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
              onClick={async () => {
                setLoading(true);
                await commands.installDrivers();
                await new Promise(resolve => setTimeout(resolve, 5000));
                setLoading(false);
                setError(false);
                await runSetup(false);
              }}
              disabled={loading}
            >
              {t("bootstrap.setupError.install")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={compatReport !== null}>
        <AlertDialogContent className="max-w-2xl">
          <AlertDialogHeader>
            <AlertDialogTitle>
              {compatBlocking ? t("bootstrap.compatibility.blockingTitle") : t("bootstrap.compatibility.limitedTitle")}
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
                    {compatBlocking
                      ? t("bootstrap.compatibility.blockingIntro")
                      : t("bootstrap.compatibility.limitedIntro")}
                    <ul className="list-disc pl-5 mt-2 space-y-1">
                      {compatReport.unsupported_apis.map(api => (
                        <li key={api.name}>
                          <b>{api.name}</b> — {api.description} {t("bootstrap.compatibility.apiRequires", { version: api.required_version })}
                        </li>
                      ))}
                    </ul>
                  </div>
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
            {!compatBlocking && (
              <AlertDialogAction
                className="bg-blue-600 hover:bg-blue-700 text-white"
                onClick={async () => {
                  await updateConfig({dontShowAgain: {...(await getConfig())!.dontShowAgain, compatibility: compatDontShowAgain}});
                  setCompatReport(null);
                  void runSetup(true);
                }}
              >
                {t("common.continue")}
              </AlertDialogAction>
            )}
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={() => commands.exitApp()}
            >
              {t("common.exit")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={permReport !== null}>
        <AlertDialogContent className="max-w-2xl">
          <AlertDialogHeader>
            <AlertDialogTitle>Permissions needed</AlertDialogTitle>
            <AlertDialogDescription asChild>
              <div className="space-y-3 text-left">
                <div>
                  ScreenExtend needs macOS permission to capture this screen and to control the
                  keyboard and mouse from your connected devices. Enable each item below in System
                  Settings, then relaunch.
                </div>
                <ul className="space-y-2">
                  {permReport?.map(p => (
                    <li
                      key={p.key}
                      className="flex items-start justify-between gap-3 rounded-md border p-3"
                    >
                      <div>
                        <div className="font-semibold flex items-center gap-2">
                          {p.granted
                            ? <Check className="text-green-600 mr-2" size={16} />
                            : <X className="text-red-600 mr-2" size={16} />}
                          {p.name}
                        </div>
                        <div className="text-sm text-muted-foreground">{p.description}</div>
                      </div>
                      {!p.granted && (
                        <button
                          className="shrink-0 rounded-md bg-secondary px-3 py-1.5 text-sm hover:bg-secondary/80"
                          onClick={() => void commands.openPermissionSettings(p.key)}
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
            <AlertDialogAction
              className="bg-secondary hover:bg-secondary/80 text-secondary-foreground"
              onClick={() => commands.exitApp()}
            >
              {t("common.quit")}
            </AlertDialogAction>
            <button
              className="inline-flex h-10 items-center justify-center rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground hover:bg-secondary/80"
              onClick={() => void recheckPermissions()}
            >
              Re-check
            </button>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={() => void relaunch()}
            >
              Relaunch
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
