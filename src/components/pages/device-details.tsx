import React, { useState, useContext } from "react";

import { Slider } from "../ui/slider";
import { Checkbox } from "../ui/checkbox";
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

import { AuthProviderContext, updateUser, getUser, type Device } from "@/components/auth-provider";
import { useToast } from "@/components/ui/use-toast";
import { events } from "@/lib/bindings";
import { useFormik } from "formik";

export function DeviceDetails({ device }: { device: Device }) {
  const [open, setOpen] = useState(false);
  const [warningDialogOpen, setWarningDialogOpen] = useState(false);
  const [dontShowAgain, setDontShowAgain] = useState(true);
  const [inProgress, setInProgress] = useState(false);
  const { currentUser } = useContext(AuthProviderContext);
  const { toast } = useToast();

  const deviceDetails = useFormik({
    initialValues: {
      ...device,
    },
    onSubmit: async (values) => {
      setInProgress(true);
      await events.deviceModifyAction.emit(values);
      await new Promise(events.deviceModify.once);
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
      if ((await getUser(currentUser))!.dontShowAgain.editDevice) {
        setOpen(false);
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
        if ((await getUser(currentUser))!.dontShowAgain.editDevice) {
          setOpen(false);
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
                disabled={inProgress}
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
              name="OS"
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
              disabled={inProgress}
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
                  disabled={inProgress}
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
              min={15}
              max={500}
              step={1}
              disabled={inProgress}
            />
          </div>
        </div>
        <SheetFooter>
          <div className="flex gap-4 w-full mt-3">
            <DeleteDevice
              onClick={async () => {
                setInProgress(true);
                await events.deviceRemoveAction.emit(deviceDetails.values);
                await new Promise(events.deviceRemove.once);
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
              className="flex-1 text-white"
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
                await updateUser(currentUser, {dontShowAgain: {...(await getUser(currentUser))!.dontShowAgain, editDevice: dontShowAgain}});
                setWarningDialogOpen(false);
              }}
            >
              Cancel
            </AlertDialogCancel>
            <AlertDialogAction
              className="bg-red-600 hover:bg-red-700 text-white"
              onClick={async () => {
                await updateUser(currentUser, {dontShowAgain: {...(await getUser(currentUser))!.dontShowAgain, editDevice: dontShowAgain}});
                setWarningDialogOpen(false);
                setOpen(false);
                deviceDetails.resetForm();
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
  const [dontShowAgain, setDontShowAgain] = useState(true);

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
