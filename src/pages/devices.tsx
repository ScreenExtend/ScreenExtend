import { useState, useEffect } from "react";
import { Link } from "react-router-dom";

import Layout from "@/layout/layout";
import { DeviceDetails } from "@/components/pages/device-details";
import { buttonVariants } from "@/components/ui/button";
import { Plus, Info } from "lucide-react";
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

import { type Device } from "@/components/auth-provider";
import { listen, emit } from "@tauri-apps/api/event";
import { cn } from "@/lib/utils";

export default function Devices() {
  const [devicesTooltipOpen, setDevicesTooltipOpen] = useState(false);
  const [devices, setDevices] = useState<Device[]>([]);

  useEffect(() => {
    const start_listener = async () => {
      await listen("device_join", event => setDevices(prev => [...prev, event.payload as Device]));
      await listen("device_modify", event => setDevices(prev => prev.map(device => device.ip === (event.payload as Device).ip ? (event.payload as Device) : device)));
      await listen("device_remove", event => setDevices(prev => prev.filter(device => device.ip !== (event.payload as Device).ip)));
      await emit("device_ready");
    }
    void start_listener();
  }, []);

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
                <TooltipContent>
                  <p>The arrangement of extended displays can be modified in your system's display settings.{"\u00a0"}</p>
                </TooltipContent>
              </Tooltip>
            </TooltipProvider>
          </div>
          <p className="text-gray-500">{ devices.length } device{ devices.length !== 1 && "s" } connected</p>
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
              {devices.map((device) => (
                <TableRow key={device.id}>
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
                Add a device to get started
              </Link>
            </div>
          )}
        </div>
      </div>
    </Layout>
  );
}
