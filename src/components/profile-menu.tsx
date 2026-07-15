import { useContext, useEffect, useState } from "react";

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

import { updateConfig, getConfig } from "@/components/config-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { commands } from "@/lib/bindings";
import defaultLogo from "@/assets/default.svg";

export function ProfileMenu() {
  const { windowClosing: [closing, setClosing], windowAvatar: [avatar] } = useContext(GlobalProviderContext);
  const [background, setBackground] = useState(false);
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
          <img src={avatar ?? defaultLogo} alt="Profile" className="h-full w-full object-cover" />
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
              await updateConfig({dontShowAgain: {editDevice: false, editNetwork: false, compatibility: false}});
            }}
          >
            <RotateCcw className="mr-2 h-4 w-4" />
            <span>Reset Preferences</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={async () => {
              setClosing(true);
              await commands.removeDrivers();
              await commands.stopHostedNetwork();
              await commands.exitApp();
            }}
          >
            <Trash2 className="mr-2 h-4 w-4" />
            <span style={{ color: "red" }}><b>Uninstall Drivers</b></span>
          </DropdownMenuItem>
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
