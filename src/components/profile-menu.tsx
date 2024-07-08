import { useContext } from "react";
import { useNavigate } from "react-router-dom";

import { LogOut, Trash2 } from "lucide-react";
import { Avatar, AvatarFallback, AvatarImage } from "@/components/ui/avatar";
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

import { useTheme } from "@/components/theme-provider";
import { AuthProviderContext } from "@/components/auth-provider";
import { cn } from "@/lib/utils";
import defaultUser from "@/assets/default.jpg";

export function ProfileMenu() {
  const { currentUser } = useContext(AuthProviderContext);
  const { setTheme } = useTheme();
  const navigate = useNavigate();

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Avatar className="cursor-pointer">
          <AvatarImage src={defaultUser} alt="@shadcn" />
          <AvatarFallback>User</AvatarFallback>
        </Avatar>
      </DropdownMenuTrigger>
      <DropdownMenuContent className="w-56">
        <DropdownMenuLabel>My Account</DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuGroup>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={() => {
              setTheme("system");
              Object.keys(localStorage).filter(x => x.startsWith("-")).forEach(x => localStorage.removeItem(x));
              navigate("/");
            }}
          >
            <LogOut className="mr-2 h-4 w-4" />
            <span>Log Out</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className={cn(
              "cursor-pointer",
              currentUser.username.length === 0 && "cursor-not-allowed"
            )}
            onClick={(event) => event.preventDefault()}
            disabled={currentUser.username.length === 0}
          >
            <Trash2 className="mr-2 h-4 w-4" />
            <AlertDialog open={currentUser.username.length === 0 ? false : undefined}>
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
                    onClick={() => {
                      Object.keys(localStorage).filter(x => x.startsWith(currentUser.username + "-")).forEach(x => localStorage.removeItem(x));
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