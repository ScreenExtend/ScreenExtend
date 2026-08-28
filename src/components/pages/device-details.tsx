import React, { useEffect, useState } from "react";

import { Info } from "lucide-react";

import { Slider } from "@/components/ui/slider";
import { Switch } from "@/components/ui/switch";
import { Checkbox } from "@/components/ui/checkbox";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Sheet,
  SheetContent,
  SheetFooter,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
  SheetClose,
} from "@/components/ui/sheet";
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

import { updateConfig, getConfig, saveDeviceSettings, removeSavedDevice, type Device } from "@/components/config-provider";
import { useToast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { commands, events } from "@/lib/bindings";
import { cn } from "@/lib/utils";
import { useFormik } from "formik";

function TipLabel({
  text,
  children,
  className,
}: {
  text: string;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <span
            className={cn(
              "inline-flex w-fit cursor-help items-center gap-1",
              className
            )}
          />
        }
      >
        {children}
        <Info className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
      </TooltipTrigger>
      <TooltipContent className="max-w-[15rem]">{text}</TooltipContent>
    </Tooltip>
  );
}

export function DeviceDetails({ device }: { device: Device }) {
  const [open, setOpen] = useState(false);
  const [warningDialogOpen, setWarningDialogOpen] = useState(false);
  const [dontShowAgain, setDontShowAgain] = useState(true);
  const [inProgress, setInProgress] = useState(false);
  const [tempRate, setTempRate] = useState(device.refreshRate);
  const [tempQuality, setTempQuality] = useState(device.videoQuality);
  const { toast } = useToast();
  const { t } = useTranslation();

  const [audioBackend, setAudioBackend] = useState<string>("unsupported");
  const [audioInstalling, setAudioInstalling] = useState(false);
  const [installPromptOpen, setInstallPromptOpen] = useState(false);
  const refreshAudioBackend = React.useCallback(() => {
    commands
      .checkSystemRequirements()
      .then((r) => setAudioBackend(r.audio_backend))
      .catch(() => setAudioBackend("unsupported"));
  }, []);
  useEffect(() => {
    refreshAudioBackend();
  }, [refreshAudioBackend]);

  const audioNeedsInstall = audioBackend === "needs_driver_install";
  const audioUnsupported = audioBackend === "unsupported";
  const audioActiveLegacy = audioBackend === "virtual_device";

  const runAudioInstall = async () => {
    setInstallPromptOpen(false);
    setAudioInstalling(true);
    try {
      const outcome = await commands.installAudioDriver();
      if (outcome === "installed") {
        setAudioBackend("virtual_device");
        toast({
          title: t("device.systemAudio.installedTitle"),
          description: t("device.systemAudio.installedBody"),
        });
      } else if (outcome === "needs_reboot") {
        toast({
          title: t("device.systemAudio.needsRebootTitle"),
          description: t("device.systemAudio.needsRebootBody"),
        });
      } else if (outcome === "cancelled") {
        toast({ description: t("device.systemAudio.cancelledBody") });
      } else {
        toast({
          variant: "destructive",
          title: t("device.systemAudio.installFailedTitle"),
          description: t("device.systemAudio.installFailedBody"),
        });
      }
    } finally {
      setAudioInstalling(false);
      refreshAudioBackend();
    }
  };

  const deviceDetails = useFormik({
    initialValues: {
      ...device,
      dpr: device.dpr ?? device.maxDpr ?? 1,
      maxDpr: device.maxDpr ?? device.dpr ?? 1,
      systemAudio: device.systemAudio ?? false,
    },
    onSubmit: async (values) => {
      setInProgress(true);
      const normalized: Device = {
        ...values,
        scale: Number(values.scale),
        refreshRate: Number(values.refreshRate),
        videoScale: Number(values.videoScale),
        videoQuality: Number(values.videoQuality),
        dpr: Number(values.dpr),
      };
      try {
        await commands.setDeviceOverride(
          normalized.ip,
          normalized.scale,
          normalized.orientation,
          normalized.refreshRate,
          normalized.videoScale,
          normalized.videoQuality,
          normalized.remoteControl,
          normalized.dpr,
          normalized.systemAudio
        );
        await saveDeviceSettings(normalized);
        await events.deviceModify.emit(normalized);
        toast({
          title: t("toasts.device.updatedTitle"),
          description: t("toasts.device.updatedDescription"),
        });
        setOpen(false);
      } catch (e) {
        console.error("failed to save device settings", e);
        toast({ variant: "destructive", title: t("toasts.device.updatedTitle"), description: String(e) });
      } finally {
        setInProgress(false);
      }
    },
  });

  const considerClosing = async (event: CustomEvent<{originalEvent: PointerEvent}> | CustomEvent<{originalEvent: FocusEvent}> | KeyboardEvent) => {
    event.preventDefault();
    if (JSON.stringify(deviceDetails.values) === JSON.stringify(device)) {
      setOpen(false);
    } else {
      if ((await getConfig())!.dontShowAgain.editDevice) {
        setOpen(false);
        deviceDetails.resetForm({ values: device });
      } else {
        setWarningDialogOpen(true);
      }
    }
  };

  const openChangeHandler = async (open: boolean) => {
    if (open) {
      setOpen(true);
    } else {
      if (JSON.stringify(deviceDetails.values) === JSON.stringify(device)) {
        setOpen(false);
      } else {
        if ((await getConfig())!.dontShowAgain.editDevice) {
          setOpen(false);
          deviceDetails.resetForm({ values: device });
        } else {
          setWarningDialogOpen(true);
        }
      }
    }
  };

  return (
    <Sheet onOpenChange={openChangeHandler} open={open}>
      <SheetTrigger asChild>
        <Button variant="outline">Edit Device</Button>
      </SheetTrigger>
      <SheetContent
        className="min-w-[350px] overflow-y-auto"
        onInteractOutside={considerClosing}
        onEscapeKeyDown={considerClosing}
        trapFocus={true}
      >
        <SheetClose asChild />
        <SheetHeader>
          <SheetTitle>Edit Device</SheetTitle>
        </SheetHeader>
        <TooltipProvider delay={150}>
        <div className="py-4">
          <div className="flex">
            <div className="flex-1">
              <TipLabel text="The name for this device in ScreenExtend.">
                <Label>Device Name</Label>
              </TipLabel>
              <Input
                placeholder="Device Name"
                name="name"
                value={deviceDetails.values.name}
                onChange={deviceDetails.handleChange}
                onBlur={deviceDetails.handleBlur}
                hoverLabel={false}
                disabled={inProgress}
              />
            </div>
            <div className="flex-1 ml-4">
              <TipLabel text="Rotates the extended display for this device.">
                <Label>Orientation</Label>
              </TipLabel>
              <Select
                name="orientation"
                value={deviceDetails.values.orientation}
                defaultValue={deviceDetails.values.orientation}
                onValueChange={(value) => {
                  deviceDetails.setFieldValue("orientation", value);
                }}
                disabled={inProgress}
              >
                <SelectTrigger className="w-full border-2">
                  <SelectValue placeholder="Orientation" />
                </SelectTrigger>
                <SelectContent className="cursor-pointer">
                  <SelectItem value="Portrait">Portrait</SelectItem>
                  <SelectItem value="Landscape">Landscape</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>
          <div className="mt-4">
            <TipLabel text="The device's network address (read-only).">
              <Label>Device IP</Label>
            </TipLabel>
            <Input
              disabled={true}
              placeholder="182.167.99.1"
              name={device.ip}
              value={deviceDetails.values.ip}
              onChange={deviceDetails.handleChange}
              onBlur={deviceDetails.handleBlur}
              hoverLabel={false}
            />
          </div>
          <div className="mt-4">
            <TipLabel text="The operating system of this device (read-only).">
              <Label>Device OS</Label>
            </TipLabel>
            <Input
              disabled={true}
              placeholder="00-B0-D0-63-C2-26"
              name="OS"
              value={deviceDetails.values.os}
              onChange={deviceDetails.handleChange}
              onBlur={deviceDetails.handleBlur}
              hoverLabel={false}
            />
          </div>
          <div className="mt-4">
            <TipLabel text="The device's screen resolution in pixels (read-only).">
              <Label>Screen Size</Label>
            </TipLabel>
            <Input
              disabled={true}
              placeholder="1080x1920"
              name="screenSize"
              value={deviceDetails.values.screenSize}
              onChange={deviceDetails.handleChange}
              onBlur={deviceDetails.handleBlur}
              hoverLabel={false}
            />
          </div>
          <div className="mt-4 flex items-center justify-between gap-4 border-t pt-4">
            <TipLabel text="Let this device control your PC with touch and keyboard. Turn off to keep the display view-only.">
              <Label>Remote Control</Label>
            </TipLabel>
            <Switch
              checked={deviceDetails.values.remoteControl}
              onCheckedChange={(checked) =>
                deviceDetails.setFieldValue("remoteControl", checked)
              }
              disabled={inProgress}
            />
          </div>
          <div className="mt-4 flex items-center justify-between gap-4 border-t pt-4">
            <TipLabel
              text={
                audioUnsupported
                  ? t("device.systemAudio.unsupported")
                  : audioNeedsInstall
                    ? t("device.systemAudio.needsInstall")
                    : audioActiveLegacy
                      ? t("device.systemAudio.active")
                      : t("device.systemAudio.tip")
              }
            >
              <Label>{t("device.systemAudio.label")}</Label>
            </TipLabel>
            <Switch
              checked={deviceDetails.values.systemAudio}
              onCheckedChange={(checked) => {
                if (checked && audioNeedsInstall) {
                  setInstallPromptOpen(true);
                  return;
                }
                deviceDetails.setFieldValue("systemAudio", checked);
              }}
              disabled={inProgress || audioInstalling || audioUnsupported}
            />
          </div>
          <AlertDialog open={installPromptOpen} onOpenChange={setInstallPromptOpen}>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>
                  {t("device.systemAudio.installTitle")}
                </AlertDialogTitle>
                <AlertDialogDescription>
                  {t("device.systemAudio.installBody")}
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>{t("common.cancel")}</AlertDialogCancel>
                <AlertDialogAction onClick={runAudioInstall}>
                  {t("device.systemAudio.installConfirm")}
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
          <div className="mt-4">
            <TipLabel text="Zooms the extended desktop's content up or down." className="my-2">
              <Label>Scale - ({deviceDetails.values.scale}%)</Label>
            </TipLabel>
            <Slider
              value={[deviceDetails.values.scale]}
              defaultValue={[deviceDetails.values.scale]}
              onValueChange={(value) => {
                deviceDetails.setFieldValue("scale", value[0]);
              }}
              min={25}
              max={200}
              step={25}
              disabled={inProgress}
            />
          </div>
          <div className="mt-4">
            <TipLabel
              text="Renders this display at the device's pixel density for sharper text and UI. Defaults to the screen's native ratio; lower it toward 1x to save bandwidth and encoding load."
              className="my-2"
            >
              <Label>Pixel Ratio - ({deviceDetails.values.dpr.toFixed(1)}×)</Label>
            </TipLabel>
            <Slider
              value={[deviceDetails.values.dpr]}
              defaultValue={[deviceDetails.values.dpr]}
              onValueChange={(value) => {
                deviceDetails.setFieldValue("dpr", value[0]);
              }}
              min={1}
              max={Math.max(1, deviceDetails.values.maxDpr)}
              step={0.5}
              disabled={inProgress || deviceDetails.values.maxDpr <= 1}
            />
          </div>
          <div className="mt-4">
            <Label className="my-2 flex items-center">
              <TipLabel text="How many frames per second the display targets (Hz).">
                Refresh Rate -
              </TipLabel>
              <div className="flex items-center ml-1">
                <Input
                  name="refreshRate"
                  type="number"
                  min={15}
                  max={500}
                  step={1}
                  value={deviceDetails.values.refreshRate}
                  onChange={(event) => {
                    deviceDetails.setFieldValue(
                      "refreshRate",
                      event.target.value
                    );
                  }}
                  onFocus={(event) => {
                    setTempRate(parseInt(event.target.value));
                  }}
                  onBlur={(event) => {
                    const value = parseInt(event.target.value.trim());
                    if (!(value >= 15 && value <= 500)) {
                      deviceDetails.setFieldValue(
                        "refreshRate",
                        tempRate
                      );
                    } else {
                      setTempRate(value);
                    }
                  }}
                  className="w-12 px-1 text-center"
                  hoverLabel={false}
                  disabled={inProgress}
                />
                <span className="ml-1">Hz</span>
              </div>
            </Label>
            <Slider
              value={[deviceDetails.values.refreshRate]}
              defaultValue={[deviceDetails.values.refreshRate]}
              onValueChange={(value) => {
                deviceDetails.setFieldValue("refreshRate", value[0]);
                setTempRate(value[0]);
              }}
              min={15}
              max={500}
              step={5}
              disabled={inProgress}
            />
          </div>
          <div className="mt-4">
            <TipLabel text="Resolution of the streamed video. Lower uses less bandwidth." className="my-2">
              <Label>Video Scale - ({deviceDetails.values.videoScale}%)</Label>
            </TipLabel>
            <Slider
              value={[deviceDetails.values.videoScale]}
              defaultValue={[deviceDetails.values.videoScale]}
              onValueChange={(value) => {
                deviceDetails.setFieldValue("videoScale", value[0]);
              }}
              min={10}
              max={100}
              step={5}
              disabled={inProgress}
            />
          </div>
          <div className="mt-4">
            <Label className="my-2 flex items-center">
              <TipLabel text="Higher encodes faster but looks worse. Pick the highest value that still looks good.">
                Video Quality -
              </TipLabel>
              <div className="flex items-center ml-1">
                <Input
                  name="videoQuality"
                  type="number"
                  min={1}
                  max={51}
                  step={1}
                  value={deviceDetails.values.videoQuality}
                  onChange={(event) => {
                    deviceDetails.setFieldValue(
                      "videoQuality",
                      event.target.value
                    );
                  }}
                  onFocus={(event) => {
                    setTempQuality(parseInt(event.target.value));
                  }}
                  onBlur={(event) => {
                    const value = parseInt(event.target.value.trim());
                    if (!(value >= 1 && value <= 51)) {
                      deviceDetails.setFieldValue("videoQuality", tempQuality);
                    } else {
                      setTempQuality(value);
                    }
                  }}
                  className="w-12 px-1 text-center"
                  hoverLabel={false}
                  disabled={inProgress}
                />
              </div>
            </Label>
            <Slider
              value={[deviceDetails.values.videoQuality]}
              defaultValue={[deviceDetails.values.videoQuality]}
              onValueChange={(value) => {
                deviceDetails.setFieldValue("videoQuality", value[0]);
                setTempQuality(value[0]);
              }}
              min={1}
              max={51}
              step={1}
              disabled={inProgress}
            />
          </div>
        </div>
        </TooltipProvider>
        <SheetFooter>
          <div className="flex w-full mt-3">
            <DeleteDevice
              onClick={async () => {
                setInProgress(true);
                await commands.removeDeviceOverride(device.ip);
                await removeSavedDevice(device.ip);
                await events.deviceRemove.emit(device);
                setInProgress(false);
                toast({
                  title: t("toasts.device.removedTitle"),
                  description: t("toasts.device.removedDescription"),
                });
                setOpen(false);
              }}
              disabled={inProgress}
            />
            <Button
              className="flex-1 text-white ml-4"
              type="submit"
              onClick={() => {
                deviceDetails.handleSubmit();
              }}
              disabled={inProgress}
            >
              Save changes
            </Button>
          </div>
        </SheetFooter>
      </SheetContent>
      <AlertDialog open={warningDialogOpen} onOpenChange={setWarningDialogOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("dialogs.editDeviceUnsaved.title")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("dialogs.editDeviceUnsaved.description")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <div className="flex items-center space-x-2 mb-4">
            <Checkbox
              id="dontShowAgain"
              checked={dontShowAgain}
              onCheckedChange={checked => setDontShowAgain(checked === true)}
            />
            <label
              htmlFor="dontShowAgain"
              className="text-sm text-muted-foreground cursor-pointer"
            >
              {t("common.dontShowAgain")}
            </label>
          </div>
          <AlertDialogFooter>
            <AlertDialogCancel onClick={async () => {
                await updateConfig({dontShowAgain: {...(await getConfig())!.dontShowAgain, editDevice: dontShowAgain}});
                setWarningDialogOpen(false);
              }}
            >
              {t("common.cancel")}
            </AlertDialogCancel>
            <AlertDialogAction
              className="bg-red-600 hover:bg-red-700 text-white"
              onClick={async () => {
                await updateConfig({dontShowAgain: {...(await getConfig())!.dontShowAgain, editDevice: dontShowAgain}});
                setWarningDialogOpen(false);
                setOpen(false);
                deviceDetails.resetForm({ values: device });
              }}
            >
              {t("common.continue")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Sheet>
  );
}

//const CheckSelect = ({ name, checked, onCheckedChange }: { name: string, checked: boolean, onCheckedChange: (checked: boolean) => void }) => {
//  return (
//    <div className="flex items-center space-x-2 flex-1">
//      <Checkbox
//        id={name}
//        checked={checked}
//        onCheckedChange={onCheckedChange}
//      />
//      <Label
//        htmlFor={name}
//        className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:select-none peer-disabled:opacity-70 capitalize"
//      >
//        {name}
//      </Label>
//    </div>
//  );
//};

export function DeleteDevice(props: React.ComponentPropsWithoutRef<typeof Button>) {
  const { t } = useTranslation();
  return (
    <AlertDialog>
      <AlertDialogTrigger asChild>
        <Button
          className="flex-1 bg-red-600 hover:bg-red-700 text-white"
          variant="outline"
          disabled={props.disabled}
        >
          {t("dialogs.removeDevice.trigger")}
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("dialogs.removeDevice.title")}</AlertDialogTitle>
          <AlertDialogDescription>
            {t("dialogs.removeDevice.description")}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>{t("common.cancel")}</AlertDialogCancel>
          <AlertDialogAction
            className="bg-red-600 hover:bg-red-700 text-white"
            onClick={props.onClick}
          >
            {t("common.continue")}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
