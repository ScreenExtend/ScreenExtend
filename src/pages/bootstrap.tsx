import { useContext, useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";

import { Loader2 } from "lucide-react";
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
import { commands, type CompatibilityReport } from "@/lib/bindings";
import { buildQrValues } from "@/lib/utils";
import { useTheme, type Theme } from "@/components/theme-provider";

export default function Bootstrap() {
  const { theme, setTheme } = useTheme();
  const { windowLoaded: [loaded, setLoaded], windowOtp: [, setOtp], windowHostedNetworkOn: [, setHostedNetworkOn], windowSessionId: [, setSessionId], windowQrValues: [, setQrValues], windowPublicSessionsEnabled: [, setPublicSessionsEnabled] } = useContext(GlobalProviderContext);

  const [error, setError] = useState(false);
  const [loading, setLoading] = useState(false);
  const [compatReport, setCompatReport] = useState<CompatibilityReport | null>(null);
  const [compatBlocking, setCompatBlocking] = useState(false);
  const [compatDontShowAgain, setCompatDontShowAgain] = useState(true);
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
      const existing = await getConfig();
      const savedPorts = existing?.serverPorts;
      if (savedPorts) {
        await commands.setServerPorts(savedPorts.http, savedPorts.https);
      }
      for (const device of existing?.devices ?? []) {
        await commands.setDeviceOverride(
          device.ip,
          device.scale,
          device.orientation,
          device.refreshRate,
          device.videoScale,
          device.videoQuality
        );
      }
      const publicSessionsEnabled = existing?.publicSessionsEnabled !== false;
      setPublicSessionsEnabled(publicSessionsEnabled);

      await commands.watchForNetworkChanges();
      const newSessionId = Array.from(crypto.getRandomValues(new Uint8Array(12)), b => '23456789ABCDEFGHJKLMNPQRSTUVWXYZ'[b % 32]).join('');
      const newOtp = [...Array(6)].reduce(a => a + "0123456789"[~~(Math.random() * "0123456789".length)], "");
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
      if (!existing) {
        await createConfig({ name: await commands.getUsername(), theme });
      } else {
        setTheme(existing.theme as Theme);
      }
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

  const start = async () => {
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

  useEffect(() => {
    if (running.current) return;
    running.current = true;
    void start();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="h-screen w-full flex flex-col items-center justify-center">
      <Link to="/dashboard" id="dashlink"></Link>
      <Loader2 className="animate-spin mb-4" size={48} />
      <p className="text-xl font-semibold">Starting ScreenExtend</p>
      <AlertDialog open={error}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Setup Error</AlertDialogTitle>
            <AlertDialogDescription>
              There was an error while attepting to start ScreenExtend. This often occurs due to core drivers or libraries not being installed. <b>Click the button below to install the missing components.</b> If this error is recurring, contact support at <a href="mailto:support@screenextend.app" target="_blank" style={{ textDecoration: "underline" }}>support@screenextend.app</a>.
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
              Install Drivers
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={compatReport !== null}>
        <AlertDialogContent className="max-w-2xl">
          <AlertDialogHeader>
            <AlertDialogTitle>
              {compatBlocking ? "Unsupported Operating System" : "Limited System Support"}
            </AlertDialogTitle>
            <AlertDialogDescription asChild>
              <div className="space-y-3 text-left">
                <div>
                  Detected system: <b>{compatReport?.os_version}</b>
                  <br />
                  Minimum required: <b>{compatReport?.min_os_version}</b>
                </div>
                {compatReport && compatReport.unsupported_apis.length > 0 && (
                  <div>
                    {compatBlocking
                      ? "ScreenExtend cannot run because the following required platform APIs are unavailable on this system:"
                      : "The following platform APIs are unavailable. ScreenExtend can continue, but these features will be limited or non-functional:"}
                    <ul className="list-disc pl-5 mt-2 space-y-1">
                      {compatReport.unsupported_apis.map(api => (
                        <li key={api.name}>
                          <b>{api.name}</b> — {api.description} (requires {api.required_version})
                        </li>
                      ))}
                    </ul>
                  </div>
                )}
                <div>
                  Please upgrade your operating system or contact support at{" "}
                  <a href="mailto:support@screenextend.app" target="_blank" style={{ textDecoration: "underline" }}>
                    support@screenextend.app
                  </a>.
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
                Don't show this message again
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
                Continue
              </AlertDialogAction>
            )}
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={() => commands.exitApp()}
            >
              Exit
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
