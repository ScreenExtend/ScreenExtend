import Layout from "@/layout/layout";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { useState } from "react";

export default function Settings() {
  const [isPublic, setIsPublic] = useState(true);

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
                    Toggle Public
                  </p>
                </div>
                <Switch checked={isPublic} onCheckedChange={setIsPublic} />
              </div>
              <div className="flex items-center space-x-4 p-3 px-0">
                <Input
                  type="password"
                  placeholder="Password"
                  disabled={isPublic}
                />
                <Button disabled={isPublic}>Save Password</Button>
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
                    Toggle Online
                  </p>
                </div>
                <Switch />
              </div>
              <div></div>
            </CardContent>
          </Card>
        </div>
      </div>
    </Layout>
  );
}
