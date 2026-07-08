import React, { useState, useContext, useEffect } from "react";

import Layout from "@/layout/layout";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Eye, EyeOff, RefreshCw } from "lucide-react";
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

import { updateConfig, getConfig, flushConfig, DEFAULT_HTTP_PORT, DEFAULT_HTTPS_PORT } from "@/components/config-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { LogTerminal } from "@/components/log-terminal";
import { useToast } from "@/components/ui/use-toast";
import { commands } from "@/lib/bindings";
import { cn, buildQrValues } from "@/lib/utils";
import { type as getOsType } from "@tauri-apps/plugin-os";

const MIN_HOSTED_NETWORK_PASSWORD_LENGTH = getOsType() === "macos" ? 10 : 8;

export default function Settings() {
  const { windowOtp: [otp, setOtp], windowHostedNetworkOn: [hostedNetworkOn, setHostedNetworkOn], windowSessionId: [sessionId], windowQrValues: [, setQrValues], windowPublicSessionsEnabled: [publicSessionsEnabled, setPublicSessionsEnabled] } = useContext(GlobalProviderContext);
  const { toast } = useToast();

  const [spin, setSpin] = useState(false);
  const [hostedNetworkName, setHostedNetworkName] = useState("");
  const [hostedNetworkPassword, setHostedNetworkPassword] = useState("");
  const [oldHostedNetworkName, setOldHostedNetworkName] = useState("");
  const [oldHostedNetworkPassword, setOldHostedNetworkPassword] = useState("");
  const [showHostedNetworkPassword, setShowHostedNetworkPassword] = useState(false);
  const [hostedNetworkModalOpen, setHostedNetworkModalOpen] = useState(false);
  const [wifiModalOpen, setWifiModalOpen] = useState(false);
  const [wifiTurningOn, setWifiTurningOn] = useState(false);
  const [disabled, setDisabled] = useState(false);
  const [inputDisabled, setInputDisabled] = useState(false);
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

  const togglePasswordVisibility = () => {
    if ((!hostedNetworkOn || inputDisabled)) return;
    setShowHostedNetworkPassword(prev => !prev);
  }

  useEffect(() => {
    async function updateText() {
      const config = (await getConfig())!;
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
      const seconds = await commands.getDisconnectGrace();
      setDisconnectGrace(String(seconds));
      setOldDisconnectGrace(String(seconds));
      setConfigLoaded(true);
    }
    void updateText();
  }, []);

  const saveDisconnectGrace = async () => {
    const parsed = Number(disconnectGrace);
    if (!Number.isFinite(parsed)) {
      setDisconnectGrace(oldDisconnectGrace);
      return;
    }
    const seconds = Math.min(600, Math.max(0, Math.round(parsed)));
    setDisconnectGrace(String(seconds));
    if (String(seconds) === oldDisconnectGrace) return;
    await commands.setDisconnectGrace(seconds);
    localStorage.setItem("disconnectGraceSecs", String(seconds));
    setOldDisconnectGrace(String(seconds));
    toast({
      title: "Disconnect Timeout Updated",
      description: seconds === 0 ? "Displays of disconnected devices will now be removed immediately." : `Disconnected devices now have ${seconds} second${seconds === 1 ? "" : "s"} to reconnect before their display is removed.`,
    });
  };

  const saveTurnConfig = async () => {
    const urls = turnUrls.trim();
    const username = turnUsername.trim();
    const credential = turnCredential.trim();
    setTurnUrls(urls);
    setTurnUsername(username);
    setTurnCredential(credential);
    await commands.setTurnConfig(urls, username, credential);
    await updateConfig({ turnConfig: { urls, username, credential } });
    toast({
      title: urls ? "TURN Server Saved" : "TURN Server Cleared",
      description: urls
        ? "Devices on other networks will now relay through this TURN server. It applies on the next connection."
        : "No TURN server is configured. Connections across different networks may fail.",
    });
  };

  const togglePublicSessions = async (enabled: boolean) => {
    setPublicSessionsEnabled(enabled);
    await updateConfig({ publicSessionsEnabled: enabled });
    await flushConfig();
    if (enabled) {
      if (sessionId) void commands.registerCloudSession(sessionId);
      toast({
        title: "Public Sessions Enabled",
        description: "Devices on other networks can now join over the internet using your session link on the Add Device screen.",
      });
    } else {
      void commands.unregisterCloudSession();
      toast({
        title: "Public Sessions Disabled",
        description: "The \"Anywhere (Internet)\" option is now hidden and this host is no longer reachable through session.screenextend.app.",
      });
    }
  };

  const saveServerPorts = async () => {
    const http = Math.round(Number(httpPort));
    const https = Math.round(Number(httpsPort));
    const isValid = (p: number) => Number.isInteger(p) && p >= 1 && p <= 65535;
    if (!isValid(http) || !isValid(https) || http === https) {
      setHttpPort(oldHttpPort);
      setHttpsPort(oldHttpsPort);
      toast({
        title: "Invalid Ports",
        description: "Enter two different port numbers between 1 and 65535 for HTTP and HTTPS.",
      });
      return;
    }
    if (String(http) === oldHttpPort && String(https) === oldHttpsPort) return;
    const applied = await commands.setServerPorts(http, https);
    setHttpPort(String(applied.http));
    setHttpsPort(String(applied.https));
    setOldHttpPort(String(applied.http));
    setOldHttpsPort(String(applied.https));
    await updateConfig({ serverPorts: { http: applied.http, https: applied.https } });
    await flushConfig();
    if (sessionId) setQrValues(await buildQrValues(sessionId, applied.http));
    toast({
      title: "Server Ports Updated",
      description: `Local network streaming now uses HTTP port ${applied.http} and HTTPS port ${applied.https}. Connected devices were disconnected and must rejoin using the updated links.`,
    });
  };

  useEffect(() => {
    if (spin) {
      const timer = setTimeout(() => {
        setSpin(false);
        setOtp([...Array(6)].reduce(a=>a+"0123456789"[~~(Math.random()*"0123456789".length)], ""));
      }, 500);
      return () => clearTimeout(timer);
    }
  }, [spin]);

  useEffect(() => {
    if (configLoaded) void updateConfig({hostedNetworkCredentials: {name: hostedNetworkName, password: hostedNetworkPassword}});
  }, [hostedNetworkName, hostedNetworkPassword, configLoaded]);

  const startNetworkWithFeedback = async (opts?: { fromWifiModal?: boolean }): Promise<boolean> => {
    await commands.stopHostedNetwork();
    const success = await commands.startHostedNetwork(hostedNetworkName, hostedNetworkPassword);
    if (success) {
      setHostedNetworkOn(true);
      toast({
        title: "Network Creation Success",
        description: "The hosted network has successfully been created. Connect other devices to the \"" + hostedNetworkName + "\" Wifi network.",
      });
      return true;
    }
    await commands.stopHostedNetwork();
    setHostedNetworkOn(false);
    if (!opts?.fromWifiModal && !(await commands.isWifiOn())) {
      setWifiModalOpen(true);
    } else {
      toast({
        title: "Network Creation Failure",
        description: "There was an error in creating the hosted network. Try the action again and ensure no other app is using the Wifi-Direct card, such as hotspot.",
      });
    }
    return false;
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
                      await commands.stopHostedNetwork();
                      setHostedNetworkName(oldHostedNetworkName);
                      setHostedNetworkPassword(oldHostedNetworkPassword);
                      setShowHostedNetworkPassword(false);
                      setHostedNetworkOn(false);
                      toast({
                        title: "Network Stop Success",
                        description: "The hosted network has successfully been stopped. All devices have been disconnected.",
                      });
                    }
                  }}
                />
              </div>
              <div
                className={cn(
                  "flex items-center space-x-4 p-3 px-0 mt-4",
                  (!hostedNetworkOn || inputDisabled) && "cursor-not-allowed select-none"
                )}
              >
                <div className="relative outline-none flex-1">
                  <Input
                    type="text"
                    placeholder="Network Name"
                    className="outline-none"
                    value={hostedNetworkName}
                    disabled={(!hostedNetworkOn || inputDisabled)}
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
                    disabled={(!hostedNetworkOn || inputDisabled)}
                    onChange={event => setHostedNetworkPassword(event.target.value)}
                    minLength={MIN_HOSTED_NETWORK_PASSWORD_LENGTH}
                    maxLength={63}
                    hoverLabel={true}
                  />
                  <div
                    className={cn(
                      "absolute top-0 bottom-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      (!hostedNetworkOn || inputDisabled) && "cursor-not-allowed select-none"
                    )}
                  >
                    {showHostedNetworkPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        style={{ opacity: hostedNetworkOn ? 1 : 0.5 }}
                        onClick={() => togglePasswordVisibility()}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        style={{ opacity: hostedNetworkOn ? 1 : 0.5 }}
                        onClick={() => togglePasswordVisibility()}
                      />
                    )}
                  </div>
                  <p className="text-red-500 text-xs mt-1" style={{ position: "absolute", display: (hostedNetworkPassword.length < MIN_HOSTED_NETWORK_PASSWORD_LENGTH ? "initial": "none") }}>A password must have at least {MIN_HOSTED_NETWORK_PASSWORD_LENGTH} characters</p>
                </div>
                <Button disabled={(!hostedNetworkOn || inputDisabled) || hostedNetworkPassword.length < MIN_HOSTED_NETWORK_PASSWORD_LENGTH} onClick={async () => {
                    if (hostedNetworkName !== oldHostedNetworkName || hostedNetworkPassword !== oldHostedNetworkPassword) {
                      if (!(await getConfig())!.dontShowAgain.editNetwork) {
                        setDisabled(false);
                        setHostedNetworkModalOpen(true);
                      } else {
                        setInputDisabled(true);
                        await commands.stopHostedNetwork();
                        const success = await commands.startHostedNetwork(hostedNetworkName, hostedNetworkPassword);
                        setInputDisabled(false);
                        if (success) {
                          setOldHostedNetworkName(hostedNetworkName);
                          setOldHostedNetworkPassword(hostedNetworkPassword);
                          toast({
                            title: "Network Settings Update Success",
                            description: "The network settings have successfully been updated.",
                          });
                        } else {
                          setHostedNetworkName(oldHostedNetworkName);
                          setHostedNetworkPassword(oldHostedNetworkPassword);
                          toast({
                            title: "Network Settings Update Failure",
                            description: "The was an error in updating the network settings.",
                          });
                        }
                      }
                    }
                  }}
                >
                  Save Settings
                </Button>
              </div>
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
        <div className="">
          <Card>
            <CardHeader>
              <CardTitle>Account Settings</CardTitle>
            </CardHeader>
            <CardContent>
              <div className="flex items-center space-x-4 p-3 px-0">
                <div className="relative outline-none flex-1">
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
                  if (trimmed.length === 0) {
                    setAccountName((await getConfig())!.name);
                    return;
                  }
                  if ((await getConfig())!.name !== trimmed) {
                    setAccountName(trimmed);
                    await updateConfig({ name: trimmed });
                    toast({
                      title: "Account Settings Updated",
                      description: "Your name has been updated.",
                    });
                  }
                }}>
                  Save Name
                </Button>
              </div>
            </CardContent>
          </Card>
        </div>
        <div className="mt-4">
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
      <AlertDialog open={hostedNetworkModalOpen}>
        <AlertDialogTrigger asChild>
          Save Settings
        </AlertDialogTrigger>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Change network settings?</AlertDialogTitle>
            <AlertDialogDescription>
              This action will cause devices on the network to be disconnected. They will need to rejoin the network with the new name and/or password.
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
              Don't show this message again
            </label>
          </div>
          <AlertDialogFooter>
            <AlertDialogCancel onClick={async () => {
                setDisabled(true);
                await updateConfig({dontShowAgain: {...(await getConfig())!.dontShowAgain, editNetwork: dontShowAgain}});
                setHostedNetworkName(oldHostedNetworkName);
                setHostedNetworkPassword(oldHostedNetworkName);
                setHostedNetworkModalOpen(false);
              }}
              disabled={disabled}
              className="disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
            >
              Cancel
            </AlertDialogCancel>
            <AlertDialogAction
              className="bg-red-600 hover:bg-red-700 text-white disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
              disabled={disabled}
              onClick={async () => {
                setDisabled(true);
                await commands.stopHostedNetwork();
                const success = await commands.startHostedNetwork(hostedNetworkName, hostedNetworkPassword);
                setHostedNetworkModalOpen(false);
                await updateConfig({dontShowAgain: {...(await getConfig())!.dontShowAgain, editNetwork: dontShowAgain}});
                if (success) {
                  setOldHostedNetworkName(hostedNetworkName);
                  setOldHostedNetworkPassword(hostedNetworkPassword);
                  toast({
                    title: "Network Settings Update Success",
                    description: "The network settings have successfully been updated.",
                  });
                } else {
                  setHostedNetworkName(oldHostedNetworkName);
                  setHostedNetworkPassword(oldHostedNetworkPassword);
                  toast({
                    title: "Network Settings Update Failure",
                    description: "The was an error in updating the network settings.",
                  });
                }
              }}
            >
              Continue
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog open={wifiModalOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Turn on Wi-Fi?</AlertDialogTitle>
            <AlertDialogDescription>
              Hosting a network requires Wi-Fi to be turned on.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel
              onClick={() => setWifiModalOpen(false)}
              disabled={wifiTurningOn}
              className="disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
            >
              Cancel
            </AlertDialogCancel>
            <AlertDialogAction
              disabled={wifiTurningOn}
              className="disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
              onClick={async () => {
                setWifiTurningOn(true);
                const turned = await commands.turnOnWifi();
                if (turned) {
                  await new Promise(resolve => setTimeout(resolve, 5000));
                  await startNetworkWithFeedback({ fromWifiModal: true });
                } else {
                  toast({
                    title: "Couldn't Turn On Wi-Fi",
                    description: "Please enable Wi-Fi manually from Windows settings, then try again.",
                  });
                }
                setWifiTurningOn(false);
                setWifiModalOpen(false);
              }}
            >
              {wifiTurningOn ? "Turning On…" : "Turn On"}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Layout>
  );
}
