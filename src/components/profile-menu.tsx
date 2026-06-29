import { useContext, useEffect, useState } from "react";

import { ReactSVG } from "react-svg";
import { Power, Trash2, RotateCcw } from "lucide-react";
import { Avatar as AvatarWrapper } from "@/components/ui/avatar";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
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

import { updateConfig, getConfig } from "@/components/config-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { commands } from "@/lib/bindings";
import defaultLogo from "@/assets/default.svg";

export function ProfileMenu() {
  const { windowClosing: [closing, setClosing] } = useContext(GlobalProviderContext);
  const [disabled, setDisabled] = useState(false);
  const [background, setBackground] = useState(false);
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");

  useEffect(() => {
    void (async () => {
      const config = await getConfig();
      if (config) setName(config.name);
    })();
  }, []);

  return (
    <DropdownMenu onOpenChange={setBackground}>
      <div
        aria-hidden="true"
        className={`fixed top-0 right-0 bottom-0 left-0 bg-black bg-opacity-80 flex items-center justify-center transition-opacity duration-200 ease-out ${
          background && !closing ? "opacity-100" : "pointer-events-none opacity-0"
        }`}
        style={{ zIndex: 9999 }}
      />
      <DropdownMenuTrigger asChild>
        <AvatarWrapper className="cursor-pointer">
          <ReactSVG src={defaultLogo} />
        </AvatarWrapper>
      </DropdownMenuTrigger>
      <DropdownMenuContent className="w-56 z-[99999] mr-4">
        <DropdownMenuLabel>{name || "My Account"}</DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuGroup>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={async () => {
              setClosing(true);
              await commands.stopHostedNetwork();
              await commands.exitApp();
            }}
          >
            <Power className="mr-2 h-4 w-4" />
            <span>Exit App</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={async () => {
              await updateConfig({dontShowAgain: {editDevice: false, editNetwork: false}});
            }}
          >
            <RotateCcw className="mr-2 h-4 w-4" />
            <span>Reset Preferences</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={(event: React.MouseEvent<HTMLDivElement>) => {
              event.preventDefault();
            }}
          >
            <Trash2 className="mr-2 h-4 w-4" />
            <AlertDialog open={open}>
              <AlertDialogTrigger asChild onClick={(event: React.MouseEvent<HTMLButtonElement>) => {
                event.preventDefault();
                setDisabled(false);
                setOpen(true);
              }}>
                <span style={{ color: "red" }}><b>Uninstall Drivers</b></span>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Uninstall Drivers</AlertDialogTitle>
                  <AlertDialogDescription>
                    This will remove ScreenExtend drivers and associated local data from this computer. This action cannot be undone.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel disabled={disabled} className="disabled:cursor-not-allowed disabled:select-none disabled:opacity-50" onClick={() => setOpen(false)}>Cancel</AlertDialogCancel>
                  <AlertDialogAction
                    className="bg-red-600 hover:bg-red-700 text-white disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
                    onClick={async (event: React.MouseEvent<HTMLButtonElement>) => {
                      event.preventDefault();
                      setDisabled(true);
                      await commands.removeDrivers();
                      setOpen(false);
                      setClosing(true);
                      await commands.stopHostedNetwork();
                      await commands.exitApp();
                    }}
                    disabled={disabled}
                  >
                    Continue
                  </AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          </DropdownMenuItem>
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
