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

import { AuthProviderContext, updateUser, getUser } from "@/components/auth-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { commands } from "@/lib/bindings";
import defaultLogo from "@/assets/default.svg";

export function ProfileMenu() {
  const { currentUser } = useContext(AuthProviderContext);
  const { windowClosing: [, setClosing] } = useContext(GlobalProviderContext);
  const [disabled, setDisabled] = useState(false);
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");

  useEffect(() => {
    void (async () => {
      const user = await getUser(currentUser);
      if (user) setName(user.name);
    })();
  }, [currentUser]);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <AvatarWrapper className="cursor-pointer">
          <ReactSVG src={defaultLogo} />
        </AvatarWrapper>
      </DropdownMenuTrigger>
      <DropdownMenuContent className="w-56">
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
              if (currentUser.length !== 0) {
                await updateUser(currentUser, {dontShowAgain: {editDevice: false, editNetwork: false}});
              }
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
