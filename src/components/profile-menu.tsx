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

import { GlobalProviderContext, GlobalContextDefault } from "@/components/global-provider";
import { AuthProviderContext, updateUser, deleteUser } from "@/components/auth-provider";
import { commands } from "@/lib/bindings";
import { useTheme } from "@/components/theme-provider";
import { useToast } from "@/components/ui/use-toast";
import defaultLogo from "@/assets/default.svg";
import { cn } from "@/lib/utils";

export function ProfileMenu() {
  const { currentUser } = useContext(AuthProviderContext);
  const { setTheme } = useTheme();
  const { dismiss } = useToast();
  const navigate = useNavigate();
  const { windowAuthValues: [, setAuthValues], windowLoaded: [, setLoaded], windowOtp: [, setOtp], windowHostedNetworkOn: [, setHostedNetworkOn], windowSlug: [, setSlug], windowQrValues: [, setQrValues] } = useContext(GlobalProviderContext);

  const logout = async () => {
    await commands.stopHostedNetwork();
    await commands.removeAllDisplays();
    setHostedNetworkOn(GlobalContextDefault.hostedNetworkOn);
    setOtp(GlobalContextDefault.otp);
    setSlug(GlobalContextDefault.slug);
    setQrValues(GlobalContextDefault.qrValues);
    setLoaded(GlobalContextDefault.loaded);
    setAuthValues(GlobalContextDefault.authValues);
    await setTheme("system");
    dismiss();
  };

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <AvatarWrapper className="cursor-pointer">
          <ReactSVG src={defaultLogo} />
        </AvatarWrapper>
      </DropdownMenuTrigger>
      <DropdownMenuContent className="w-56">
        <DropdownMenuLabel>My Account</DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuGroup>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={async () => {
              await logout();
              await deleteUser("GUESTGUESTGUESTGUESTGUEST");
              navigate("/");
            }}
          >
            <LogOut className="mr-2 h-4 w-4" />
            <span>Log Out</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className={cn(
              "cursor-pointer",
              currentUser === "GUESTGUESTGUESTGUESTGUEST" && "cursor-not-allowed select-none"
            )}
            onClick={async () => {
              if (currentUser.length !== 0) {
                await updateUser(currentUser, {dontShowAgain: {editDevice: false, editNetwork: false}});
              }
            }}
            disabled={currentUser === "GUESTGUESTGUESTGUESTGUEST"}
          >
            <RotateCcw className="mr-2 h-4 w-4" />
            <span>Reset Modal Preferences</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className={cn(
              "cursor-pointer",
              currentUser === "GUESTGUESTGUESTGUESTGUEST" && "cursor-not-allowed select-none"
            )}
            onClick={event => event.preventDefault()}
            disabled={currentUser === "GUESTGUESTGUESTGUESTGUEST"}
          >
            <Trash2 className="mr-2 h-4 w-4" />
            <AlertDialog open={currentUser === "GUESTGUESTGUESTGUESTGUEST" ? false : undefined}>
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
                      await logout();
                      await deleteUser(currentUser);
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
