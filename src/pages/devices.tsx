import { Link } from "react-router-dom";
import { cn } from "@/lib/utils";
import Layout from "@/layout/layout";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { DeviceDetails } from "@/components/pages/device-details";
import { buttonVariants } from "@/components/ui/button";
import { Plus } from "lucide-react";

const devices = Array(7)
  .fill({})
  .map((_, index) => ({
    name: `Device ${index + 1}`,
    ip: `192.168.${Math.floor(Math.random() * 255) + 1}.${Math.floor(Math.random() * 255) + 1}`,
    os: ["Windows", "MacOS", "Linux", "Android", "iOS", "iPadOS"][
      Math.floor(Math.random() * 6)
    ],
    scale: Math.floor(Math.random() * 100) + 1,
    orientation: Math.random() > 0.5 ? "Portrait" : "Landscape",
    refreshRate: Math.floor(Math.random() * 100) + 1,
    screenSize: "1080x1920",
    isAudioActive: Math.random() > 0.5,
    isVedioActive: Math.random() > 0.5,
    isKeyboardActive: Math.random() > 0.5,
    isMouseActive: Math.random() > 0.5,
    isCameraActive: Math.random() > 0.5,
    isMicrophoneActive: Math.random() > 0.5,
    isClipboardActive: Math.random() > 0.5,
  }));

export type Device = (typeof devices)[0];

export default function Devices() {
  return (
    <Layout>
      <div className="p-8">
        <div className="mb-6">
          <h2 className="text-2xl font-semibold">Connected Devices</h2>
          <p className="text-gray-500">7 devices connected.</p>
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
              {devices.map((device, index) => (
                <TableRow key={index}>
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
