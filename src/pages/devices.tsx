import { useState } from "react";
import { Link } from "react-router-dom";

import Layout from "@/layout/layout";
import { DeviceDetails } from "@/components/pages/device-details";
import { buttonVariants, Button } from "@/components/ui/button";
import { Plus, Info, RefreshCw } from "lucide-react";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";

import { toast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { useConnectedDevices } from "@/components/global-provider";
import { type ConnectedDevice } from "@/components/config-provider";
import { commands } from "@/lib/bindings";
import { cn } from "@/lib/utils";
import { formatHz } from "@/lib/format";
import { DisplayCanvas } from "@/components/display-canvas";

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
  const devices = useConnectedDevices();
  const { t } = useTranslation();
  const labelFor = (device: ConnectedDevice) => rowLabel(device, devices);

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
          </div>
          <p className="text-gray-500">{ devices.length } Screen{ devices.length !== 1 && "s" } Connected</p>
        </div>

        <TooltipProvider delay={150}>
          <DisplayCanvas devices={devices} labelFor={labelFor} />

          {devices.length > 0 && (
            <div className="mt-6">
              <h3 className="mb-2 text-sm font-medium text-muted-foreground">{t("rearrangeDisplays.deviceList")}</h3>
              <div className="rounded-md border">
                {/* Keyed by identity, not by array position: `deviceJoin` rebuilds the
                    list as filter-then-push, so a rejoining device moves to the end and
                    every index after it shifts. React then reuses row instances across
                    different devices, which can carry an open sheet onto the wrong one. */}
                {devices.map((device, i) => (
                  <div
                    key={device.joinId || device.token || device.ip}
                    className={cn("flex items-center px-4 py-3", i > 0 && "border-t")}
                  >
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-base font-medium">{labelFor(device)}</p>
                      <p className="truncate text-sm text-muted-foreground">
                        {[device.ip, device.os, device.screenSize, device.orientation, formatHz(device.refreshRate)]
                          .filter(Boolean)
                          .join(" · ")}
                      </p>
                    </div>
                    <div className="ml-3 flex items-center space-x-2">
                      <RefreshStream device={device} label={labelFor(device)} />
                      <DeviceDetails device={device} />
                    </div>
                  </div>
                ))}
              </div>
            </div>
          )}
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
    </Layout>
  );
}
