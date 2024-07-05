import Layout from "@/layout/layout";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import React, { useState, useContext } from "react";
import { Eye, EyeOff, Info } from "lucide-react";
import { cn } from "@/lib/utils";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { AuthProviderContext } from "@/components/auth-provider";

export default function Settings() {
  const { currentUser, setCurrentUser } = useContext(AuthProviderContext);
  const [isPrivate, setIsPrivate] = useState(false);
  const [isOnline, setIsOnline] = useState(false);
  const [showPassword, setShowPassword] = useState(false);
  const [showNetPassword, setShowNetPassword] = useState(false);
  const [showAccountPassword, setShowAccountPassword] = useState(false);
  const [networkName, setNetworkName] = useState("ScreenExtend");
  const [networkPassword, setNetworkPassword] = useState("");
  const [accountPassword, setAccountPassword] = useState(currentUser.password);

  const handleNetworkNameChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    const value = e.target.value;
    if (value.startsWith("ScreenExtend")) {
      setNetworkName(value);
    } else {
      setNetworkName("ScreenExtend" + value.slice(12));
    }
  };

  function togglePasswordVisibility(type: "password" | "netPassword" | "accountPassword") {
    if (type === "password") {
      if (!isPrivate) return;
      setShowPassword((prev) => !prev);
    } else if (type === "accountPassword") {
      setShowAccountPassword((prev) => !prev);
    } else {
      if (!isOnline) return;
      setShowNetPassword((prev) => !prev);
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
              <CardTitle>Public Settings</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
              <div className=" flex items-center space-x-4 border-b p-3 px-0">
                <div className="flex-1 space-y-1">
                  <p className="text-sm font-medium leading-none">
                    Toggle Private
                  </p>
                </div>
                <Switch checked={isPrivate} onCheckedChange={setIsPrivate} />
              </div>
              <div className="flex items-center space-x-4 p-3 px-0">
                <div className="relative outline-none flex-1">
                  <Input
                    type={showPassword ? "text" : "password"}
                    placeholder="Password"
                    className="outline-none"
                    disabled={!isPrivate}
                    hoverLabel={true}
                  />
                  <div
                    className={cn(
                      "absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      !isPrivate && "cursor-not-allowed"
                    )}
                  >
                    {showPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        onClick={() => togglePasswordVisibility("password")}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        onClick={() => togglePasswordVisibility("password")}
                      />
                    )}
                  </div>
                </div>
                <Button disabled={!isPrivate}>Save Password</Button>
              </div>
            </CardContent>
          </Card>
        </div>
        <div className="mb-4">
          <Card>
            <CardHeader>
              <CardTitle>Online / Offline</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
              <div className="flex items-center space-x-4 border-b p-3 px-0">
                <div className="flex-1 space-y-1">
                  <p className="text-sm font-medium leading-none">
                    Toggle Offline
                  </p>
                </div>
                <Switch checked={isOnline} onCheckedChange={setIsOnline} />
                <TooltipProvider>
                  <Tooltip>
                    <TooltipTrigger asChild className="cursor-pointer">
                      <Info size={15} />
                    </TooltipTrigger>
                    <TooltipContent>
                      <p>You can create a local network that other devices can join. This is useful for speed or if no other networks are available.</p>
                    </TooltipContent>
                  </Tooltip>
                </TooltipProvider>
              </div>
              <div className="flex items-center space-x-4 p-3 px-0">
                <div className="relative outline-none flex-1">
                  <Input
                    type="text"
                    placeholder="Network Name"
                    className="outline-none"
                    disabled={!isOnline}
                    value={networkName}
                    onChange={handleNetworkNameChange}
                    hoverLabel={true}
                  />
                </div>
                <div className="relative outline-none flex-1">
                  <Input
                    type={showNetPassword ? "text" : "password"}
                    placeholder="Network Password"
                    className="outline-none"
                    disabled={!isOnline}
                    value={networkPassword}
                    hoverLabel={true}
                    onChange={(e) => {setNetworkPassword(e.target.value)}}
                  />
                  <div
                    className={cn(
                      "absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer",
                      !isOnline && "cursor-not-allowed"
                    )}
                  >
                    {showNetPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        onClick={() => togglePasswordVisibility("netPassword")}
                      />
                      ) : (
                        <Eye
                          className="h-5 w-5"
                          onClick={() => togglePasswordVisibility("netPassword")}
                        />
                    )}
                  </div>
                </div>
                <Button disabled={!isOnline}>Save Settings</Button>
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
                    type={showAccountPassword ? "text" : "password"}
                    placeholder="Password"
                    className="outline-none"
                    defaultValue={accountPassword}
                    onChange={(e) => {setAccountPassword(e.target.value)}}
                    id={"changePasswordInput"}
                    hoverLabel={true}
                  />
                  <div
                    className={"absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer"}
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
                <Button onClick={() => {
                  setCurrentUser({username: currentUser.username, password: accountPassword})
                  setShowAccountPassword(false);
                }}>Save Password</Button>
              </div>
            </CardContent>
          </Card>
        </div>
      </div>
    </Layout>
  );
}