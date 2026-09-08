import { Suspense, lazy, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Route,
  RouterProvider,
  createMemoryRouter,
  createRoutesFromElements,
} from "react-router-dom";

import Bootstrap from "@/pages/bootstrap";
import { loadDashboard, loadDevices, loadSettings } from "@/lib/route-chunks";
import { Loader2 } from "lucide-react";

// React Router 7 wraps navigations in `startTransition`, so a route that has not finished
// loading holds the current screen rather than flashing the fallback. Bootstrap warms the
// dashboard chunk while the Rust `setup()` runs and the other two once it has painted, so
// in practice these are resolved before anything navigates to them.
const Dashboard = lazy(loadDashboard);
const Devices = lazy(loadDevices);
const Settings = lazy(loadSettings);

import { NextStepProvider, NextStepReact } from "nextstepjs";
import { HighlightProxy, WalkthroughArrow, WalkthroughCard, walkthroughSteps } from "@/components/walkthrough";

import { getConfig, getConfigSnapshot, subscribeToConfig, updateConfig, flushConfig, recordKnownDevice, type Device } from "@/components/config-provider";
import { loadAvatar } from "@/lib/avatar";
import { DEFAULT_ZOOM, applyZoom, clampZoom, zoomIn, zoomOut } from "@/lib/zoom";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import {
  AudioOutputsContext,
  AvatarContext,
  ClosingContext,
  DevicesContext,
  LoadedContext,
  QrValuesContext,
  SessionContext,
  SettersContext,
  ZoomContext,
  useClosing,
  type AudioOutputsInfo,
  type GlobalSetters,
  type QrValue,
} from "@/components/global-provider";
import { ThemeProvider } from "@/components/theme-provider";
import { commands, events } from "@/lib/bindings";
import { buildQrValues } from "@/lib/utils";
import { toast } from "@/components/ui/use-toast";
import { t, useTranslation } from "@/i18n";
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
      <Route path="/dashboard" element={<Suspense fallback={null}><Dashboard /></Suspense>} />
      <Route path="/devices" element={<Suspense fallback={null}><Devices /></Suspense>} />
      <Route path="/settings" element={<Suspense fallback={null}><Settings /></Suspense>} />
    </>
  )
);

// `networkChange` fires on any adapter change -- a VPN coming up, Wi-Fi roaming -- and
// usually reports the same set. Handing back the previous array keeps the QR components
// and every other consumer of the context out of the commit.
const sameQrValues = (a: QrValue[], b: QrValue[]): boolean =>
  a.length === b.length && a.every((v, i) => v.title === b[i].title && v.value === b[i].value);

const deviceEquals = (a: Device, b: Device): boolean => {
  const keys = Object.keys(b) as (keyof Device)[];
  if (keys.length !== Object.keys(a).length) return false;
  return keys.every(key => a[key] === b[key]);
};

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

function ClosingOverlay() {
  const closing = useClosing();
  const { t } = useTranslation();
  return (
    <div className="fixed top-0 right-0 bottom-0 left-0 bg-black bg-opacity-80 flex items-center justify-center" style={{ display: closing ? "flex" : "none", zIndex: 999999 }}>
      <div className="rounded-lg p-6 flex flex-col items-center">
        <Loader2 className="animate-spin text-white mb-4" size={48} />
        <p className="text-xl font-semibold text-white">{t("app.closing")}</p>
      </div>
    </div>
  );
}

// Owns every piece of global state and every backend subscription. It renders
// `children` untouched, so the element tree below it is referentially stable and
// React skips it when this component re-renders: only the consumers of the one
// context that actually changed commit.
function GlobalState({ children }: { children: ReactNode }) {
  const [hostedNetworkOn, setHostedNetworkOn] = useState(false);
  const [otp, setOtp] = useState("");
  const [sessionId, setSessionId] = useState("");
  const [qrValues, setQrValues] = useState<QrValue[]>([]);
  const applyQrValues = useMemo(
    () => (next: QrValue[]) => setQrValues(prev => (sameQrValues(prev, next) ? prev : next)),
    []
  );
  const [loaded, setLoaded] = useState(false);
  const [devices, setDevices] = useState<Device[]>([]);
  const [publicSessionsEnabled, setPublicSessionsEnabled] = useState(true);
  const [avatar, setAvatar] = useState<string | null>(null);
  const [zoom, setZoom] = useState(DEFAULT_ZOOM);
  const [audioOutputsByIp, setAudioOutputsByIp] = useState<Record<string, AudioOutputsInfo>>({});
  const zoomReady = useRef(false);
  const appliedZoom = useRef(DEFAULT_ZOOM);
  const zoomPersist = useRef<number | undefined>(undefined);

  const [closing, setClosing] = useState(false);

  const setters = useMemo<GlobalSetters>(() => ({
    setHostedNetworkOn,
    setOtp,
    setSessionId,
    setQrValues,
    setLoaded,
    setClosing,
    setDevices,
    setPublicSessionsEnabled,
    setAvatar,
    setZoom,
    setAudioOutputsByIp,
  }), []);

  const session = useMemo(
    () => ({ sessionId, otp, publicSessionsEnabled, hostedNetworkOn }),
    [sessionId, otp, publicSessionsEnabled, hostedNetworkOn]
  );

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

  // The rescale is applied per keypress; only the store write is coalesced, so
  // holding Ctrl+`+` through five steps is one write instead of five.
  useEffect(() => {
    if (!zoomReady.current) return;
    void applyZoom(zoom).then(() => {
      appliedZoom.current = zoom;
      if (zoomPersist.current !== undefined) window.clearTimeout(zoomPersist.current);
      zoomPersist.current = window.setTimeout(() => {
        zoomPersist.current = undefined;
        void updateConfig({ zoomFactor: appliedZoom.current }).catch(e => console.error("persisting zoomFactor failed", e));
      }, 500);
    }).catch(() => {
      setZoom(appliedZoom.current);
      toast({
        variant: "destructive",
        title: t("toasts.zoom.failureTitle"),
        description: t("toasts.zoom.failureDescription"),
      });
    });
  }, [zoom]);

  useEffect(() => () => {
    if (zoomPersist.current !== undefined) window.clearTimeout(zoomPersist.current);
  }, []);

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
        setDevices(prev => {
          const index = prev.findIndex(d => d.ip === device.ip);
          if (index < 0) return prev;
          // A no-op modify must not allocate a new array: `devices` identity
          // drives an extra IPC burst in Dashboard and Settings.
          if (deviceEquals(prev[index], device)) return prev;
          const next = prev.slice();
          next[index] = device;
          return next;
        });
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
          applyQrValues(await buildQrValues(newId, cfg?.serverPorts?.http));
        } catch {
          applyQrValues(await buildQrValues(newId, getConfigSnapshot()?.serverPorts?.http).catch(() => []));
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
          applyQrValues(await buildQrValues(id, cfg?.serverPorts?.http));
        } catch {
          applyQrValues(await buildQrValues(id, getConfigSnapshot()?.serverPorts?.http).catch(() => []));
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
  }, [loaded]);

  return (
    <SettersContext.Provider value={setters}>
      <SessionContext.Provider value={session}>
        <QrValuesContext.Provider value={qrValues}>
          <DevicesContext.Provider value={devices}>
            <AudioOutputsContext.Provider value={audioOutputsByIp}>
              <AvatarContext.Provider value={avatar}>
                <ZoomContext.Provider value={zoom}>
                  <ClosingContext.Provider value={closing}>
                    <LoadedContext.Provider value={loaded}>
                      {children}
                    </LoadedContext.Provider>
                  </ClosingContext.Provider>
                </ZoomContext.Provider>
              </AvatarContext.Provider>
            </AudioOutputsContext.Provider>
          </DevicesContext.Provider>
        </QrValuesContext.Provider>
      </SessionContext.Provider>
    </SettersContext.Provider>
  );
}

function App() {
  return (
    <GlobalState>
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
            <ClosingOverlay />
          </NextStepReact>
        </NextStepProvider>
        </ThemeProvider>
    </GlobalState>
  );
}

export default App;
