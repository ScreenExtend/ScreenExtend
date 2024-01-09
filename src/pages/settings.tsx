import Layout from "@/layout/layout";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { useState } from "react";
import { Eye, EyeOff, Info } from "lucide-react";
import { cn } from "@/lib/utils";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";

export default function Settings() {
  const [isPrivate, setIsPrivate] = useState(false);
  const [isOnline, setIsOnline] = useState(false);
  const [showPassword, setShowPassword] = useState(false);

  function togglePasswordVisibility() {
    if (!isPrivate) return;
    setShowPassword((prev) => !prev);
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
                        onClick={togglePasswordVisibility}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        onClick={togglePasswordVisibility}
                      />
                    )}
                  </div>
                </div>
                <Button disabled={!isPrivate}>Save Password</Button>
              </div>
            </CardContent>
          </Card>
        </div>
        <div className="">
          <Card>
            <CardHeader>
              <CardTitle>Online / Offline</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
              <div className=" flex items-center space-x-4 border-b p-3 px-0">
                <div className="flex-1 space-y-1">
                  <p className="text-sm font-medium leading-none">
                    Toggle Offline
                  </p>
                </div>
                <Switch checked={isOnline} onCheckedChange={setIsOnline} />
              </div>
              <div className="flex items-center gap-4">
                <div className="grid grid-cols-2 gap-4 flex-grow">
                  <Input
                    placeholder="Network name"
                    type="text"
                    disabled={!isOnline}
                  />
                  <Input
                    placeholder="Network password"
                    type="password"
                    disabled={!isOnline}
                  />
                </div>
                <TooltipProvider>
                  <Tooltip>
                    <TooltipTrigger asChild className="cursor-pointer">
                      <Info size={15} />
                    </TooltipTrigger>
                    <TooltipContent>
                      <p>I will configure this later</p>
                    </TooltipContent>
                  </Tooltip>
                </TooltipProvider>
              </div>
            </CardContent>
          </Card>
        </div>
      </div>
    </Layout>
  );
}
