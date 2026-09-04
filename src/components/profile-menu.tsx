import { useContext, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useNextStep } from "nextstepjs";

import { Power, Trash2, RotateCcw, Compass } from "lucide-react";
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

import { updateConfig, flushConfig, useConfig } from "@/components/config-provider";
import { WALKTHROUGH_TOUR } from "@/components/walkthrough";
import { GlobalProviderContext } from "@/components/global-provider";
import { useToast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { commands } from "@/lib/bindings";
import defaultLogo from "@/assets/default.svg";

export function ProfileMenu() {
  const { windowClosing: [closing, setClosing], windowAvatar: [avatar] } = useContext(GlobalProviderContext);
  const [background, setBackground] = useState(false);
  const name = useConfig()?.name ?? "";
  const navigate = useNavigate();
  const { startNextStep } = useNextStep();
  const { toast } = useToast();
  const { t } = useTranslation();

  const replayTour = async () => {
    try {
      await updateConfig({ walkthroughCompleted: false });
      await flushConfig();
    } catch (e) {
      console.error("failed to reset the walkthrough state", e);
      toast({
        variant: "destructive",
        title: t("toasts.replayTour.failureTitle"),
        description: t("toasts.replayTour.failureDescription"),
      });
      return;
    }
    navigate("/dashboard");
    startNextStep(WALKTHROUGH_TOUR);
  };

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
              try {
                await commands.stopHostedNetwork();
              } catch (e) {
                console.error("stopHostedNetwork on exit failed", e);
              }
              try {
                await commands.exitApp();
              } catch (e) {
                console.error("exitApp failed", e);
                setClosing(false);
                toast({
                  variant: "destructive",
                  title: t("toasts.exitApp.failureTitle"),
                  description: t("toasts.exitApp.failureDescription"),
                });
              }
            }}
          >
            <Power className="mr-2 h-4 w-4" />
            <span>Exit App</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={async () => {
              try {
                await updateConfig({dontShowAgain: {editDevice: false, editNetwork: false, compatibility: false, configEditor: false}});
                await flushConfig();
                toast({
                  title: t("toasts.resetPreferences.successTitle"),
                  description: t("toasts.resetPreferences.successDescription"),
                });
              } catch (e) {
                console.error("failed to reset preferences", e);
                toast({
                  variant: "destructive",
                  title: t("toasts.resetPreferences.failureTitle"),
                  description: t("toasts.resetPreferences.failureDescription"),
                });
              }
            }}
          >
            <RotateCcw className="mr-2 h-4 w-4" />
            <span>Reset Preferences</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={() => { void replayTour(); }}
          >
            <Compass className="mr-2 h-4 w-4" />
            <span>Replay Tour</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            className="cursor-pointer"
            onClick={async () => {
              setClosing(true);
              try {
                const outcome = await commands.uninstallAudioDriver();
                if (outcome === "cancelled" || outcome === "failed") {
                  setClosing(false);
                  toast({
                    variant: "destructive",
                    title: t("toasts.uninstallDrivers.failureTitle"),
                    description: t("toasts.uninstallDrivers.failureDescription"),
                  });
                  return;
                }
                await commands.removeDrivers();
              } catch (e) {
                console.error("failed to uninstall drivers", e);
                setClosing(false);
                toast({
                  variant: "destructive",
                  title: t("toasts.uninstallDrivers.failureTitle"),
                  description: t("toasts.uninstallDrivers.failureDescription"),
                });
                return;
              }
              try {
                await commands.stopHostedNetwork();
              } catch (e) {
                console.error("stopHostedNetwork on driver uninstall failed", e);
              }
              try {
                await commands.exitApp();
              } catch (e) {
                console.error("exitApp failed", e);
                setClosing(false);
                toast({
                  variant: "destructive",
                  title: t("toasts.exitApp.failureTitle"),
                  description: t("toasts.exitApp.failureDescription"),
                });
              }
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
