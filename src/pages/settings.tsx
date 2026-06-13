import React, { useState, useContext, useEffect } from "react";

import Layout from "@/layout/layout";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Eye, EyeOff, Info, RefreshCw } from "lucide-react";
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
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
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

import { AuthProviderContext, updateUser, getUser } from "@/components/auth-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { LogTerminal } from "@/components/log-terminal";
import { useToast } from "@/components/ui/use-toast";
import { commands } from "@/lib/bindings";
import { cn } from "@/lib/utils";

export default function Settings() {
  const { currentUser } = useContext(AuthProviderContext);
  const { windowOtp: [otp, setOtp], windowHostedNetworkOn: [hostedNetworkOn, setHostedNetworkOn] } = useContext(GlobalProviderContext);
  const { toast } = useToast();

  const [spin, setSpin] = useState(false);
  const [hostedNetworkTooltipOpen, setHostedNetworkTooltipOpen] = useState(false);
  const [hostedNetworkName, setHostedNetworkName] = useState("ScreenExtend");
  const [hostedNetworkPassword, setHostedNetworkPassword] = useState("12345678");
  const [oldHostedNetworkName, setOldHostedNetworkName] = useState(hostedNetworkName);
  const [oldHostedNetworkPassword, setOldHostedNetworkPassword] = useState(hostedNetworkPassword);
  const [showHostedNetworkPassword, setShowHostedNetworkPassword] = useState(false);
  const [hostedNetworkModalOpen, setHostedNetworkModalOpen] = useState(false);
  const [disabled, setDisabled] = useState(false);
  const [inputDisabled, setInputDisabled] = useState(false);
  const [dontShowAgain, setDontShowAgain] = useState(true);

  const [_accountPassword, setAccountPassword] = useState("");
  const [_showAccountPassword, setShowAccountPassword] = useState(false);
  const [accountName, setAccountName] = useState("");

  const [disconnectGrace, setDisconnectGrace] = useState("10");
  const [oldDisconnectGrace, setOldDisconnectGrace] = useState("10");
  const [disconnectGraceTooltipOpen, setDisconnectGraceTooltipOpen] = useState(false);

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

  const togglePasswordVisibility = (type: "sessionPassword" | "accountPassword" | "hostedNetworkPassword") => {
    if (type === "accountPassword") {
      if (currentUser === "GUESTGUESTGUESTGUESTGUEST") return;
      setShowAccountPassword(prev => !prev);
    } else {
      if ((!hostedNetworkOn || inputDisabled)) return;
      setShowHostedNetworkPassword(prev => !prev);
    }
  }

  useEffect(() => {
    async function updateText() {
      const user = (await getUser(currentUser))!;
      setHostedNetworkName(user.hostedNetworkCredentials.name);
      setHostedNetworkPassword(user.hostedNetworkCredentials.password);
      setOldHostedNetworkName(hostedNetworkName);
      setOldHostedNetworkPassword(hostedNetworkPassword);
      setAccountPassword(user.password);
      setAccountName(user.name);
    }
    void updateText();
    async function loadDisconnectGrace() {
      const seconds = await commands.getDisconnectGrace();
      setDisconnectGrace(String(seconds));
      setOldDisconnectGrace(String(seconds));
    }
    void loadDisconnectGrace();
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
    void updateUser(currentUser, {hostedNetworkCredentials: {name: hostedNetworkName, password: hostedNetworkPassword}});
  }, [hostedNetworkName, hostedNetworkPassword]);

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
              <CardTitle>Create Hosted Network</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
              <div className="flex items-center space-x-4 border-b p-3 px-0">
                <div className="flex-1 space-y-1">
                  <p className="text-sm font-medium leading-none">
                    {hostedNetworkOn ? "Stop Network" : "Start Network"}
                  </p>
                </div>
                <Switch
                  checked={hostedNetworkOn}
                  onCheckedChange={async () => {
                    if ((!hostedNetworkOn || inputDisabled)) {
                      await commands.stopHostedNetwork();
                      const success = await commands.startHostedNetwork(hostedNetworkName, hostedNetworkPassword);
                      if (success) {
                        setHostedNetworkOn(true);
                        toast({
                          title: "Network Creation Success",
                          description: "The hosted network has successfully been created. Connect other devices to the \"" + hostedNetworkName + "\" Wifi network.",
                        });
                      } else {
                        await commands.stopHostedNetwork();
                        setHostedNetworkOn(false);
                        toast({
                          title: "Network Creation Failure",
                          description: "There was an error in creating the hosted network. Try the action again and ensure no other app is using the Wifi-Direct card, such as hotspot.",
                        });
                      }
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
                <TooltipProvider>
                  <Tooltip delayDuration={100} open={hostedNetworkTooltipOpen} onOpenChange={(state) => setHostedNetworkTooltipOpen(state)}>
                    <TooltipTrigger asChild className="cursor-pointer" onClick={() => setHostedNetworkTooltipOpen(true)}>
                      <Info size={15} />
                    </TooltipTrigger>
                    <TooltipContent className="max-w-[220px]">
                      <p>Host a local network for devices to join, useful for speed or when no other network is available.</p>
                    </TooltipContent>
                  </Tooltip>
                </TooltipProvider>
              </div>
              <div
                className={cn(
                  "flex items-center space-x-4 p-3 px-0",
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
                      hostedNetworkPassword.length < 8 && "border-red-500 focus:ring-red-500"
                    )}
                    value={hostedNetworkPassword}
                    disabled={(!hostedNetworkOn || inputDisabled)}
                    onChange={event => setHostedNetworkPassword(event.target.value)}
                    minLength={8}
                    maxLength={63}
                    hoverLabel={true}
                  />
                  <div
                    className={cn(
                      "absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      (!hostedNetworkOn || inputDisabled) && "cursor-not-allowed select-none"
                    )}
                  >
                    {showHostedNetworkPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        style={{ opacity: hostedNetworkOn ? 1 : 0.5 }}
                        onClick={() => togglePasswordVisibility("hostedNetworkPassword")}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        style={{ opacity: hostedNetworkOn ? 1 : 0.5 }}
                        onClick={() => togglePasswordVisibility("hostedNetworkPassword")}
                      />
                    )}
                  </div>
                  <p className="text-red-500 text-xs mt-1" style={{ position: "absolute", display: (hostedNetworkPassword.length < 8 ? "initial": "none") }}>A password must have at least 8 characters</p>
                </div>
                <Button disabled={(!hostedNetworkOn || inputDisabled) || hostedNetworkPassword.length < 8} onClick={async () => {
                    if (hostedNetworkName !== oldHostedNetworkName || hostedNetworkPassword !== oldHostedNetworkPassword) {
                      if (!(await getUser(currentUser))!.dontShowAgain.editNetwork) {
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
              <CardTitle className="flex flex-row items-center">
                Device Disconnect Timeout
                <TooltipProvider>
                  <Tooltip delayDuration={100} open={disconnectGraceTooltipOpen} onOpenChange={(state) => setDisconnectGraceTooltipOpen(state)}>
                    <TooltipTrigger asChild className="cursor-pointer ml-2" onClick={() => setDisconnectGraceTooltipOpen(true)}>
                      <Info size={15} />
                    </TooltipTrigger>
                    <TooltipContent className="max-w-[260px]">
                      <p>How long a disconnected device's virtual display is kept before being removed.</p>
                    </TooltipContent>
                  </Tooltip>
                </TooltipProvider>
              </CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
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
        <div className="">
          <Card>
            <CardHeader>
              <CardTitle>Account Settings</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
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
                    setAccountName((await getUser(currentUser))!.name);
                    return;
                  }
                  if ((await getUser(currentUser))!.name !== trimmed) {
                    setAccountName(trimmed);
                    await updateUser(currentUser, { name: trimmed });
                    toast({
                      title: "Account Settings Updated",
                      description: "Your name has been updated.",
                    });
                  }
                }}>
                  Save Name
                </Button>
              </div>
              {/*<div
                className={cn(
                  "flex items-center space-x-4 p-3 px-0",
                  currentUser === "GUESTGUESTGUESTGUESTGUEST" && "cursor-not-allowed select-none"
                 )}
              >
                <div className="relative outline-none flex-1">
                  <Input
                    type={showAccountPassword ? "text" : "password"}
                    placeholder="Password"
                    className="outline-none"
                    defaultValue={accountPassword}
                    onChange={event => setAccountPassword(event.target.value)}
                    disabled={currentUser === "GUESTGUESTGUESTGUESTGUEST"}
                    id="changePasswordInput"
                    hoverLabel={true}
                  />
                  <div
                    className={cn(
                      "absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      currentUser === "GUESTGUESTGUESTGUESTGUEST" && "cursor-not-allowed select-none"
                    )}
                  >
                    {showAccountPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        style={{ opacity: currentUser === "GUESTGUESTGUESTGUESTGUEST" ? 0.5 : 1 }}
                        onClick={() => togglePasswordVisibility("accountPassword")}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        style={{ opacity: currentUser === "GUESTGUESTGUESTGUESTGUEST" ? 0.5 : 1 }}
                        onClick={() => togglePasswordVisibility("accountPassword")}
                      />
                    )}
                  </div>
                </div>
                <Button disabled={currentUser === "GUESTGUESTGUESTGUESTGUEST"} onClick={async () => {
                  setShowAccountPassword(false);
                  if ((await getUser(currentUser))!.password !== accountPassword) {
                    await updateUser(currentUser, { password: accountPassword });
                    toast({
                      title: "Account Settings Updated",
                      description: "Your account settings have been updated.",
                    });
                  }
                }}>
                  Save Password
                </Button>
              </div>*/}
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
                await updateUser(currentUser, {dontShowAgain: {...(await getUser(currentUser))!.dontShowAgain, editNetwork: dontShowAgain}});
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
                await updateUser(currentUser, {dontShowAgain: {...(await getUser(currentUser))!.dontShowAgain, editNetwork: dontShowAgain}});
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
    </Layout>
  );
}
