import { useContext } from "react";
import { useNavigate } from "react-router-dom";

import { ReactSVG } from "react-svg";
import { LogOut, Trash2, RotateCcw } from "lucide-react";
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

import { AuthProviderContext, updateUser, deleteUser } from "@/components/auth-provider";
import { stopHostedNetwork, removeAllDisplays } from "@/lib/bindings";
import { useTheme } from "@/components/theme-provider";
import { useToast } from "@/components/ui/use-toast";
import { appWindow } from "@tauri-apps/api/window";
import { cn } from "@/lib/utils";

export function ProfileMenu() {
  const { currentUser } = useContext(AuthProviderContext);
  const { setTheme } = useTheme();
  const { dismiss } = useToast();
  const navigate = useNavigate();

  void appWindow.onCloseRequested(async () => {
    deleteUser("");
    await stopHostedNetwork();
    await removeAllDisplays();
    window.otp = "";
    window.hostedNetworkOn = false;
    setTheme("system");
    dismiss();
    navigate("/");
    window.close();
  });

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <AvatarWrapper className="cursor-pointer">
          <ReactSVG src="/src/assets/default.svg" />
        </AvatarWrapper>
      </DropdownMenuTrigger>
      <DropdownMenuContent className="w-56">
        <DropdownMenuLabel>My Account</DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuGroup>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={async () => {
              deleteUser("");
              await stopHostedNetwork();
              await removeAllDisplays();
              window.otp = "";
              window.hostedNetworkOn = false;
              setTheme("system");
              dismiss();
              navigate("/");
            }}
          >
            <LogOut className="mr-2 h-4 w-4" />
            <span>Log Out</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className={cn(
              "cursor-pointer",
              currentUser.length === 0 && "cursor-not-allowed select-none"
            )}
            onClick={() => {
              if (currentUser.length !== 0) {
                updateUser(currentUser, {dontShowAgain: {editDevice: false, removeDevice: false, editNetwork: false}});
              }
            }}
            disabled={currentUser.length === 0}
          >
            <RotateCcw className="mr-2 h-4 w-4" />
            <span>Reset Modal Preferences</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className={cn(
              "cursor-pointer",
              currentUser.length === 0 && "cursor-not-allowed select-none"
            )}
            onClick={event => event.preventDefault()}
            disabled={currentUser.length === 0}
          >
            <Trash2 className="mr-2 h-4 w-4" />
            <AlertDialog open={currentUser.length === 0 ? false : undefined}>
              <AlertDialogTrigger asChild>
                <span style={{ color: "red" }}><b>Delete Account</b></span>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Delete Account</AlertDialogTitle>
                  <AlertDialogDescription>
                    This action cannot be undone. This will permanently delete your account and remove your local preference data.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction
                    className="bg-red-600 hover:bg-red-700 text-white"
                    onClick={async () => {
                      deleteUser(currentUser);
                      await stopHostedNetwork();
                      await removeAllDisplays();
                      window.otp = "";
                      window.hostedNetworkOn = false;
                      setTheme("system");
                      dismiss();
                      navigate("/");
                    }}
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