import React, { useState } from "react";

import { Slider } from "@/components/ui/slider";
import { Checkbox } from "@/components/ui/checkbox";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
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

import { updateConfig, getConfig, type Device } from "@/components/config-provider";
import { useToast } from "@/components/ui/use-toast";
import { commands, events } from "@/lib/bindings";
import { useFormik } from "formik";

export function DeviceDetails({ device }: { device: Device }) {
  const [open, setOpen] = useState(false);
  const [warningDialogOpen, setWarningDialogOpen] = useState(false);
  const [dontShowAgain, setDontShowAgain] = useState(true);
  const [inProgress, setInProgress] = useState(false);
  const [tempRate, setTempRate] = useState(device.refreshRate);
  const [tempQuality, setTempQuality] = useState(device.videoQuality);
  const { toast } = useToast();

  const deviceDetails = useFormik({
    initialValues: {
      ...device,
    },
    onSubmit: async (values) => {
      setInProgress(true);
      const normalized: Device = {
        ...values,
        scale: Number(values.scale),
        refreshRate: Number(values.refreshRate),
        videoScale: Number(values.videoScale),
        videoQuality: Number(values.videoQuality),
      };
      await commands.setDeviceOverride(
        normalized.ip,
        normalized.scale,
        normalized.orientation,
        normalized.refreshRate,
        normalized.videoScale,
        normalized.videoQuality
      );
      await events.deviceModify.emit(normalized);
      setInProgress(false);
      toast({
        title: "Device Settings Updated",
        description: "Your device settings have successfully been updated.",
      });
      setOpen(false);
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
        <div className="py-4">
          <div className="flex">
            <div className="flex-1">
              <Label>Device Name</Label>
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
              <Label>Orientation</Label>
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
            <Label>Device IP</Label>
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
            <Label>Device OS</Label>
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
            <Label>Screen Size</Label>
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
          <div className="mt-4">
            <Label className="block my-2">
              Scale - ({deviceDetails.values.scale}%)
            </Label>
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
            <Label className="my-2 flex items-center">
              Refresh Rate -{" "}
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
            <Label className="block my-2">
              Video Scale - ({deviceDetails.values.videoScale}%)
            </Label>
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
              Video Quality -{" "}
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
            <p className="text-sm text-muted-foreground mt-2">
              Higher values encode faster but lower the quality. Pick the highest value that still looks good to you.
            </p>
          </div>
        </div>
        <SheetFooter>
          <div className="flex w-full mt-3">
            <DeleteDevice
              onClick={async () => {
                setInProgress(true);
                await commands.removeDeviceOverride(device.ip);
                await events.deviceRemove.emit(device);
                setInProgress(false);
                toast({
                  title: "Device Removed",
                  description: "Your device has been successfully removed.",
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
            <AlertDialogTitle>Edit Device</AlertDialogTitle>
            <AlertDialogDescription>
              You have unsaved changes. Clicking continue will discard your edits.
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
              Don't show this message again
            </label>
          </div>
          <AlertDialogFooter>
            <AlertDialogCancel onClick={async () => {
                await updateConfig({dontShowAgain: {...(await getConfig())!.dontShowAgain, editDevice: dontShowAgain}});
                setWarningDialogOpen(false);
              }}
            >
              Cancel
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
              Continue
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
  return (
    <AlertDialog>
      <AlertDialogTrigger asChild>
        <Button
          className="flex-1 bg-red-600 hover:bg-red-700 text-white"
          variant="outline"
          disabled={props.disabled}
        >
          Remove Device
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Remove Device</AlertDialogTitle>
          <AlertDialogDescription>
            This action cannot be undone. The device will immediately disconnect but can reconnect for future sessions.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction
            className="bg-red-600 hover:bg-red-700 text-white"
            onClick={props.onClick}
          >
            Continue
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
