import React, { useState, useContext } from "react";

import Layout from "@/layout/layout";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Eye, EyeOff, Info } from "lucide-react";
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

import { AuthProviderContext } from "@/components/auth-provider";
import { useToast } from "@/components/ui/use-toast";
import { invoke } from "@tauri-apps/api/tauri";
import { cn } from "@/lib/utils";

export default function Settings() {
  const {currentUser, setCurrentUser} = useContext(AuthProviderContext);
  const { toast } = useToast();

  const [sessionPassword, setSessionPassword] = useState("");
  const [showSessionPassword, setShowSessionPassword] = useState(false);
  const [oldSessionPassword, setOldSessionPassword] = useState(sessionPassword);

  const [hostedNetworkOn, setHostedNetworkOn] = useState(false);
  const [hostedNetworkTooltipOpen, setHostedNetworkTooltipOpen] = useState(false);
  const [hostedNetworkName, setHostedNetworkName] = useState("ScreenExtend");
  const [hostedNetworkPassword, setHostedNetworkPassword] = useState("ScreenExtend" + Array.from({length: 5}, () => Math.floor(Math.random() * 10)).join("") + "!");
  const [oldHostedNetworkName, setOldHostedNetworkName] = useState(hostedNetworkName);
  const [oldHostedNetworkPassword, setOldHostedNetworkPassword] = useState(hostedNetworkPassword);
  const [showHostedNetworkPassword, setShowHostedNetworkPassword] = useState(false);
  const [hostedNetworkModalOpen, setHostedNetworkModalOpen] = useState(false);
  const [dontShowAgain, setDontShowAgain] = useState(true);

  const [accountPassword, setAccountPassword] = useState(currentUser.password);
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

  function togglePasswordVisibility(type: "sessionPassword" | "accountPassword" | "hostedNetworkPassword") {
    if (type === "sessionPassword") {
      setShowSessionPassword((prev) => !prev);
    } else if (type === "accountPassword") {
      if (currentUser.username.length === 0) return;
      setShowAccountPassword((prev) => !prev);
    } else {
      if (!hostedNetworkOn) return;
      setShowHostedNetworkPassword((prev) => !prev);
    }
  }

  return (
    <Layout>
      <div className="p-8">
        <div className="mb-6">
          <h2 className="text-2xl font-semibold">Settings</h2>
        </div>
        <div className="mb-4">
          <Card>
            <CardHeader>
              <CardTitle>Session Settings</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
              <div className="flex items-center space-x-4 p-3 px-0">
                <div className="relative outline-none flex-1">
                  <Input
                    type={showSessionPassword ? "text" : "password"}
                    placeholder="Password"
                    className="outline-none"
                    hoverLabel={true}
                    onChange={(e) => setSessionPassword(e.target.value)}
                  />
                  <div className="absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer">
                    {showSessionPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        onClick={() => togglePasswordVisibility("sessionPassword")}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        onClick={() => togglePasswordVisibility("sessionPassword")}
                      />
                    )}
                  </div>
                </div>
                <Button onClick={() => {
                    if (sessionPassword !== oldSessionPassword) {
                      setOldSessionPassword(sessionPassword);
                      toast({
                        title: "Session Settings Updated",
                        description: "Your session settings have been updated.",
                      });
                    }
                  }}
                >Save Password</Button>
              </div>
            </CardContent>
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
                      const success = await invoke("start_hosted_network", {ssid: hostedNetworkName, password: hostedNetworkPassword});
                      if (success) {
                        setHostedNetworkOn(true);
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
                      await invoke("stop_hosted_network");
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
                  !hostedNetworkOn && "cursor-not-allowed"
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
                    onBlur={() => setHostedNetworkName((prev) => prev.trim())}
                    hoverLabel={true}
                  />
                </div>
                <div className="relative outline-none flex-1">
                  <Input
                    type={showHostedNetworkPassword ? "text" : "password"}
                    placeholder="Network Password"
                    className="outline-none"
                    disabled={!hostedNetworkOn}
                    onChange={(e) => setHostedNetworkPassword(e.target.value)}
                    minLength={8}
                    maxLength={63}
                    hoverLabel={true}
                  />
                  <div
                    className={cn(
                      "absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      !hostedNetworkOn && "cursor-not-allowed"
                    )}
                  >
                    {showHostedNetworkPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        onClick={() => togglePasswordVisibility("hostedNetworkPassword")}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        onClick={() => togglePasswordVisibility("hostedNetworkPassword")}
                      />
                    )}
                  </div>
                </div>
                <Button disabled={!hostedNetworkOn} onClick={() => {
                    if (hostedNetworkName !== oldHostedNetworkName || hostedNetworkPassword !== oldHostedNetworkPassword) {
                      setHostedNetworkModalOpen(true);
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
                  currentUser.username.length === 0 && "cursor-not-allowed"
                 )}
              >
                <div className="relative outline-none flex-1">
                  <Input
                    type={showAccountPassword ? "text" : "password"}
                    placeholder="Password"
                    className="outline-none"
                    defaultValue={accountPassword}
                    onChange={(e) => setAccountPassword(e.target.value)}
                    disabled={currentUser.username.length === 0}
                    id="changePasswordInput"
                    hoverLabel={true}
                  />
                  <div
                    className={cn(
                      "absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      currentUser.username.length === 0 && "cursor-not-allowed"
                    )}
                  >
                    {showAccountPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        onClick={() => togglePasswordVisibility("accountPassword")}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        onClick={() => togglePasswordVisibility("accountPassword")}
                      />
                    )}
                  </div>
                </div>
                <Button disabled={currentUser.username.length === 0} onClick={() => {
                  setCurrentUser({username: currentUser.username, password: accountPassword});
                  setShowAccountPassword(false);
                  if (currentUser.password !== accountPassword) {
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
              This action will cause devices on the network to be disconnected. They will need to rejoin the network with the new ssid and/or password.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <div className="flex items-center space-x-2 mb-4">
            <Checkbox
              id="dontShowAgain"
              checked={dontShowAgain}
              onCheckedChange={(checked) => setDontShowAgain(checked === true)}
            />
            <label
              htmlFor="dontShowAgain"
              className="text-sm text-muted-foreground cursor-pointer"
            >
              Don't show this message again
            </label>
          </div>
          <AlertDialogFooter>
            <AlertDialogCancel onClick={() => {
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
                setOldHostedNetworkName(hostedNetworkName);
                setOldHostedNetworkPassword(hostedNetworkPassword);
                await invoke("stop_hosted_network");
                const success = await invoke("start_hosted_network", {ssid: hostedNetworkName, password: hostedNetworkPassword});
                setHostedNetworkModalOpen(false);
                if (success) {
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