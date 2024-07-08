import React, { useState } from "react";

import { Slider } from "../ui/slider";
import { Checkbox } from "../ui/checkbox";
import { Device } from "@/pages/devices";
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

import { useToast } from "@/components/ui/use-toast";
import { useFormik } from "formik";

export function DeviceDetails({ device }: { device: Device }) {
  const [open, setOpen] = useState(false);
  const [warningDialogOpen, setWarningDialogOpen] = useState(false);
  const { toast } = useToast();

  const deviceDetails = useFormik({
    initialValues: {
      ...device,
    },
    onSubmit: (values) => {
      toast({
        title: "Device Settings Updated",
        description: "Your device settings have been updated.",
      });
      setOpen(false);
      void values;
    },
  });

  const considerClosing = (event: CustomEvent<{originalEvent: PointerEvent}> | CustomEvent<{originalEvent: FocusEvent}> | KeyboardEvent) => {
    event.preventDefault();
    if (JSON.stringify(deviceDetails.values) === JSON.stringify(device)) {
      setOpen(false);
    } else {
      setWarningDialogOpen(true);
    }
  };

  const openChangeHandler = (open: boolean) => {
    if (open) setOpen(open);
    if (!open) {
      if (JSON.stringify(deviceDetails.values) === JSON.stringify(device)) {
        setOpen(open);
      } else {
        setWarningDialogOpen(true);
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
        onOpenAutoFocus={(event) => {
          event.preventDefault();
        }}
      >
        <SheetClose asChild />
        <SheetHeader>
          <SheetTitle>Edit Device</SheetTitle>
        </SheetHeader>
        <div className="grid gap-4 py-4">
          <div className="flex gap-4">
            <div className="flex-1">
              <Label>Device Name</Label>
              <Input
                placeholder="Device Name"
                name="name"
                value={deviceDetails.values.name}
                onChange={deviceDetails.handleChange}
                onBlur={deviceDetails.handleBlur}
                hoverLabel={false}
              />
            </div>
            <div className="flex-1">
              <Label>Orientation</Label>
              <Select
                name="orientation"
                defaultValue={deviceDetails.values.orientation}
                onValueChange={(value) => {
                  deviceDetails.setFieldValue("orientation", value);
                }}
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
          <div>
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
          <div>
            <Label>Device OS</Label>
            <Input
              disabled={true}
              placeholder="00-B0-D0-63-C2-26"
              name={device.os}
              value={deviceDetails.values.os}
              onChange={deviceDetails.handleChange}
              onBlur={deviceDetails.handleBlur}
              hoverLabel={false}
            />
          </div>
          <div>
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
          <div>
            <Label className="block my-2">
              Scale - ({deviceDetails.values.scale}%)
            </Label>
            <Slider
              defaultValue={[deviceDetails.values.scale]}
              onValueChange={(value) => {
                deviceDetails.setFieldValue("scale", value);
              }}
              min={25}
              max={200}
              step={25}
            />
          </div>
          <div>
            <Label className="my-2 flex items-center gap-1">
              Refresh Rate -{" "}
              <div className="flex items-center gap-1">
                <Input
                  name="refreshRate"
                  type="number"
                  min={60}
                  max={360}
                  step={1}
                  value={deviceDetails.values.refreshRate}
                  onChange={(event) => {
                    deviceDetails.setFieldValue(
                      "refreshRate",
                      event.target.value
                    );
                  }}
                  className="w-10 px-1 text-center"
                  hoverLabel={false}
                />{" "}
                Hz
              </div>
            </Label>
            <Slider
              value={[deviceDetails.values.refreshRate]}
              defaultValue={[deviceDetails.values.refreshRate]}
              onValueChange={(value) => {
                deviceDetails.setFieldValue("refreshRate", value);
              }}
              min={60}
              max={360}
              step={1}
            />
          </div>
          <div className="flex gap-4">
            <CheckSelect
              name="audio"
              checked={deviceDetails.values.isAudioActive}
              onCheckedChange={(checked) => {
                deviceDetails.setFieldValue("isAudioActive", checked);
              }}
            />
            <CheckSelect
              name="video"
              checked={deviceDetails.values.isVedioActive}
              onCheckedChange={(checked) => {
                deviceDetails.setFieldValue("isVedioActive", checked);
              }}
            />
          </div>
          <div className="flex gap-4">
            <CheckSelect
              name="camera"
              checked={deviceDetails.values.isCameraActive}
              onCheckedChange={(checked) => {
                deviceDetails.setFieldValue("isCameraActive", checked);
              }}
            />
            <CheckSelect
              name="microphone"
              checked={deviceDetails.values.isMicrophoneActive}
              onCheckedChange={(checked) => {
                deviceDetails.setFieldValue("isMicrophoneActive", checked);
              }}
            />
          </div>
          <div className="flex gap-4">
            <CheckSelect
              name="keyboard"
              checked={deviceDetails.values.isKeyboardActive}
              onCheckedChange={(checked) => {
                deviceDetails.setFieldValue("isKeyboardActive", checked);
              }}
            />
            <CheckSelect
              name="mouse"
              checked={deviceDetails.values.isMouseActive}
              onCheckedChange={(checked) => {
                deviceDetails.setFieldValue("isMouseActive", checked);
              }}
            />
          </div>
          <CheckSelect
            name="clipboard"
            checked={deviceDetails.values.isClipboardActive}
            onCheckedChange={(checked) => {
              deviceDetails.setFieldValue("isClipboardActive", checked);
            }}
          />
        </div>
        <SheetFooter>
          <div className="flex gap-4 w-full">
            <DeleteDevice
              onClick={() => {
                toast({
                  title: "Device removed",
                  description: "Your device has been removed from the list.",
                });
                setOpen(false);
              }}
            />
            <Button
              className="flex-1 text-white"
              type="submit"
              onClick={() => {
                deviceDetails.handleSubmit();
              }}
            >
              Save changes
            </Button>
          </div>
        </SheetFooter>
        <CloseConfirmationDialog
          isOpen={warningDialogOpen}
          isOpenHandler={setWarningDialogOpen}
          acceptWarning={() => {
            setWarningDialogOpen(false);
            setOpen(false);
            deviceDetails.resetForm();
          }}
          declineWarning={() => {
            setWarningDialogOpen(false);
          }}
        />
      </SheetContent>
    </Sheet>
  );
}

const CheckSelect = ({ name, checked, onCheckedChange }: { name: string, checked: boolean, onCheckedChange: (checked: boolean) => void }) => {
  return (
    <div className="flex items-center space-x-2 flex-1">
      <Checkbox
        id={name}
        checked={checked}
        onCheckedChange={onCheckedChange}
      />
      <Label
        htmlFor={name}
        className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 capitalize"
      >
        {name}
      </Label>
    </div>
  );
};

export function DeleteDevice(props: React.ComponentPropsWithoutRef<typeof Button>) {
  const [dontShowAgain, setDontShowAgain] = useState(true);

  return (
    <AlertDialog>
      <AlertDialogTrigger asChild>
        <Button
          className="flex-1 bg-red-600 hover:bg-red-700 text-white"
          variant="outline"
        >
          Remove Device
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Remove Device</AlertDialogTitle>
          <AlertDialogDescription>
            This action cannot be undone. The device will immediately lose be disconnect. It can reconnect for future sessions.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <div className="flex items-center space-x-2 mb-4">
            <Checkbox
                id="dontShowAgain"
                checked={dontShowAgain}
                onCheckedChange={(checked) => setDontShowAgain(checked === true)}
            />
            <label
                htmlFor="dontShowAgain"
                className="text-sm text-muted-foreground cursor-pointer"
            >
                Don't show this message again
            </label>
        </div>
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

function CloseConfirmationDialog({ isOpen, isOpenHandler, acceptWarning, declineWarning }: { isOpen: boolean, acceptWarning: () => void, declineWarning: () => void, isOpenHandler: (isOpen: boolean) => void }) {
  const [dontShowAgain, setDontShowAgain] = useState(true);

  return (
    <AlertDialog open={isOpen} onOpenChange={isOpenHandler}>
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
            onCheckedChange={(checked) => setDontShowAgain(checked === true)}
          />
          <label
            htmlFor="dontShowAgain"
            className="text-sm text-muted-foreground cursor-pointer"
          >
            Don't show this message again
          </label>
        </div>
        <AlertDialogFooter>
          <AlertDialogCancel onClick={declineWarning}>Cancel</AlertDialogCancel>
          <AlertDialogAction
            className="bg-red-600 hover:bg-red-700 text-white"
            onClick={acceptWarning}
          >
            Continue
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
