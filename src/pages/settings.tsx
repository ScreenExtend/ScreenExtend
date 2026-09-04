import React, { useState, useContext, useEffect, useRef, useCallback, lazy, Suspense } from "react";
import QRCode from "react-qr-code";
import { useBlocker } from "react-router-dom";

import Layout from "@/layout/layout";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Avatar as AvatarWrapper } from "@/components/ui/avatar";
import { AvatarCropModal } from "@/components/avatar-crop-modal";
import { Eye, EyeOff, RefreshCw, Camera, Minus, Plus, RotateCcw, ChevronDown, QrCode, Ban, Trash2 } from "lucide-react";
import defaultLogo from "@/assets/default.svg";
import {
  InputOTP,
  InputOTPGroup,
  InputOTPSlot,
} from "@/components/ui/input-otp";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle
} from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";

import { updateConfig, getConfig, flushConfig, getKnownDevices, setKnownDeviceBanned, removeKnownDevice, DEFAULT_HTTP_PORT, DEFAULT_HTTPS_PORT, type Config, type KnownDevice } from "@/components/config-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { LogTerminal } from "@/components/log-terminal";
const ConfigJsonEditor = lazy(() =>
  import("@/components/config-json-editor").then(m => ({ default: m.ConfigJsonEditor }))
);
import { useTheme, type Theme } from "@/components/theme-provider";
import { useToast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { commands, type ServerPorts } from "@/lib/bindings";
import { cn, buildQrValues, buildWifiQrValue, generateOtp } from "@/lib/utils";
import { saveAvatar, clearAvatar } from "@/lib/avatar";
import { DEFAULT_ZOOM, MIN_ZOOM, MAX_ZOOM, clampZoom, zoomIn, zoomOut, formatZoom } from "@/lib/zoom";
import { type as getOsType } from "@tauri-apps/plugin-os";
import { enable as enableAutostart, disable as disableAutostart, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";

const MIN_HOSTED_NETWORK_PASSWORD_LENGTH = getOsType() === "macos" ? 10 : 8;
const IS_WINDOWS = getOsType() === "windows";
const IS_MACOS = getOsType() === "macos";
const SUPPORTS_WIFI_QR = IS_WINDOWS || IS_MACOS;

function isLikelyIp(value: string): boolean {
  if (!value) return false;
  if (/^(\d{1,3}\.){3}\d{1,3}$/.test(value)) {
    return value.split(".").every(octet => Number(octet) <= 255);
  }
  return value.includes(":") && /^[0-9a-fA-F:]+$/.test(value);
}

function formatLastSeen(ts: number): string {
  if (!ts) return "—";
  const mins = Math.floor((Date.now() - ts) / 60000);
  if (mins < 1) return "Just now";
  if (mins < 60) return `${mins} min ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs} hr${hrs === 1 ? "" : "s"} ago`;
  const days = Math.floor(hrs / 24);
  if (days < 30) return `${days} day${days === 1 ? "" : "s"} ago`;
  return new Date(ts).toLocaleDateString();
}

export default function Settings() {
  const { windowOtp: [otp, setOtp], windowHostedNetworkOn: [hostedNetworkOn, setHostedNetworkOn], windowSessionId: [sessionId], windowQrValues: [, setQrValues], windowPublicSessionsEnabled: [publicSessionsEnabled, setPublicSessionsEnabled], windowAvatar: [avatar, setAvatar], windowZoom: [zoom, setZoom], windowDevices: [connectedDevices] } = useContext(GlobalProviderContext);
  const { toast } = useToast();
  const { t } = useTranslation();
  const { setTheme } = useTheme();

  const fileInputRef = useRef<HTMLInputElement>(null);
  const [cropSrc, setCropSrc] = useState<string | null>(null);
  const [cropOpen, setCropOpen] = useState(false);

  const [spin, setSpin] = useState(false);
  const [hostedNetworkName, setHostedNetworkName] = useState("");
  const [hostedNetworkPassword, setHostedNetworkPassword] = useState("");
  const [oldHostedNetworkName, setOldHostedNetworkName] = useState("");
  const [oldHostedNetworkPassword, setOldHostedNetworkPassword] = useState("");
  const [showHostedNetworkPassword, setShowHostedNetworkPassword] = useState(false);
  const [hostedNetworkModalOpen, setHostedNetworkModalOpen] = useState(false);
  const [wifiQrModalOpen, setWifiQrModalOpen] = useState(false);
  const [wifiModalOpen, setWifiModalOpen] = useState(false);
  const [wifiTurningOn, setWifiTurningOn] = useState(false);
  const [disabled, setDisabled] = useState(false);
  const [inputDisabled, setInputDisabled] = useState(false);
  const [credentialsUnlocked, setCredentialsUnlocked] = useState(false);
  const [dontShowAgain, setDontShowAgain] = useState(true);
  const [accountName, setAccountName] = useState("");
  const [disconnectGrace, setDisconnectGrace] = useState("");
  const [oldDisconnectGrace, setOldDisconnectGrace] = useState("");
  const [turnUrls, setTurnUrls] = useState("");
  const [turnUsername, setTurnUsername] = useState("");
  const [turnCredential, setTurnCredential] = useState("");
  const [showTurnCredential, setShowTurnCredential] = useState(false);
  const [httpPort, setHttpPort] = useState(String(DEFAULT_HTTP_PORT));
  const [httpsPort, setHttpsPort] = useState(String(DEFAULT_HTTPS_PORT));
  const [oldHttpPort, setOldHttpPort] = useState(String(DEFAULT_HTTP_PORT));
  const [oldHttpsPort, setOldHttpsPort] = useState(String(DEFAULT_HTTPS_PORT));
  const [configLoaded, setConfigLoaded] = useState(false);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [disableGpuEncode, setDisableGpuEncode] = useState(false);
  const [autostartEnabled, setAutostartEnabled] = useState(false);
  const [knownDevices, setKnownDevices] = useState<KnownDevice[]>([]);
  const [banIpInput, setBanIpInput] = useState("");
  const [configDirty, setConfigDirty] = useState(false);
  const [configUnsavedOpen, setConfigUnsavedOpen] = useState(false);
  const [configDontShowAgain, setConfigDontShowAgain] = useState(true);
  const configProceeding = useRef(false);
  const configLoadOk = useRef(false);
  const credentialsSaveToastShown = useRef(false);
  const knownDevicesLoaded = useRef(false);

  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) =>
      configDirty && currentLocation.pathname !== nextLocation.pathname
  );

  useEffect(() => {
    if (blocker.state !== "blocked") {
      setConfigUnsavedOpen(false);
      return;
    }
    void (async () => {
      try {
        if ((await getConfig())?.dontShowAgain?.configEditor) {
          blocker.proceed?.();
        } else {
          setConfigUnsavedOpen(true);
        }
      } catch {
        setConfigUnsavedOpen(true);
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [blocker.state]);

  const rememberConfigDialogChoice = async () => {
    try {
      const current = await getConfig();
      if (!current) return;
      await updateConfig({
        dontShowAgain: { ...current.dontShowAgain, configEditor: configDontShowAgain },
      });
      await flushConfig();
    } catch (e) {
      console.error("failed to persist config editor dialog choice", e);
    }
  };

  const handleConfigSaved = useCallback(async (config: Config) => {
    setAccountName(config.name);
    setHostedNetworkName(config.hostedNetworkCredentials.name);
    setHostedNetworkPassword(config.hostedNetworkCredentials.password);
    setOldHostedNetworkName(config.hostedNetworkCredentials.name);
    setOldHostedNetworkPassword(config.hostedNetworkCredentials.password);
    setTurnUrls(config.turnConfig.urls);
    setTurnUsername(config.turnConfig.username);
    setTurnCredential(config.turnConfig.credential);
    setHttpPort(String(config.serverPorts.http));
    setHttpsPort(String(config.serverPorts.https));
    setOldHttpPort(String(config.serverPorts.http));
    setOldHttpsPort(String(config.serverPorts.https));
    setDisableGpuEncode(config.disableGpuEncode);
    setZoom(clampZoom(config.zoomFactor));
    setPublicSessionsEnabled(config.publicSessionsEnabled);
    try {
      await setTheme(config.theme as Theme);
      if (config.publicSessionsEnabled) {
        if (sessionId) void commands.registerCloudSession(sessionId).catch(e => console.error("cloud session registration failed", e));
      } else {
        void commands.unregisterCloudSession().catch(e => console.error("cloud session unregistration failed", e));
      }
      if (sessionId) setQrValues(await buildQrValues(sessionId, config.serverPorts.http));
      setKnownDevices(await getKnownDevices());
    } catch {
      toast({
        title: t("toasts.configEditor.refreshFailedTitle"),
        description: t("toasts.configEditor.refreshFailedDescription"),
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  const handleNetworkNameChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    let value = e.target.value;
    if (value.length > 32) {
      value = value.substring(0, 32);
    }
    if (value.startsWith("ScreenExtend")) {
      setHostedNetworkName(value);
    } else {
      setHostedNetworkName("ScreenExtend" + value.slice(12));
    }
  };

  const networkFieldsDisabled = (!hostedNetworkOn && !credentialsUnlocked) || inputDisabled;

  const togglePasswordVisibility = () => {
    if (networkFieldsDisabled) return;
    setShowHostedNetworkPassword(prev => !prev);
  }

  useEffect(() => {
    async function updateText() {
      try {
        const config = await getConfig();
        if (!config) throw new Error("config store is empty");
        setHostedNetworkName(config.hostedNetworkCredentials.name);
        setHostedNetworkPassword(config.hostedNetworkCredentials.password);
        setOldHostedNetworkName(config.hostedNetworkCredentials.name);
        setOldHostedNetworkPassword(config.hostedNetworkCredentials.password);
        setAccountName(config.name);
        const turn = config.turnConfig ?? { urls: "", username: "", credential: "" };
        setTurnUrls(turn.urls);
        setTurnUsername(turn.username);
        setTurnCredential(turn.credential);
        const ports = config.serverPorts ?? { http: DEFAULT_HTTP_PORT, https: DEFAULT_HTTPS_PORT };
        setHttpPort(String(ports.http));
        setHttpsPort(String(ports.https));
        setOldHttpPort(String(ports.http));
        setOldHttpsPort(String(ports.https));
        setDisableGpuEncode(config.disableGpuEncode ?? false);
        configLoadOk.current = true;
        const seconds = await commands.getDisconnectGrace();
        setDisconnectGrace(String(seconds));
        setOldDisconnectGrace(String(seconds));
      } catch {
        toast({
          variant: "destructive",
          title: t("toasts.settings.loadFailedTitle"),
          description: t("toasts.settings.loadFailedDescription"),
        });
      } finally {
        setConfigLoaded(true);
      }
    }
    void updateText();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    isAutostartEnabled().then(setAutostartEnabled).catch(e => console.error("failed to read autostart state", e));
  }, []);

  useEffect(() => {
    const firstRun = !knownDevicesLoaded.current;
    knownDevicesLoaded.current = true;
    getKnownDevices().then(setKnownDevices).catch(e => {
      console.error("failed to refresh known devices", e);
      if (firstRun) return;
      toast({
        variant: "destructive",
        title: t("toasts.knownDevices.loadFailedTitle"),
        description: t("toasts.knownDevices.loadFailedDescription"),
      });
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectedDevices]);

  const isConnected = (ip: string) => connectedDevices.some(d => d.ip === ip);

  const applyBan = async (device: { token?: string; ip: string }, banned: boolean): Promise<boolean> => {
    const token = device.token ?? "";
    const ip = device.ip;
    try {
      await commands.setDeviceBanned(token, ip, banned);
      await commands.setDeviceApproved(token, !banned);
      await setKnownDeviceBanned(token, ip, banned);
      setKnownDevices(await getKnownDevices());
    } catch {
      setKnownDevices(await getKnownDevices().catch(() => knownDevices));
      toast({
        variant: "destructive",
        title: t("toasts.deviceBan.failureTitle"),
        description: t("toasts.deviceBan.failureDescription"),
      });
      return false;
    }
    toast({
      title: banned ? t("toasts.deviceBan.bannedTitle") : t("toasts.deviceBan.unbannedTitle"),
      description: banned
        ? t("toasts.deviceBan.bannedDescription", { ip })
        : t("toasts.deviceBan.unbannedDescription", { ip }),
    });
    return true;
  };

  const banManualIp = async () => {
    const ip = banIpInput.trim();
    if (!isLikelyIp(ip)) {
      toast({
        title: t("toasts.deviceBan.invalidTitle"),
        description: t("toasts.deviceBan.invalidDescription"),
      });
      return;
    }
    setBanIpInput("");
    if (!(await applyBan({ ip, token: "" }, true))) setBanIpInput(ip);
  };

  const forgetDevice = async (device: { token?: string; ip: string }) => {
    const token = device.token ?? "";
    try {
      await commands.setDeviceBanned(token, device.ip, false);
      await commands.setDeviceApproved(token, false);
      await removeKnownDevice(token, device.ip);
      setKnownDevices(await getKnownDevices());
    } catch {
      setKnownDevices(await getKnownDevices().catch(() => knownDevices));
      toast({
        variant: "destructive",
        title: t("toasts.device.removeFailedTitle"),
        description: t("toasts.device.removeFailedDescription"),
      });
      return;
    }
    toast({
      title: t("toasts.device.removedTitle"),
      description: t("toasts.device.removedDescription"),
    });
  };

  const sortedKnownDevices = [...knownDevices].sort((a, b) => {
    const connectedDelta = Number(isConnected(b.ip)) - Number(isConnected(a.ip));
    return connectedDelta !== 0 ? connectedDelta : b.lastSeen - a.lastSeen;
  });

  const saveDisconnectGrace = async () => {
    const parsed = Number(disconnectGrace);
    if (!Number.isFinite(parsed)) {
      setDisconnectGrace(oldDisconnectGrace);
      return;
    }
    const seconds = Math.min(600, Math.max(0, Math.round(parsed)));
    setDisconnectGrace(String(seconds));
    if (String(seconds) === oldDisconnectGrace) return;
    try {
      await commands.setDisconnectGrace(seconds);
    } catch {
      setDisconnectGrace(oldDisconnectGrace);
      toast({
        variant: "destructive",
        title: t("toasts.disconnectTimeout.failureTitle"),
        description: t("toasts.disconnectTimeout.failureDescription"),
      });
      return;
    }
    localStorage.setItem("disconnectGraceSecs", String(seconds));
    setOldDisconnectGrace(String(seconds));
    toast({
      title: t("toasts.disconnectTimeout.title"),
      description: seconds === 0
        ? t("toasts.disconnectTimeout.immediate")
        : t("toasts.disconnectTimeout.delayed", {
            seconds,
            unit: t(seconds === 1 ? "toasts.disconnectTimeout.second" : "toasts.disconnectTimeout.seconds"),
          }),
    });
  };

  const saveTurnConfig = async () => {
    const urls = turnUrls.trim();
    const username = turnUsername.trim();
    const credential = turnCredential.trim();
    setTurnUrls(urls);
    setTurnUsername(username);
    setTurnCredential(credential);
    try {
      await commands.setTurnConfig(urls, username, credential);
      await updateConfig({ turnConfig: { urls, username, credential } });
    } catch {
      toast({
        variant: "destructive",
        title: t("toasts.turn.failureTitle"),
        description: t("toasts.turn.failureDescription"),
      });
      return;
    }
    toast({
      title: urls ? t("toasts.turn.savedTitle") : t("toasts.turn.clearedTitle"),
      description: urls ? t("toasts.turn.savedDescription") : t("toasts.turn.clearedDescription"),
    });
  };

  const togglePublicSessions = async (enabled: boolean) => {
    setPublicSessionsEnabled(enabled);
    try {
      await updateConfig({ publicSessionsEnabled: enabled });
      await flushConfig();
    } catch {
      setPublicSessionsEnabled(!enabled);
      toast({
        variant: "destructive",
        title: t("toasts.publicSessions.failureTitle"),
        description: t("toasts.publicSessions.failureDescription"),
      });
      return;
    }
    if (enabled) {
      if (sessionId) void commands.registerCloudSession(sessionId).catch(e => console.error("cloud session registration failed", e));
      toast({
        title: t("toasts.publicSessions.enabledTitle"),
        description: t("toasts.publicSessions.enabledDescription"),
      });
    } else {
      void commands.unregisterCloudSession().catch(e => console.error("cloud session unregistration failed", e));
      toast({
        title: t("toasts.publicSessions.disabledTitle"),
        description: t("toasts.publicSessions.disabledDescription"),
      });
    }
  };

  const toggleAutostart = async (enabled: boolean) => {
    try {
      if (enabled) {
        await enableAutostart();
      } else {
        await disableAutostart();
      }
      setAutostartEnabled(enabled);
      toast({
        title: enabled ? t("toasts.autostart.enabledTitle") : t("toasts.autostart.disabledTitle"),
        description: enabled ? t("toasts.autostart.enabledDescription") : t("toasts.autostart.disabledDescription"),
      });
    } catch (e) {
      setAutostartEnabled(await isAutostartEnabled().catch(() => !enabled));
      toast({
        title: t("toasts.autostart.failureTitle"),
        description: t("toasts.autostart.failureDescription"),
      });
    }
  };

  const toggleGpuEncode = async (disabled: boolean) => {
    setDisableGpuEncode(disabled);
    try {
      await commands.setDisableGpuEncode(disabled);
      await updateConfig({ disableGpuEncode: disabled });
      await flushConfig();
    } catch {
      setDisableGpuEncode(await commands.getDisableGpuEncode().catch(() => !disabled));
      toast({
        variant: "destructive",
        title: t("toasts.gpuEncoding.failureTitle"),
        description: t("toasts.gpuEncoding.failureDescription"),
      });
      return;
    }
    toast({
      title: disabled ? t("toasts.gpuEncoding.disabledTitle") : t("toasts.gpuEncoding.enabledTitle"),
      description: disabled ? t("toasts.gpuEncoding.disabledDescription") : t("toasts.gpuEncoding.enabledDescription"),
    });
  };

  const saveServerPorts = async () => {
    const http = Math.round(Number(httpPort));
    const https = Math.round(Number(httpsPort));
    const isValid = (p: number) => Number.isInteger(p) && p >= 1 && p <= 65535;
    if (!isValid(http) || !isValid(https) || http === https) {
      setHttpPort(oldHttpPort);
      setHttpsPort(oldHttpsPort);
      toast({
        title: t("toasts.invalidPorts.title"),
        description: t("toasts.invalidPorts.description"),
      });
      return;
    }
    if (String(http) === oldHttpPort && String(https) === oldHttpsPort) return;
    let applied: ServerPorts;
    try {
      applied = await commands.setServerPorts(http, https);
    } catch {
      setHttpPort(oldHttpPort);
      setHttpsPort(oldHttpsPort);
      toast({
        variant: "destructive",
        title: t("toasts.serverPorts.failureTitle"),
        description: t("toasts.serverPorts.failureDescription"),
      });
      return;
    }
    setHttpPort(String(applied.http));
    setHttpsPort(String(applied.https));
    if (sessionId) setQrValues(await buildQrValues(sessionId, applied.http));
    try {
      await updateConfig({ serverPorts: { http: applied.http, https: applied.https } });
      await flushConfig();
    } catch {
      toast({
        variant: "destructive",
        title: t("toasts.config.flushFailedTitle"),
        description: t("toasts.config.flushFailedDescription"),
      });
      return;
    }
    setOldHttpPort(String(applied.http));
    setOldHttpsPort(String(applied.https));
    toast({
      title: t("toasts.serverPorts.title"),
      description: t("toasts.serverPorts.description", { http: applied.http, https: applied.https }),
    });
  };

  useEffect(() => {
    if (spin) {
      const timer = setTimeout(() => {
        setSpin(false);
        setOtp(generateOtp());
      }, 500);
      return () => clearTimeout(timer);
    }
  }, [spin]);

  useEffect(() => {
    if (!configLoaded || !configLoadOk.current) return;
    updateConfig({hostedNetworkCredentials: {name: hostedNetworkName, password: hostedNetworkPassword}})
      .then(() => { credentialsSaveToastShown.current = false; })
      .catch(e => {
        console.error("failed to persist hosted network credentials", e);
        if (credentialsSaveToastShown.current) return;
        credentialsSaveToastShown.current = true;
        toast({
          variant: "destructive",
          title: t("toasts.settings.saveFailedTitle"),
          description: t("toasts.settings.saveFailedDescription"),
        });
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostedNetworkName, hostedNetworkPassword, configLoaded]);

  useEffect(() => {
    if (!hostedNetworkOn) setWifiQrModalOpen(false);
  }, [hostedNetworkOn]);

  const startNetworkWithFeedback = async (opts?: { fromWifiModal?: boolean }): Promise<boolean> => {
    let success = false;
    try {
      await commands.stopHostedNetwork();
      success = await commands.startHostedNetwork(hostedNetworkName, hostedNetworkPassword);
    } catch {
      setHostedNetworkOn(false);
      toast({
        variant: "destructive",
        title: t("toasts.networkCreate.failureTitle"),
        description: t("toasts.networkCreate.failureDescription"),
      });
      return false;
    }
    if (success) {
      setHostedNetworkOn(true);
      toast({
        title: t("toasts.networkCreate.successTitle"),
        description: t("toasts.networkCreate.successDescription", { name: hostedNetworkName }),
      });
      return true;
    }
    setHostedNetworkOn(false);
    let wifiOn = true;
    try {
      await commands.stopHostedNetwork();
      wifiOn = await commands.isWifiOn();
    } catch {
      wifiOn = true;
    }
    if (!opts?.fromWifiModal && !wifiOn) {
      setWifiModalOpen(true);
    } else {
      toast({
        title: t("toasts.networkCreate.failureTitle"),
        description: t("toasts.networkCreate.failureDescription"),
      });
    }
    return false;
  };

  const onFileSelected = (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    e.target.value = "";
    if (!file) return;
    setCropSrc(URL.createObjectURL(file));
    setCropOpen(true);
  };

  const closeCrop = () => {
    setCropOpen(false);
    if (cropSrc) URL.revokeObjectURL(cropSrc);
    setCropSrc(null);
  };

  const handleCropSave = async (bytes: Uint8Array, dataUrl: string) => {
    const ok = await saveAvatar(bytes);
    if (!ok) {
      toast({
        title: t("toasts.avatar.saveFailedTitle"),
        description: t("toasts.avatar.saveFailedDescription"),
      });
      return;
    }
    setAvatar(dataUrl);
    closeCrop();
    toast({
      title: t("toasts.avatar.updatedTitle"),
      description: t("toasts.avatar.updatedDescription"),
    });
  };

  const handleRemoveAvatar = async () => {
    const ok = await clearAvatar();
    if (!ok) {
      toast({
        title: t("toasts.avatar.removeFailedTitle"),
        description: t("toasts.avatar.removeFailedDescription"),
      });
      return;
    }
    setAvatar(null);
    toast({
      title: t("toasts.avatar.removedTitle"),
      description: t("toasts.avatar.removedDescription"),
    });
  };

  if (!configLoaded) return <Layout><></></Layout>;

  return (
    <Layout>
      <div className="p-8">
        <div className="mb-6">
          <h2 className="text-2xl font-semibold">Settings</h2>
        </div>
        <div className="mb-4">
          <Card>
            <CardHeader>
              <CardTitle className="flex flex-row items-center">
                Session OTP
                <Button variant="ghost" className="ml-2 w-7 h-7 p-0">
                  <RefreshCw
                    className={cn(
                      "cursor-pointer transition-transform",
                      spin ? "animate-spin pointer-events-none" : ""
                    )}
                    onClick={() => {
                      setSpin(true);
                    }}
                    size={15}
                    style={{ animationDuration: "500ms" }}
                  />
                </Button>
              </CardTitle>
              <InputOTP
                maxLength={6}
                value={otp}
                containerClassName={
                  spin ? "opacity-50" : "opacity-100"
                }
                disabled
              >
                <InputOTPGroup>
                  <InputOTPSlot index={0} />
                  <InputOTPSlot index={1} />
                  <InputOTPSlot index={2} />
                  <InputOTPSlot index={3} />
                  <InputOTPSlot index={4} />
                  <InputOTPSlot index={5} />
                </InputOTPGroup>
              </InputOTP>
            </CardHeader>
          </Card>
        </div>
        <div className="mb-4">
          <Card>
            <CardHeader>
              <CardTitle className="flex flex-row items-center">
                Public Internet Sessions
              </CardTitle>
            </CardHeader>
            <CardContent>
              <div className="flex items-center space-x-4 p-3 px-0">
                <div className="flex-1 space-y-1">
                  <p className="text-sm font-medium leading-none">
                    {publicSessionsEnabled ? "Public sessions enabled" : "Public sessions disabled"}
                  </p>
                  <p className="text-sm text-muted-foreground">
                    Allow devices to join over the internet via the "Anywhere (Internet)" option using session.screenextend.app.
                  </p>
                </div>
                <Switch
                  checked={publicSessionsEnabled}
                  onCheckedChange={(checked) => void togglePublicSessions(checked)}
                />
              </div>
            </CardContent>
          </Card>
        </div>
        <div className="mb-4">
          <Card>
            <CardHeader>
              <CardTitle className="flex flex-row items-center">
                Launch at Startup
              </CardTitle>
            </CardHeader>
            <CardContent>
              <div className="flex items-center space-x-4 p-3 px-0">
                <div className="flex-1 space-y-1">
                  <p className="text-sm font-medium leading-none">
                    {autostartEnabled ? "Start on login enabled" : "Start on login disabled"}
                  </p>
                  <p className="text-sm text-muted-foreground">
                    Automatically open ScreenExtend when you sign in to your computer.
                  </p>
                </div>
                <Switch
                  checked={autostartEnabled}
                  onCheckedChange={(checked) => void toggleAutostart(checked)}
                />
              </div>
            </CardContent>
          </Card>
        </div>
        <div className="mb-4">
          <Card>
            <CardHeader>
              <div>
                <CardTitle>Create Hosted Network</CardTitle>
                <p className="text-sm text-muted-foreground mt-1">
                  Host a local network for devices to join, useful for speed or when no other network is available.
                </p>
              </div>
            </CardHeader>
            <CardContent>
              <div className="flex items-center space-x-4 border-b p-3 px-0 pt-0">
                <div className="flex-1 space-y-1">
                  <p className="text-sm font-medium leading-none">
                    {hostedNetworkOn ? "Stop Network" : "Start Network"}
                  </p>
                </div>
                <Switch
                  checked={hostedNetworkOn}
                  onCheckedChange={async () => {
                    if ((!hostedNetworkOn || inputDisabled)) {
                      await startNetworkWithFeedback();
                    } else {
                      let stopped = false;
                      try {
                        stopped = await commands.stopHostedNetwork();
                      } catch {
                        stopped = false;
                      }
                      if (!stopped) {
                        setHostedNetworkOn(true);
                        toast({
                          variant: "destructive",
                          title: t("toasts.networkStop.failureTitle"),
                          description: t("toasts.networkStop.failureDescription"),
                        });
                        return;
                      }
                      setHostedNetworkName(oldHostedNetworkName);
                      setHostedNetworkPassword(oldHostedNetworkPassword);
                      setShowHostedNetworkPassword(false);
                      setHostedNetworkOn(false);
                      setCredentialsUnlocked(false);
                      toast({
                        title: t("toasts.networkStop.title"),
                        description: t("toasts.networkStop.description"),
                      });
                    }
                  }}
                />
              </div>
              <div
                className={cn(
                  "flex items-center space-x-4 p-3 px-0 mt-2",
                  networkFieldsDisabled && "cursor-not-allowed select-none",
                  "mb-2"
                )}
              >
                <div className="relative outline-none flex-1">
                  <Input
                    type="text"
                    placeholder="Network Name"
                    className="outline-none"
                    value={hostedNetworkName}
                    disabled={networkFieldsDisabled}
                    onChange={handleNetworkNameChange}
                    onBlur={() => setHostedNetworkName(hostedNetworkName.trim())}
                    hoverLabel={true}
                  />
                </div>
                <div className="relative outline-none flex-1">
                  <Input
                    type={showHostedNetworkPassword ? "text" : "password"}
                    placeholder="Network Password"
                    className={cn(
                      "outline-none",
                      hostedNetworkPassword.length < MIN_HOSTED_NETWORK_PASSWORD_LENGTH && "border-red-500 focus:ring-red-500"
                    )}
                    value={hostedNetworkPassword}
                    disabled={networkFieldsDisabled}
                    onChange={event => setHostedNetworkPassword(event.target.value)}
                    minLength={MIN_HOSTED_NETWORK_PASSWORD_LENGTH}
                    maxLength={63}
                    hoverLabel={true}
                  />
                  <div
                    className={cn(
                      "absolute top-0 bottom-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      networkFieldsDisabled && "cursor-not-allowed select-none"
                    )}
                  >
                    {showHostedNetworkPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        style={{ opacity: networkFieldsDisabled ? 0.5 : 1 }}
                        onClick={() => togglePasswordVisibility()}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        style={{ opacity: networkFieldsDisabled ? 0.5 : 1 }}
                        onClick={() => togglePasswordVisibility()}
                      />
                    )}
                  </div>
                  <p className="text-red-500 text-xs mt-1" style={{ position: "absolute", display: (hostedNetworkPassword.length < MIN_HOSTED_NETWORK_PASSWORD_LENGTH ? "initial": "none") }}>A password must have at least {MIN_HOSTED_NETWORK_PASSWORD_LENGTH} characters</p>
                </div>
                <Button disabled={networkFieldsDisabled || hostedNetworkPassword.length < MIN_HOSTED_NETWORK_PASSWORD_LENGTH} onClick={async () => {
                    if (hostedNetworkName !== oldHostedNetworkName || hostedNetworkPassword !== oldHostedNetworkPassword) {
                      if (!(await getConfig().catch(() => undefined))?.dontShowAgain.editNetwork) {
                        setDisabled(false);
                        setHostedNetworkModalOpen(true);
                      } else {
                        setInputDisabled(true);
                        let success = false;
                        try {
                          await commands.stopHostedNetwork();
                          success = await commands.startHostedNetwork(hostedNetworkName, hostedNetworkPassword);
                        } catch {
                          success = false;
                        } finally {
                          setInputDisabled(false);
                        }
                        if (success) {
                          setHostedNetworkOn(true);
                          setCredentialsUnlocked(false);
                          setOldHostedNetworkName(hostedNetworkName);
                          setOldHostedNetworkPassword(hostedNetworkPassword);
                          toast({
                            title: t("toasts.networkSettings.successTitle"),
                            description: t("toasts.networkSettings.successDescription"),
                          });
                        } else {
                          setHostedNetworkOn(false);
                          setCredentialsUnlocked(true);
                          setHostedNetworkName(oldHostedNetworkName);
                          setHostedNetworkPassword(oldHostedNetworkPassword);
                          toast({
                            title: t("toasts.networkSettings.failureTitle"),
                            description: t("toasts.networkSettings.failureDescription"),
                          });
                        }
                      }
                    }
                  }}
                >
                  Save Settings
                </Button>
              </div>
              {SUPPORTS_WIFI_QR && hostedNetworkOn && (
                <div className="flex items-center space-x-4 border-t p-4 px-0 pb-0">
                  <div className="flex-1 space-y-1">
                    <p className="text-sm font-medium leading-none">Connect with QR Code</p>
                  </div>
                  <Button variant="outline" onClick={() => setWifiQrModalOpen(true)}>
                    <QrCode className="mr-2 h-4 w-4" />
                    Show QR Code
                  </Button>
                </div>
              )}
            </CardContent>
          </Card>
        </div>
        <div className="mb-4">
          <Card>
            <CardHeader>
              <div>
                <CardTitle>Device Disconnect Timeout</CardTitle>
                <p className="text-sm text-muted-foreground mt-1">
                  How long a disconnected device's virtual display is kept before being removed.
                </p>
              </div>
            </CardHeader>
            <CardContent>
              <div className="flex items-center space-x-4 p-3 px-0">
                <div className="relative outline-none flex-1">
                  <Input
                    type="number"
                    placeholder="Timeout (seconds)"
                    className="outline-none"
                    value={disconnectGrace}
                    min={0}
                    max={600}
                    onChange={event => setDisconnectGrace(event.target.value)}
                    onBlur={() => {
                      if (!Number.isFinite(Number(disconnectGrace)) || disconnectGrace.trim() === "") {
                        setDisconnectGrace(oldDisconnectGrace);
                      }
                    }}
                    hoverLabel={true}
                  />
                </div>
                <Button onClick={() => void saveDisconnectGrace()}>
                  Save Timeout
                </Button>
              </div>
            </CardContent>
          </Card>
        </div>
        <div className="mb-4">
          <Card>
            <CardHeader>
              <div>
                <CardTitle>Past Devices</CardTitle>
                <p className="text-sm text-muted-foreground mt-1">
                  Every device that has connected to this PC. These devices rejoin automatically without entering the code. Forget a device to require the code again, or ban it to disconnect and block it by its IP address.
                </p>
              </div>
            </CardHeader>
            <CardContent>
              <div className="flex items-center space-x-4 p-3 px-0 pt-0">
                <div className="relative outline-none flex-1">
                  <Input
                    type="text"
                    placeholder="IP address to ban (e.g. 192.168.1.42)"
                    className="outline-none"
                    value={banIpInput}
                    onChange={event => setBanIpInput(event.target.value)}
                    onKeyDown={event => { if (event.key === "Enter") void banManualIp(); }}
                    hoverLabel={true}
                  />
                </div>
                <Button variant="outline" onClick={() => void banManualIp()}>
                  <Ban className="mr-2 h-4 w-4" />
                  Ban IP
                </Button>
              </div>
              {sortedKnownDevices.length > 0 ? (
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead className="w-[160px]">Device Name</TableHead>
                      <TableHead>OS</TableHead>
                      <TableHead>IP</TableHead>
                      <TableHead>Last Seen</TableHead>
                      <TableHead></TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody className="border-t">
                    {sortedKnownDevices.map(device => (
                      <TableRow key={device.ip} className={device.banned ? "opacity-60" : ""}>
                        <TableCell className="font-medium">
                          <div className="flex items-center space-x-2">
                            <span>{device.name || "Unknown device"}</span>
                            {isConnected(device.ip) && (
                              <span className="inline-flex items-center space-x-1 text-xs text-green-600">
                                <span className="h-1.5 w-1.5 rounded-full bg-green-500" />
                                Connected
                              </span>
                            )}
                            {!device.banned && !isConnected(device.ip) && (
                              <span className="text-xs text-muted-foreground">Auto-join</span>
                            )}
                            {device.banned && (
                              <span className="text-xs font-medium text-red-500">Banned</span>
                            )}
                          </div>
                        </TableCell>
                        <TableCell>{device.os || "—"}</TableCell>
                        <TableCell>{device.ip}</TableCell>
                        <TableCell className="text-muted-foreground">{formatLastSeen(device.lastSeen)}</TableCell>
                        <TableCell className="text-right">
                          <div className="flex items-center justify-end space-x-2">
                            <Button
                              variant="outline"
                              size="sm"
                              className={device.banned ? "" : "text-red-600 hover:text-red-700"}
                              onClick={() => void applyBan(device, !device.banned)}
                            >
                              {device.banned ? "Unban" : "Ban"}
                            </Button>
                            <Button
                              variant="ghost"
                              size="icon"
                              aria-label="Forget device"
                              title="Forget device"
                              onClick={() => void forgetDevice(device)}
                            >
                              <Trash2 className="h-4 w-4 text-muted-foreground" />
                            </Button>
                          </div>
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              ) : (
                <p className="text-sm text-muted-foreground py-2">
                  No devices have connected yet.
                </p>
              )}
            </CardContent>
          </Card>
        </div>
        <div className="">
          <Card>
            <CardHeader>
              <CardTitle>Account Settings</CardTitle>
            </CardHeader>
            <CardContent>
              <input
                ref={fileInputRef}
                type="file"
                accept="image/*"
                className="hidden"
                onChange={onFileSelected}
              />
              <div className="flex items-center space-x-5 px-0">
                <div className="flex flex-col items-center space-y-1.5">
                  <button
                    type="button"
                    onClick={() => fileInputRef.current?.click()}
                    className="group relative rounded-full focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
                    aria-label="Change profile picture"
                    title="Change profile picture"
                  >
                    <AvatarWrapper className="h-16 w-16 border">
                      <img
                        src={avatar ?? defaultLogo}
                        alt="Profile"
                        className="h-full w-full object-cover"
                      />
                    </AvatarWrapper>
                    <span className="absolute inset-0 flex items-center justify-center rounded-full bg-black/50 opacity-0 transition-opacity group-hover:opacity-100">
                      <Camera className="h-5 w-5 text-white" />
                    </span>
                    <span className="absolute bottom-0 right-0 flex h-6 w-6 items-center justify-center rounded-full border-2 border-background bg-primary text-primary-foreground shadow-sm">
                      <Camera className="h-3 w-3" />
                    </span>
                  </button>
                  <button
                    type="button"
                    onClick={() => (avatar ? void handleRemoveAvatar() : fileInputRef.current?.click())}
                    className="text-xs text-muted-foreground hover:text-foreground"
                  >
                    {avatar ? "Remove" : "Change photo"}
                  </button>
                </div>
                <div className="ml-4 relative outline-none flex-1">
                  <Input
                    type="text"
                    placeholder="Name"
                    className="outline-none"
                    value={accountName}
                    onChange={event => setAccountName(event.target.value)}
                    maxLength={19}
                    hoverLabel={true}
                  />
                </div>
                <Button onClick={async () => {
                  const trimmed = accountName.trim();
                  try {
                    const current = await getConfig();
                    if (trimmed.length === 0) {
                      setAccountName(current?.name ?? "");
                      return;
                    }
                    if (current?.name !== trimmed) {
                      setAccountName(trimmed);
                      await updateConfig({ name: trimmed });
                      toast({
                        title: t("toasts.account.updatedTitle"),
                        description: t("toasts.account.updatedDescription"),
                      });
                    }
                  } catch {
                    toast({
                      variant: "destructive",
                      title: t("toasts.account.updateFailedTitle"),
                      description: t("toasts.account.updateFailedDescription"),
                    });
                  }
                }} className="ml-4">
                  Save Name
                </Button>
              </div>
            </CardContent>
          </Card>
        </div>
        <div className="mt-4">
          <Card>
            <CardHeader>
              <div>
                <CardTitle>Zoom</CardTitle>
                <p className="text-sm text-muted-foreground mt-1">
                  Scale the entire interface. You can also use Ctrl and + or − to adjust it, and Ctrl 0 to reset.
                </p>
              </div>
            </CardHeader>
            <CardContent>
              <div className="flex items-center space-x-3 px-0">
                <Button
                  variant="outline"
                  size="icon"
                  aria-label="Zoom out"
                  disabled={zoom <= MIN_ZOOM}
                  onClick={() => setZoom(z => zoomOut(z))}
                >
                  <Minus className="h-4 w-4" />
                </Button>
                <span className="w-16 text-center text-sm font-medium tabular-nums">
                  {formatZoom(zoom)}
                </span>
                <Button
                  variant="outline"
                  size="icon"
                  aria-label="Zoom in"
                  disabled={zoom >= MAX_ZOOM}
                  onClick={() => setZoom(z => zoomIn(z))}
                >
                  <Plus className="h-4 w-4" />
                </Button>
                <div className={"ml-1" + (zoom === DEFAULT_ZOOM) ? "cursor-not-allowed select-none" : ""}>
                  <Button
                    variant="ghost"
                    disabled={zoom === DEFAULT_ZOOM}
                    onClick={() => setZoom(DEFAULT_ZOOM)}
                  >
                    <RotateCcw className="mr-2 h-4 w-4" />
                    Reset
                  </Button>
                </div>
              </div>
            </CardContent>
          </Card>
        </div>
        <div className="mt-6">
          <button
            type="button"
            onClick={() => setAdvancedOpen((o) => !o)}
            aria-expanded={advancedOpen}
            className="flex w-full items-center justify-between rounded-md px-4 py-4 text-left transition-colors hover:bg-accent"
          >
            <div>
              <h3 className="text-lg font-semibold">Advanced</h3>
              <p className="text-sm text-muted-foreground mt-1">
                TURN relay, server ports, the raw configuration file, and application logs.
              </p>
            </div>
            <ChevronDown
              className={cn(
                "h-5 w-5 shrink-0 text-muted-foreground transition-transform",
                advancedOpen && "rotate-180"
              )}
            />
          </button>
          {advancedOpen && (
            <div className="mt-4">
              <div className="mb-4">
                <Card>
                  <CardHeader>
                    <div>
                      <CardTitle>TURN Server</CardTitle>
                      <p className="text-sm text-muted-foreground mt-1">
                        A TURN server relays video when two devices are on different networks and can't connect directly. Free TURN providers include Metered, Twilio, or Cloudflare.
                      </p>
                    </div>
                  </CardHeader>
                  <CardContent>
                    <div className="flex items-center space-x-4 p-3 px-0">
                      <div className="relative outline-none flex-1">
                        <Input
                          type="text"
                          placeholder="turn:turn.example.com:3478"
                          className="outline-none"
                          value={turnUrls}
                          onChange={event => setTurnUrls(event.target.value)}
                          hoverLabel={true}
                        />
                      </div>
                    </div>
                    <div className="flex items-center space-x-4 p-3 px-0">
                      <div className="relative outline-none flex-1">
                        <Input
                          type="text"
                          placeholder="Username"
                          className="outline-none"
                          value={turnUsername}
                          onChange={event => setTurnUsername(event.target.value)}
                          hoverLabel={true}
                        />
                      </div>
                      <div className="relative outline-none flex-1">
                        <Input
                          type={showTurnCredential ? "text" : "password"}
                          placeholder="Credential"
                          className="outline-none"
                          value={turnCredential}
                          onChange={event => setTurnCredential(event.target.value)}
                          hoverLabel={true}
                        />
                        <div className="absolute top-0 bottom-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer">
                          {showTurnCredential ? (
                            <EyeOff className="h-5 w-5" onClick={() => setShowTurnCredential(false)} />
                          ) : (
                            <Eye className="h-5 w-5" onClick={() => setShowTurnCredential(true)} />
                          )}
                        </div>
                      </div>
                      <Button onClick={() => void saveTurnConfig()}>
                        Save TURN
                      </Button>
                    </div>
                  </CardContent>
                </Card>
              </div>
              <div className="mb-4">
                <Card>
                  <CardHeader>
                    <div>
                      <CardTitle>Server Ports</CardTitle>
                      <p className="text-sm text-muted-foreground mt-1">
                        The TCP ports the local-network server listens on for device connections. Change these if another app already uses 8080/8443. Connected devices must rejoin with the updated link after a change.
                      </p>
                    </div>
                  </CardHeader>
                  <CardContent>
                    <div className="flex items-center space-x-4 p-3 px-0">
                      <div className="relative outline-none flex-1">
                        <Input
                          type="number"
                          placeholder="HTTP Port"
                          className="outline-none"
                          value={httpPort}
                          min={1}
                          max={65535}
                          onChange={event => setHttpPort(event.target.value)}
                          onBlur={() => { if (!/^\d+$/.test(httpPort.trim())) setHttpPort(oldHttpPort); }}
                          hoverLabel={true}
                        />
                      </div>
                      <div className="relative outline-none flex-1">
                        <Input
                          type="number"
                          placeholder="HTTPS Port"
                          className="outline-none"
                          value={httpsPort}
                          min={1}
                          max={65535}
                          onChange={event => setHttpsPort(event.target.value)}
                          onBlur={() => { if (!/^\d+$/.test(httpsPort.trim())) setHttpsPort(oldHttpsPort); }}
                          hoverLabel={true}
                        />
                      </div>
                      <Button onClick={() => void saveServerPorts()}>
                        Save Ports
                      </Button>
                    </div>
                  </CardContent>
                </Card>
              </div>
              {IS_WINDOWS && (
                <div className="mb-4">
                  <Card>
                    <CardHeader>
                      <div>
                        <CardTitle>Video Encoding</CardTitle>
                        <p className="text-sm text-muted-foreground mt-1">
                          Screen video is normally encoded by your GPU (NVIDIA, Intel, or AMD), which is fast and light on the CPU.
                        </p>
                      </div>
                    </CardHeader>
                    <CardContent>
                      <div className="flex items-center space-x-4 p-3 px-0">
                        <div className="flex-1 space-y-1">
                          <p className="text-sm font-medium leading-none">
                            Disable GPU video encoding
                          </p>
                          <p className="text-sm text-muted-foreground">
                            Not recommended. Forces slower CPU-only (software) encoding, which raises CPU usage and can reduce quality or frame rate. Leave this off unless hardware encoding is broken or unavailable. Changing it reconnects active devices.
                          </p>
                        </div>
                        <Switch
                          checked={disableGpuEncode}
                          onCheckedChange={(checked) => void toggleGpuEncode(checked)}
                        />
                      </div>
                    </CardContent>
                  </Card>
                </div>
              )}
              <div className="mb-4">
                <Card>
                  <CardHeader>
                    <div>
                      <CardTitle>Configuration File</CardTitle>
                      <p className="text-sm text-muted-foreground mt-1">
                        Edit config.json directly. Hover a key for its description and allowed values, or press Ctrl+Space for suggestions. Saving validates the whole document and applies it immediately; nothing is written unless it is valid.
                      </p>
                    </div>
                  </CardHeader>
                  <CardContent>
                    <Suspense
                      fallback={
                        <div className="flex h-[420px] items-center justify-center rounded-md border text-sm text-muted-foreground">
                          {t("configEditor.loading")}
                        </div>
                      }
                    >
                      <ConfigJsonEditor
                        minHostedNetworkPasswordLength={MIN_HOSTED_NETWORK_PASSWORD_LENGTH}
                        onDirtyChange={setConfigDirty}
                        onSaved={handleConfigSaved}
                      />
                    </Suspense>
                  </CardContent>
                </Card>
              </div>
              <div>
                <Card>
                  <CardHeader>
                    <CardTitle>Logs</CardTitle>
                  </CardHeader>
                  <CardContent>
                    <LogTerminal />
                  </CardContent>
                </Card>
              </div>
            </div>
          )}
        </div>
      </div>
      <AlertDialog open={hostedNetworkModalOpen}>
        <AlertDialogTrigger asChild>
          {t("dialogs.editNetwork.trigger")}
        </AlertDialogTrigger>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("dialogs.editNetwork.title")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("dialogs.editNetwork.description")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <div className="flex items-center space-x-2 mb-4">
            <Checkbox
              id="dontShowAgain"
              checked={dontShowAgain}
              onCheckedChange={checked => setDontShowAgain(checked === true)}
            />
            <label
              htmlFor="dontShowAgain"
              className="text-sm text-muted-foreground cursor-pointer"
            >
              {t("common.dontShowAgain")}
            </label>
          </div>
          <AlertDialogFooter>
            <AlertDialogCancel onClick={async () => {
                setDisabled(true);
                try {
                  const current = await getConfig();
                  if (current) {
                    await updateConfig({dontShowAgain: {...current.dontShowAgain, editNetwork: dontShowAgain}});
                  }
                } catch (e) {
                  console.error("failed to persist edit network dialog choice", e);
                } finally {
                  setHostedNetworkName(oldHostedNetworkName);
                  setHostedNetworkPassword(oldHostedNetworkPassword);
                  setHostedNetworkModalOpen(false);
                  setDisabled(false);
                }
              }}
              disabled={disabled}
              className="disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
            >
              {t("common.cancel")}
            </AlertDialogCancel>
            <AlertDialogAction
              className="bg-red-600 hover:bg-red-700 text-white disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
              disabled={disabled}
              onClick={async () => {
                setDisabled(true);
                let success = false;
                try {
                  await commands.stopHostedNetwork();
                  success = await commands.startHostedNetwork(hostedNetworkName, hostedNetworkPassword);
                } catch {
                  success = false;
                } finally {
                  setHostedNetworkModalOpen(false);
                  setDisabled(false);
                }
                try {
                  const current = await getConfig();
                  if (current) {
                    await updateConfig({dontShowAgain: {...current.dontShowAgain, editNetwork: dontShowAgain}});
                  }
                } catch (e) {
                  console.error("failed to persist edit network dialog choice", e);
                }
                if (success) {
                  setHostedNetworkOn(true);
                  setCredentialsUnlocked(false);
                  setOldHostedNetworkName(hostedNetworkName);
                  setOldHostedNetworkPassword(hostedNetworkPassword);
                  toast({
                    title: t("toasts.networkSettings.successTitle"),
                    description: t("toasts.networkSettings.successDescription"),
                  });
                } else {
                  setHostedNetworkOn(false);
                  setCredentialsUnlocked(true);
                  setHostedNetworkName(oldHostedNetworkName);
                  setHostedNetworkPassword(oldHostedNetworkPassword);
                  toast({
                    title: t("toasts.networkSettings.failureTitle"),
                    description: t("toasts.networkSettings.failureDescription"),
                  });
                }
              }}
            >
              {t("common.continue")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={wifiModalOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("dialogs.turnOnWifi.title")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("dialogs.turnOnWifi.description")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel
              onClick={() => setWifiModalOpen(false)}
              disabled={wifiTurningOn}
              className="disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
            >
              {t("common.cancel")}
            </AlertDialogCancel>
            <AlertDialogAction
              disabled={wifiTurningOn}
              className="disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
              onClick={async () => {
                setWifiTurningOn(true);
                try {
                  let turned = false;
                  try {
                    turned = await commands.turnOnWifi();
                  } catch {
                    turned = false;
                  }
                  if (turned) {
                    await new Promise(resolve => setTimeout(resolve, 5000));
                    await startNetworkWithFeedback({ fromWifiModal: true });
                  } else {
                    toast({
                      title: t("toasts.wifi.turnOnFailedTitle"),
                      description: t("toasts.wifi.turnOnFailedDescription"),
                    });
                  }
                } catch (e) {
                  console.error("wi-fi turn-on flow failed", e);
                } finally {
                  setWifiTurningOn(false);
                  setWifiModalOpen(false);
                }
              }}
            >
              {wifiTurningOn ? t("dialogs.turnOnWifi.turningOn") : t("dialogs.turnOnWifi.turnOn")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={wifiQrModalOpen} onOpenChange={setWifiQrModalOpen}>
        <AlertDialogContent className="max-w-sm">
          <AlertDialogHeader>
            <AlertDialogTitle className="text-center">{t("dialogs.wifiQr.title", { name: oldHostedNetworkName })}</AlertDialogTitle>
            <AlertDialogDescription className="text-center">
              {t("dialogs.wifiQr.description")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <div className="rounded-xl bg-white p-4">
            <QRCode
              value={buildWifiQrValue(oldHostedNetworkName, oldHostedNetworkPassword)}
              viewBox="0 0 256 256"
              style={{ width: "100%", height: "auto", display: "block" }}
            />
          </div>
          <div className="space-y-1 text-sm">
            <div className="flex justify-between space-x-4">
              <span className="text-muted-foreground">{t("dialogs.wifiQr.networkLabel")}</span>
              <span className="break-all text-right font-medium">{oldHostedNetworkName}</span>
            </div>
            <div className="flex justify-between space-x-4">
              <span className="text-muted-foreground">{t("dialogs.wifiQr.passwordLabel")}</span>
              <span className="break-all text-right font-medium">{oldHostedNetworkPassword || t("dialogs.wifiQr.noPassword")}</span>
            </div>
          </div>
          <AlertDialogFooter>
            <AlertDialogAction className="w-full hover:opacity-75" onClick={() => setWifiQrModalOpen(false)}>
              {t("common.done")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog
        open={configUnsavedOpen}
        onOpenChange={open => {
          if (open) return;
          setConfigUnsavedOpen(false);
          if (configProceeding.current) {
            configProceeding.current = false;
            return;
          }
          blocker.reset?.();
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("dialogs.configEditorUnsaved.title")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("dialogs.configEditorUnsaved.description")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <div className="flex items-center space-x-2 mb-4">
            <Checkbox
              id="configDontShowAgain"
              checked={configDontShowAgain}
              onCheckedChange={checked => setConfigDontShowAgain(checked === true)}
            />
            <label
              htmlFor="configDontShowAgain"
              className="text-sm text-muted-foreground cursor-pointer"
            >
              {t("common.dontShowAgain")}
            </label>
          </div>
          <AlertDialogFooter>
            <AlertDialogCancel onClick={() => void rememberConfigDialogChoice()}>
              {t("common.cancel")}
            </AlertDialogCancel>
            <AlertDialogAction
              className="bg-red-600 hover:bg-red-700 text-white"
              onClick={async () => {
                configProceeding.current = true;
                setConfigDirty(false);
                blocker.proceed?.();
                await rememberConfigDialogChoice();
              }}
            >
              {t("common.continue")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AvatarCropModal
        open={cropOpen}
        imageSrc={cropSrc}
        onCancel={closeCrop}
        onSave={handleCropSave}
      />
    </Layout>
  );
}
