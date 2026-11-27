import { useState } from "react";
import { Link } from "react-router-dom";

import Layout from "@/layout/layout";
import { DeviceDetails } from "@/components/pages/device-details";
import { buttonVariants, Button } from "@/components/ui/button";
import { Plus, Info, Monitor, RefreshCw } from "lucide-react";
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
import { useConnectedDevices } from "@/components/global-provider";
import { type ConnectedDevice } from "@/components/config-provider";
import { commands } from "@/lib/bindings";
import { cn } from "@/lib/utils";
import { RearrangeDisplaysDialog } from "@/components/rearrange-displays-dialog";

function RefreshStream({ device, label }: { device: ConnectedDevice; label: string }) {
  const [busy, setBusy] = useState(false);
  const { t } = useTranslation();
  const name = label;

  const refresh = async () => {
    setBusy(true);
    try {
      // this tab's stream, not the device's: with two tabs open only the clicked row restarts
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

// One row per live session, so two tabs of one device share a name: tell them apart by
// screen size when it differs, else by position among that device's sessions. A device with
// a single session gets no suffix.
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
  const [arrangeOpen, setArrangeOpen] = useState(false);
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
                  <p>{t("rearrangeDisplays.tooltip")}</p>
                </TooltipContent>
              </Tooltip>
            </TooltipProvider>
            <div className="flex-grow"></div>
            <Button variant="secondary" size="sm" onClick={() => setArrangeOpen(true)}>
              <Monitor className="mr-2" size={16} />
              {t("rearrangeDisplays.button")}
            </Button>
            <RearrangeDisplaysDialog open={arrangeOpen} onOpenChange={setArrangeOpen} />
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
