import { useState, useContext } from "react";
import { Link } from "react-router-dom";

import Layout from "@/layout/layout";
import { DeviceDetails } from "@/components/pages/device-details";
import { buttonVariants, Button } from "@/components/ui/button";
import { Plus, Info, ExternalLink } from "lucide-react";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

import { useToast } from "@/components/ui/use-toast";
import { Command } from "@tauri-apps/plugin-shell";
import { type } from "@tauri-apps/plugin-os";
import { GlobalProviderContext } from "@/components/global-provider";
import { cn } from "@/lib/utils";

export default function Devices() {
  const [devicesTooltipOpen, setDevicesTooltipOpen] = useState(false);
  const { windowDevices: [devices] } = useContext(GlobalProviderContext);
  const { toast } = useToast();

  return (
    <Layout>
      <div className="p-8">
        <div className="mb-6">
          <div className="flex items-center">
            <h2 className="text-2xl font-semibold">Connected Devices</h2>
            <TooltipProvider>
              <Tooltip delayDuration={100} open={devicesTooltipOpen} onOpenChange={state => setDevicesTooltipOpen(state)}>
                <TooltipTrigger asChild className="cursor-pointer top-1/2 ml-1.5" onClick={() => setDevicesTooltipOpen(true)}>
                  <Info size={15} />
                </TooltipTrigger>
                <TooltipContent className="max-w-[220px]">
                  <p>Rearrange extended displays in your system's display settings.</p>
                </TooltipContent>
              </Tooltip>
            </TooltipProvider>
            <div className="flex-grow"></div>
            <Button variant="secondary" size="sm" onClick={async () => {
              const osType = type();
              if (osType === "windows") {
                await Command.create("control", ["desk.cpl"]).execute();
              } else if (osType === "macos") {
                await Command.create("open", ["x-apple.systempreferences:com.apple.preference.displays"]).execute();
              } else {
                toast({
                  title: "Unable to Open Display Settings",
                  description: "Please adjust your display settings manually.",
                });
              }
            }}>
              <ExternalLink className="mr-2" size={16} />
              Display Settings
            </Button>
          </div>
          <p className="text-gray-500">{ devices.length } Device{ devices.length !== 1 && "s" } Connected</p>
        </div>
        <div>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-[150px]">Device Name</TableHead>
                <TableHead>IP</TableHead>
                <TableHead>OS</TableHead>
                <TableHead>Scale</TableHead>
                <TableHead>Orientation</TableHead>
                <TableHead>Refresh Rate</TableHead>
                <TableHead>Screen Size</TableHead>
                <TableHead></TableHead>
              </TableRow>
            </TableHeader>
            <TableBody className="border-t">
              {devices.map((device, id) => (
                <TableRow key={id}>
                  <TableCell>{device.name}</TableCell>
                  <TableCell>{device.ip}</TableCell>
                  <TableCell>{device.os}</TableCell>
                  <TableCell>{device.scale}%</TableCell>
                  <TableCell>{device.orientation}</TableCell>
                  <TableCell>{device.refreshRate} Hz</TableCell>
                  <TableCell>{device.screenSize}</TableCell>
                  <TableCell className="text-center">
                    <DeviceDetails device={device} />
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          {!devices.length && (
            <div className="w-full flex items-center justify-center py-4">
              <Link
                to="/dashboard"
                className={cn(buttonVariants({ variant: "secondary" }))}
              >
                <Plus className="mr-2" size={16} />
                Connect a Device
              </Link>
            </div>
          )}
        </div>
      </div>
    </Layout>
  );
}
