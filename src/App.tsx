import { useEffect, useRef, useState } from "react";
import {
  Route,
  RouterProvider,
  createMemoryRouter,
  createRoutesFromElements,
} from "react-router-dom";

import Bootstrap from "@/pages/bootstrap";
import Dashboard from "@/pages/dashboard";
import Settings from "@/pages/settings";
import Devices from "@/pages/devices";
import { Loader2 } from "lucide-react";

import { NextStepProvider, NextStepReact } from "nextstepjs";
import { HighlightProxy, WalkthroughArrow, WalkthroughCard, walkthroughSteps } from "@/components/walkthrough";

import { getConfig, getConfigSnapshot, subscribeToConfig, updateConfig, flushConfig, recordKnownDevice, type Device } from "@/components/config-provider";
import { loadAvatar } from "@/lib/avatar";
import { DEFAULT_ZOOM, applyZoom, clampZoom, zoomIn, zoomOut } from "@/lib/zoom";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { GlobalProviderContext, type AudioOutputsInfo } from "@/components/global-provider";
import { ThemeProvider } from "@/components/theme-provider";
import { commands, events } from "@/lib/bindings";
import { buildQrValues } from "@/lib/utils";
import { useToast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import "non.geist";
const appWindow = getCurrentWebviewWindow();

const stopListening = (unlisten: () => void) => {
  void Promise.resolve(unlisten()).catch(e => console.error("unlisten failed", e));
};

const DEVICE_OVERRIDE_KEYS = [
  "scale",
  "orientation",
  "refreshRate",
  "videoScale",
  "videoQuality",
  "remoteControl",
  "systemAudio",
  "dpr",
  "audioOutputDeviceId",
  "audioOutputDeviceLabel",
] as const satisfies readonly (keyof Device)[];

const router = createMemoryRouter(
  createRoutesFromElements(
    <>
      <Route path="/" element={<Bootstrap />} />
      <Route path="/dashboard" element={<Dashboard />} />
      <Route path="/devices" element={<Devices />} />
      <Route path="/settings" element={<Settings />} />
    </>
  )
);

function App() {
  const [hostedNetworkOn, setHostedNetworkOn] = useState(false);
  const [otp, setOtp] = useState("");
  const [sessionId, setSessionId] = useState("");
  const [qrValues, setQrValues] = useState([] as { title: string; value: string; }[]);
  const [loaded, setLoaded] = useState(false);
  const [devices, setDevices] = useState<Device[]>([]);
  const [publicSessionsEnabled, setPublicSessionsEnabled] = useState(true);
  const [avatar, setAvatar] = useState<string | null>(null);
  const [zoom, setZoom] = useState(DEFAULT_ZOOM);
  const [audioOutputsByIp, setAudioOutputsByIp] = useState<Record<string, AudioOutputsInfo>>({});
  const zoomReady = useRef(false);
  const appliedZoom = useRef(DEFAULT_ZOOM);

  const [closing, setClosing] = useState(false);

  const { toast } = useToast();
  const { t } = useTranslation();

  useEffect(() => {
    void (async () => {
      try {
        setAvatar(await loadAvatar());
      } catch {
        setAvatar(null);
      }
    })();
  }, []);

  useEffect(() => {
    void (async () => {
      let saved = DEFAULT_ZOOM;
      try {
        const cfg = await getConfig();
        saved = clampZoom(cfg?.zoomFactor ?? DEFAULT_ZOOM);
      } catch (e) {
        console.error("reading the saved zoom failed", e);
      } finally {
        zoomReady.current = true;
      }
      try {
        await applyZoom(saved);
        appliedZoom.current = saved;
        setZoom(saved);
      } catch (e) {
        console.error("applyZoom failed", e);
      }
    })();
  }, []);

  useEffect(() => {
    if (!zoomReady.current) return;
    void applyZoom(zoom).then(() => {
      appliedZoom.current = zoom;
      void updateConfig({ zoomFactor: zoom }).catch(e => console.error("persisting zoomFactor failed", e));
    }).catch(() => {
      setZoom(appliedZoom.current);
      toast({
        variant: "destructive",
        title: t("toasts.zoom.failureTitle"),
        description: t("toasts.zoom.failureDescription"),
      });
    });
  }, [zoom]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return;
      if (e.key === "+" || e.key === "=") {
        e.preventDefault();
        setZoom(z => zoomIn(z));
      } else if (e.key === "-" || e.key === "_") {
        e.preventDefault();
        setZoom(z => zoomOut(z));
      } else if (e.key === "0") {
        e.preventDefault();
        setZoom(DEFAULT_ZOOM);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const sessionIdRef = useRef(sessionId);
  useEffect(() => { sessionIdRef.current = sessionId; }, [sessionId]);

  const devicesRef = useRef(devices);
  useEffect(() => { devicesRef.current = devices; }, [devices]);

  useEffect(() => {
    const syncOverrides = () => {
      const saved = getConfigSnapshot()?.devices;
      if (!saved) return;
      setDevices(prev => {
        let changed = false;
        const next = prev.map(device => {
          const override = saved.find(d => d.ip === device.ip);
          if (!override) return device;
          const differs = DEVICE_OVERRIDE_KEYS.filter(
            key => override[key] !== undefined && override[key] !== device[key]
          );
          if (differs.length === 0) return device;
          changed = true;
          const merged = { ...device };
          for (const key of differs) {
            (merged as Record<string, unknown>)[key] = override[key];
          }
          return merged;
        });
        return changed ? next : prev;
      });
    };
    syncOverrides();
    return subscribeToConfig(syncOverrides);
  }, []);

  const notifyGranted = useRef(false);
  useEffect(() => {
    void (async () => {
      try {
        let granted = await isPermissionGranted();
        if (!granted) granted = (await requestPermission()) === "granted";
        notifyGranted.current = granted;
      } catch (e) {
        notifyGranted.current = false;
        console.error("notification permission check failed", e);
      }
    })();
  }, []);

  const notifyIfUnfocused = async (title: string, body: string) => {
    if (!notifyGranted.current) return;
    try {
      if (await appWindow.isFocused()) return;
      sendNotification({ title, body });
    } catch (e) {
      console.error("notification failed", e);
    }
  };

  useEffect(() => {
    if (sessionId && otp) {
      void commands.setSessionCredentials(sessionId, otp).catch(() => {
        toast({
          variant: "destructive",
          title: t("toasts.sessionCredentials.failureTitle"),
          description: t("toasts.sessionCredentials.failureDescription"),
        });
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, otp]);

  useEffect(() => {
    if (!loaded) return;
    const stored = localStorage.getItem("disconnectGraceSecs");
    const seconds = Number(stored);
    if (stored !== null && Number.isFinite(seconds) && seconds >= 0) {
      void commands.setDisconnectGrace(seconds).catch(e => console.error("setDisconnectGrace on load failed", e));
    }
  }, [loaded]);

  useEffect(() => {
    const unlisteners: (() => void)[] = [];
    const start_listener = async () => {
      unlisteners.push(await events.deviceJoin.listen(event => {
        const device = event.payload as Device;
        setDevices(prev => {
          const next = prev.filter(d => d.ip !== device.ip);
          next.push(device);
          return next;
        });
        void (async () => {
          const results = await Promise.allSettled([
            recordKnownDevice({
              token: device.token,
              ip: device.ip,
              name: device.name,
              os: device.os,
              screenSize: device.screenSize,
            }),
            commands.setDeviceApproved(device.token, true),
          ]);
          const failed = results.find((r): r is PromiseRejectedResult => r.status === "rejected");
          if (!failed) return;
          console.error("remembering the joined device failed", failed.reason);
          toast({
            variant: "destructive",
            title: t("toasts.deviceApprove.failureTitle"),
            description: t("toasts.deviceApprove.failureDescription"),
          });
        })();
        void notifyIfUnfocused(
          t("notifications.deviceJoin.title"),
          t("notifications.deviceJoin.body", { name: device.name || device.ip }),
        );
      }));
      unlisteners.push(await events.deviceModify.listen(event => {
        const device = event.payload as Device;
        setDevices(prev => prev.map(d => d.ip === device.ip ? device : d));
      }));
      unlisteners.push(await events.deviceAudioOutputs.listen(event => {
        const p = event.payload;
        setAudioOutputsByIp(prev => ({
          ...prev,
          [p.ip]: { supported: p.supported, outputs: p.outputs, selected: p.selected },
        }));
      }));
      unlisteners.push(await events.deviceRemove.listen(event => {
        const device = event.payload as Device;
        const known = devicesRef.current.find(d => d.ip === device.ip);
        const name = known?.name || device.name || device.ip;
        setDevices(prev => prev.filter(d => d.ip !== device.ip));
        void notifyIfUnfocused(
          t("notifications.deviceRemove.title"),
          t("notifications.deviceRemove.body", { name }),
        );
      }));
      unlisteners.push(await events.hostedNetworkNoPassword.listen(() => {
        toast({
          variant: "destructive",
          title: t("toasts.networkCreatedNoPassword.title"),
          description: t("toasts.networkCreatedNoPassword.description"),
        });
      }));
      unlisteners.push(await events.sessionIdChange.listen(async event => {
        const newId = (event.payload as { sessionId: string }).sessionId;
        if (!newId) return;
        setSessionId(newId);
        try {
          const cfg = await getConfig();
          setQrValues(await buildQrValues(newId, cfg?.serverPorts?.http));
        } catch {
          setQrValues(await buildQrValues(newId, getConfigSnapshot()?.serverPorts?.http).catch(() => []));
          toast({
            variant: "destructive",
            title: t("toasts.qrRefresh.failureTitle"),
            description: t("toasts.qrRefresh.failureDescription"),
          });
        }
      }));
      unlisteners.push(await events.networkChange.listen(async () => {
        const id = sessionIdRef.current;
        if (!id) return;
        try {
          const cfg = await getConfig();
          setQrValues(await buildQrValues(id, cfg?.serverPorts?.http));
        } catch {
          setQrValues(await buildQrValues(id, getConfigSnapshot()?.serverPorts?.http).catch(() => []));
          toast({
            variant: "destructive",
            title: t("toasts.qrRefresh.failureTitle"),
            description: t("toasts.qrRefresh.failureDescription"),
          });
        }
      }));
      unlisteners.push(await events.joinAttemptsPaused.listen(event => {
        const seconds = (event.payload as { retryAfterSecs: number }).retryAfterSecs;
        toast({
          variant: "destructive",
          title: t("toasts.joinAttemptsPaused.title"),
          description: t("toasts.joinAttemptsPaused.description", { seconds }),
        });
      }));
    }
    void start_listener().catch(e => {
      console.error("event listener registration failed", e);
      toast({
        variant: "destructive",
        title: t("toasts.eventBridge.failureTitle"),
        description: t("toasts.eventBridge.failureDescription"),
      });
    });
    return () => unlisteners.forEach(stopListening);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void appWindow.onCloseRequested(async event => {
      setClosing(true);
      try {
        if (loaded) await commands.stopHostedNetwork();
      } catch (e) {
        console.error("stopHostedNetwork on close failed", e);
      }
      try {
        await commands.exitApp();
      } catch (e) {
        event.preventDefault();
        console.error("exitApp failed", e);
        setClosing(false);
        toast({
          variant: "destructive",
          title: t("toasts.exitApp.failureTitle"),
          description: t("toasts.exitApp.failureDescription"),
        });
      }
    }).then(un => { unlisten = un; }).catch(e => console.error("onCloseRequested registration failed", e));
    return () => { if (unlisten) stopListening(unlisten); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loaded]);

  const markWalkthroughDone = async () => {
    try {
      await updateConfig({ walkthroughCompleted: true });
      await flushConfig();
    } catch {
      toast({
        variant: "destructive",
        title: t("toasts.walkthrough.failureTitle"),
        description: t("toasts.walkthrough.failureDescription"),
      });
    }
  };

  return (
    <GlobalProviderContext.Provider value={{
      windowHostedNetworkOn: [hostedNetworkOn, setHostedNetworkOn],
      windowOtp: [otp, setOtp],
      windowSessionId: [sessionId, setSessionId],
      windowQrValues: [qrValues, setQrValues],
      windowLoaded: [loaded, setLoaded],
      windowClosing: [closing, setClosing],
      windowDevices: [devices, setDevices],
      windowPublicSessionsEnabled: [publicSessionsEnabled, setPublicSessionsEnabled],
      windowAvatar: [avatar, setAvatar],
      windowZoom: [zoom, setZoom],
      windowAudioOutputsByIp: [audioOutputsByIp, setAudioOutputsByIp]
    }}>
      <ThemeProvider defaultTheme="system">
        <NextStepProvider>
          <NextStepReact
            steps={walkthroughSteps}
            cardComponent={WalkthroughCard}
            onComplete={() => void markWalkthroughDone()}
            onSkip={() => void markWalkthroughDone()}
            shadowOpacity="0.5"
            arrowComponent={WalkthroughArrow}
            cardTransition={{ ease: "easeInOut", duration: 0.4 }}
            disableConsoleLogs
          >
            <HighlightProxy />
            <RouterProvider router={router} />
            <div className="fixed top-0 right-0 bottom-0 left-0 bg-black bg-opacity-80 flex items-center justify-center" style={{ display: closing ? "flex" : "none", zIndex: 999999 }}>
              <div className="rounded-lg p-6 flex flex-col items-center">
                <Loader2 className="animate-spin text-white mb-4" size={48} />
                <p className="text-xl font-semibold text-white">{t("app.closing")}</p>
              </div>
            </div>
          </NextStepReact>
        </NextStepProvider>
        </ThemeProvider>
    </GlobalProviderContext.Provider>
  );
}

export default App;
