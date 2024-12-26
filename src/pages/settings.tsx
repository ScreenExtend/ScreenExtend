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
import { useToast } from "@/components/ui/use-toast";
import { commands, events } from "@/lib/bindings";
import { cn } from "@/lib/utils";

export default function Settings() {
  const { currentUser } = useContext(AuthProviderContext);
  const { toast } = useToast();

  const characters = "0123456789";
  const [otp, setOtp] = React.useState(/^[A-Z0-9]{6}$/.test(window.otp!) ? window.otp! : [...Array(6)].reduce(a=>a+characters[~~(Math.random()*characters.length)], ""));
  const [spin, setSpin] = useState(false);

  const [hostedNetworkOn, setHostedNetworkOn] = useState(false);
  const [hostedNetworkTooltipOpen, setHostedNetworkTooltipOpen] = useState(false);
  const [hostedNetworkName, setHostedNetworkName] = useState("ScreenExtend");
  const [hostedNetworkPassword, setHostedNetworkPassword] = useState("12345678");
  const [oldHostedNetworkName, setOldHostedNetworkName] = useState(hostedNetworkName);
  const [oldHostedNetworkPassword, setOldHostedNetworkPassword] = useState(hostedNetworkPassword);
  const [showHostedNetworkPassword, setShowHostedNetworkPassword] = useState(false);
  const [hostedNetworkModalOpen, setHostedNetworkModalOpen] = useState(false);
  const [dontShowAgain, setDontShowAgain] = useState(true);

  const [accountPassword, setAccountPassword] = useState("");
  const [showAccountPassword, setShowAccountPassword] = useState(false);

  const handleNetworkNameChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    let value = e.target.value;
    if (!/^[a-zA-Z0-9 ]+$/.test(value)) {
      value = value.replace(/[^a-zA-Z0-9 ]/g, "");
    }
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
      if (currentUser.length === 0) return;
      setShowAccountPassword(prev => !prev);
    } else {
      if (!hostedNetworkOn) return;
      setShowHostedNetworkPassword(prev => !prev);
    }
  }

  const startHostedNetworkWithIP = async (name: string, password: string) => {
    const ips1 = await commands.getPrivateIpAddresses();
    const success = await commands.startHostedNetwork(name, password);
    const ips2 = await commands.getPrivateIpAddresses();
    if (success && ips2.length - ips1.length === 1) {
      const set1 = new Set(ips1);
      await events.hostedUrl.emit(ips2.find(item => !set1.has(item))!);
      await new Promise(events.hostedUrl.once);
      return true;
    } else {
      await events.hostedUrl.emit("stop");
      await new Promise(events.hostedUrl.once);
      return false;
    }
  }

  useEffect(() => {
    setHostedNetworkOn(window.hostedNetworkOn!);
    async function updateText() {
      const user = (await getUser(currentUser))!;
      setHostedNetworkName(user.hostedNetworkCredentials.name);
      setHostedNetworkPassword(user.hostedNetworkCredentials.password);
      setOldHostedNetworkName(hostedNetworkName);
      setOldHostedNetworkPassword(hostedNetworkPassword);
      setAccountPassword(user.password);
    }
    void updateText();
  }, []);

  useEffect(() => {
    if (spin) {
      const timer = setTimeout(() => {
        setSpin(false);
        setOtp([...Array(6)].reduce(a=>a+characters[~~(Math.random()*characters.length)], ""));
      }, 500);
      return () => clearTimeout(timer);
    }
  }, [spin]);

  useEffect(() => {
    void updateUser(currentUser, {hostedNetworkCredentials: {name: hostedNetworkName, password: hostedNetworkPassword}});
  }, [hostedNetworkName, hostedNetworkPassword]);

  useEffect(() => {
    window.otp = otp;
  }, [otp]);

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
                <RefreshCw
                  className={cn(
                    "ml-3 cursor-pointer transition-transform",
                    spin ? "animate-spin pointer-events-none" : ""
                  )}
                  onClick={() => {
                    setSpin(true);
                  }}
                  size={18}
                  style={{ animationDuration: "500ms" }}
                />
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
                    if (!hostedNetworkOn) {
                      await events.hostedUrl.emit("stop");
                      await new Promise(events.hostedUrl.once);
                      const success = await startHostedNetworkWithIP(hostedNetworkName, hostedNetworkPassword);
                      if (success) {
                        setHostedNetworkOn(true);
                        window.hostedNetworkOn = true;
                        toast({
                          title: "Network Creation Success",
                          description: "The hosted network has successfully been created. Connect other devices to the \"" + hostedNetworkName + "\" Wifi network.",
                        });
                      } else {
                        toast({
                          title: "Network Creation Failure",
                          description: "There was an error in creating the hosted network. Try the action again and ensure no other app is using the Wifi-Direct card.",
                        });
                      }
                    } else {
                      window.hostedNetworkOn = false;
                      await events.hostedUrl.emit("stop");
                      await new Promise(events.hostedUrl.once);
                      if (hostedNetworkPassword.length < 8) {
                        setHostedNetworkPassword(oldHostedNetworkPassword);
                      }
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
                    <TooltipContent>
                      <p>You can create a local network that other devices can join. This is useful for speed or if no other networks are available.{"\u00a0\u00a0\u00a0"}</p>
                    </TooltipContent>
                  </Tooltip>
                </TooltipProvider>
              </div>
              <div
                className={cn(
                  "flex items-center space-x-4 p-3 px-0",
                  !hostedNetworkOn && "cursor-not-allowed select-none"
                )}
              >
                <div className="relative outline-none flex-1">
                  <Input
                    type="text"
                    placeholder="Network Name"
                    className="outline-none"
                    value={hostedNetworkName}
                    disabled={!hostedNetworkOn}
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
                    disabled={!hostedNetworkOn}
                    onChange={event => setHostedNetworkPassword(event.target.value)}
                    minLength={8}
                    maxLength={63}
                    hoverLabel={true}
                  />
                  <div
                    className={cn(
                      "absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      !hostedNetworkOn && "cursor-not-allowed select-none"
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
                <Button disabled={!hostedNetworkOn || hostedNetworkPassword.length < 8} onClick={async () => {
                    if (hostedNetworkName !== oldHostedNetworkName || hostedNetworkPassword !== oldHostedNetworkPassword) {
                      if (!(await getUser(currentUser))!.dontShowAgain.editNetwork) {
                        setHostedNetworkModalOpen(true);
                      } else {
                        await events.hostedUrl.emit("stop");
                        await new Promise(events.hostedUrl.once);
                        const success = await startHostedNetworkWithIP(hostedNetworkName, hostedNetworkPassword);
                        if (success) {
                          setOldHostedNetworkName(hostedNetworkName);
                          setOldHostedNetworkPassword(hostedNetworkPassword);
                          toast({
                            title: "Network Settings Update Success",
                            description: "The network settings have successfully been updated.",
                          });
                        } else {
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
        <div className="">
          <Card>
            <CardHeader>
              <CardTitle>Account Settings</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
              <div
                className={cn(
                  "flex items-center space-x-4 p-3 px-0",
                  currentUser.length === 0 && "cursor-not-allowed select-none"
                 )}
              >
                <div className="relative outline-none flex-1">
                  <Input
                    type={showAccountPassword ? "text" : "password"}
                    placeholder="Password"
                    className="outline-none"
                    defaultValue={accountPassword}
                    onChange={event => setAccountPassword(event.target.value)}
                    disabled={currentUser.length === 0}
                    id="changePasswordInput"
                    hoverLabel={true}
                  />
                  <div
                    className={cn(
                      "absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      currentUser.length === 0 && "cursor-not-allowed select-none"
                    )}
                  >
                    {showAccountPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        style={{ opacity: currentUser.length === 0 ? 0.5 : 1 }}
                        onClick={() => togglePasswordVisibility("accountPassword")}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        style={{ opacity: currentUser.length === 0 ? 0.5 : 1 }}
                        onClick={() => togglePasswordVisibility("accountPassword")}
                      />
                    )}
                  </div>
                </div>
                <Button disabled={currentUser.length === 0} onClick={async () => {
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
              </div>
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
                await updateUser(currentUser, {dontShowAgain: {...(await getUser(currentUser))!.dontShowAgain, editNetwork: dontShowAgain}});
                setHostedNetworkName(oldHostedNetworkName);
                setHostedNetworkPassword(oldHostedNetworkName);
                setHostedNetworkModalOpen(false);
              }}
            >
              Cancel
            </AlertDialogCancel>
            <AlertDialogAction
              className="bg-red-600 hover:bg-red-700 text-white"
              onClick={async () => {
                await events.hostedUrl.emit("stop");
                await new Promise(events.hostedUrl.once);
                const success = await startHostedNetworkWithIP(hostedNetworkName, hostedNetworkPassword);
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
