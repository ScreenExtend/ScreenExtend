import { useState } from "react";
import { Link } from "react-router-dom";

import Layout from "@/layout/layout";
import { DeviceDetails } from "@/components/pages/device-details";
import { buttonVariants, Button } from "@/components/ui/button";
import { Plus, Info, ExternalLink, RefreshCw } from "lucide-react";
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

import { toast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { Command } from "@tauri-apps/plugin-shell";
import { type } from "@tauri-apps/plugin-os";
import { useConnectedDevices } from "@/components/global-provider";
import { type ConnectedDevice } from "@/components/config-provider";
import { commands } from "@/lib/bindings";
import { cn } from "@/lib/utils";

function RefreshStream({ device, label }: { device: ConnectedDevice; label: string }) {
  const [busy, setBusy] = useState(false);
  const { t } = useTranslation();
  const name = label;

  const refresh = async () => {
    setBusy(true);
    try {
      // This tab's stream, not the device's: a device with two open restarts only the one
      // whose row was clicked.
      const restarted = await commands.refreshDeviceStream(device.token, device.joinId);
      toast(
        restarted
          ? {
            title: t("toasts.device.refreshedTitle"),
            description: t("toasts.device.refreshedDescription", { name }),
          }
          : {
            title: t("toasts.device.refreshOfflineTitle"),
            description: t("toasts.device.refreshOfflineDescription", { name }),
          }
      );
    } catch (e) {
      console.error("failed to refresh the device stream", e);
      toast({
        variant: "destructive",
        title: t("toasts.device.refreshFailedTitle"),
        description: t("toasts.device.refreshFailedDescription"),
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button
            variant="outline"
            size="icon"
            aria-label={t("device.refreshStream.label")}
            disabled={busy}
            onClick={refresh}
          />
        }
      >
        <RefreshCw size={16} className={cn(busy && "animate-spin")} />
      </TooltipTrigger>
      <TooltipContent className="max-w-[220px]">
        {t("device.refreshStream.tip")}
      </TooltipContent>
    </Tooltip>
  );
}

// One row per live session, so a machine with two tabs open is two rows carrying the same
// device name. The screen size tells them apart when it differs; when it does not, the row's
// position among that device's own sessions does. Neither shows anything for a device holding
// a single session, which is every device until the owner opens a second tab.
function rowLabel(device: ConnectedDevice, all: ConnectedDevice[]): string {
  const name = device.name || device.ip || "Unknown device";
  const siblings = all.filter(d => d.token && d.token === device.token);
  if (siblings.length < 2) return name;
  const sizes = new Set(siblings.map(d => d.screenSize).filter(Boolean));
  if (device.screenSize && sizes.size === siblings.length) {
    return `${name} (${device.screenSize})`;
  }
  const nth = siblings.findIndex(d => d.joinId === device.joinId) + 1;
  return nth > 0 ? `${name} (${nth})` : name;
}

export default function Devices() {
  const [devicesTooltipOpen, setDevicesTooltipOpen] = useState(false);
  const devices = useConnectedDevices();
  const { t } = useTranslation();

  return (
    <Layout>
      <div className="p-8">
        <div className="mb-6">
          <div className="flex items-center">
            <h2 className="text-2xl font-semibold">Connected Devices</h2>
            <TooltipProvider delay={100}>
              <Tooltip open={devicesTooltipOpen} onOpenChange={state => setDevicesTooltipOpen(state)}>
                <TooltipTrigger
                  className="cursor-pointer ml-1.5"
                  onClick={() => setDevicesTooltipOpen(true)}
                  render={<span />}
                >
                  <Info size={15} />
                </TooltipTrigger>
                <TooltipContent className="max-w-[220px]">
                  <p>Rearrange extended displays in your system's display settings.</p>
                </TooltipContent>
              </Tooltip>
            </TooltipProvider>
            <div className="flex-grow"></div>
            <Button variant="secondary" size="sm" onClick={async () => {
              const notifyFailure = () => toast({
                variant: "destructive",
                title: t("toasts.displaySettingsUnavailable.title"),
                description: t("toasts.displaySettingsUnavailable.description"),
              });
              try {
                const osType = type();
                if (osType === "windows") {
                  await Command.create("control", ["desk.cpl"]).spawn();
                } else if (osType === "macos") {
                  await Command.create("open", ["x-apple.systempreferences:com.apple.preference.displays"]).spawn();
                } else {
                  toast({
                    title: t("toasts.displaySettingsUnavailable.title"),
                    description: t("toasts.displaySettingsUnavailable.description"),
                  });
                }
              } catch (e) {
                console.error("failed to open display settings", e);
                notifyFailure();
              }
            }}>
              <ExternalLink className="mr-2" size={16} />
              Display Settings
            </Button>
          </div>
          <p className="text-gray-500">{ devices.length } Screen{ devices.length !== 1 && "s" } Connected</p>
        </div>
        <div>
          <TooltipProvider delay={150}>
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
              {/* Keyed by identity, not by array position: `deviceJoin` rebuilds the
                  list as filter-then-push, so a rejoining device moves to the end and
                  every index after it shifts. React then reuses row instances across
                  different devices, which can carry an open sheet onto the wrong one. */}
              {devices.map((device) => (
                <TableRow key={device.joinId || device.token || device.ip}>
                  <TableCell>{rowLabel(device, devices)}</TableCell>
                  <TableCell>{device.ip}</TableCell>
                  <TableCell>{device.os}</TableCell>
                  <TableCell>{device.scale}%</TableCell>
                  <TableCell>{device.orientation}</TableCell>
                  <TableCell>{device.refreshRate} Hz</TableCell>
                  <TableCell>{device.screenSize}</TableCell>
                  <TableCell className="text-center">
                    <div className="flex items-center justify-center space-x-2">
                      <RefreshStream device={device} label={rowLabel(device, devices)} />
                      <DeviceDetails device={device} />
                    </div>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          </TooltipProvider>
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
