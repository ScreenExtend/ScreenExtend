import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Sheet,
  SheetClose,
  SheetContent,
  SheetFooter,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from "@/components/ui/sheet";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Slider } from "../ui/slider";
import { Checkbox } from "../ui/checkbox";

export function DeviceDetails() {
  return (
    <Sheet>
      <SheetTrigger asChild>
        <Button variant="outline">Edit Device</Button>
      </SheetTrigger>
      <SheetContent className="min-w-[350px]">
        <SheetHeader>
          <SheetTitle>Device</SheetTitle>
        </SheetHeader>
        <div className="grid gap-4 py-4">
          <div className="flex gap-4">
            <div className="flex-1">
              <Label>Device Name</Label>
              <Input placeholder="Device One" />
            </div>
            <div className="flex-1">
              <Label>Oreintation</Label>
              <Select>
                <SelectTrigger className="w-full">
                  <SelectValue placeholder="Orientation" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="portrait">Portrait</SelectItem>
                  <SelectItem value="landscape">Landscape</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>
          <div>
            <Label>Device IP</Label>
            <Input placeholder="182.167.99.1" />
          </div>
          <div>
            <Label>Device OS</Label>
            <Input placeholder="00-B0-D0-63-C2-26" />
          </div>
          <div>
            <Label>Screen Size</Label>
            <Input placeholder="Living Room" />
          </div>
          <div>
            <Label className="block my-2">Scale</Label>
            <Slider defaultValue={[100]} max={100} step={1} />
          </div>
          <div>
            <Label className="block my-2">Refresh Rate</Label>
            <Slider defaultValue={[33]} max={100} step={1} />
          </div>
          <div className="flex gap-4">
            <CheckSelect name="audio" />
            <CheckSelect name="video" />
          </div>
          <div className="flex gap-4">
            <CheckSelect name="camera" />
            <CheckSelect name="microphone" />
          </div>
          <div className="flex gap-4">
            <CheckSelect name="keyboard" />
            <CheckSelect name="mouse" />
          </div>
          <CheckSelect name="clipboard" />
        </div>
        <SheetFooter>
          <SheetClose asChild>
            <div className="flex gap-4 w-full">
              <Button
                className="flex-1 bg-red-600 hover:bg-red-700"
                type="submit"
              >
                Remove Device
              </Button>
              <Button className="flex-1" type="submit">
                Save changes
              </Button>
            </div>
          </SheetClose>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

const CheckSelect = ({ name }: { name: string }) => {
  return (
    <div className="flex items-center space-x-2 flex-1">
      <Checkbox id={name} />
      <Label
        htmlFor={name}
        className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 capitalize"
      >
        {name}
      </Label>
    </div>
  );
};
