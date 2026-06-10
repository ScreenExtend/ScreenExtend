import { useContext, useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";

import { Loader2 } from "lucide-react";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle
} from "@/components/ui/alert-dialog";

import { AuthProviderContext, createUser, getUser } from "@/components/auth-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { commands, events } from "@/lib/bindings";
import { buildQrValues } from "@/lib/utils";
import { useTheme, type Theme } from "@/components/theme-provider";

export default function Bootstrap() {
  const { setCurrentUser } = useContext(AuthProviderContext);
  const { theme, setTheme } = useTheme();
  const { windowLoaded: [loaded, setLoaded], windowOtp: [, setOtp], windowHostedNetworkOn: [, setHostedNetworkOn], windowSessionId: [, setSessionId], windowQrValues: [, setQrValues] } = useContext(GlobalProviderContext);

  const [error, setError] = useState(false);
  const [loading, setLoading] = useState(false);
  const [firstTime, setFirstTime] = useState(true);
  const running = useRef(false);

  const start = async () => {
    let success;
    if (!loaded) {
      success = await commands.setup();
      setLoaded(success);
    } else {
      success = loaded;
    }
    if (success) {
      await commands.watchForNetworkChanges();
      const newSessionId = Array.from(crypto.getRandomValues(new Uint8Array(12)), b => '23456789ABCDEFGHJKLMNPQRSTUVWXYZ'[b % 32]).join('');
      const newOtp = [...Array(6)].reduce(a => a + "0123456789"[~~(Math.random() * "0123456789".length)], "");
      setSessionId(newSessionId);
      setOtp(newOtp);
      await commands.setSessionCredentials(newSessionId, newOtp);
      setQrValues(await buildQrValues(newSessionId));
      await events.networkChange.listen(async () => {
        setQrValues(await buildQrValues(newSessionId));
      });
      setHostedNetworkOn(false);
      const username = await commands.getUsername();
      const existing = await getUser(username);
      if (!existing) {
        await createUser({ username, name: username, password: "", theme });
      } else {
        setTheme(existing.theme as Theme);
      }
      setCurrentUser(username);
      await commands.setCurrentUser(username);
      document.getElementById("dashlink")!.click();
    } else {
      if (firstTime) {
        await commands.installDrivers();
        await new Promise(resolve => setTimeout(resolve, 5000));
        setFirstTime(false);
        start();
      } else {
        setError(true);
      }
    }
  };

  useEffect(() => {
    if (running.current) return;
    running.current = true;
    void start();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="h-screen w-full flex flex-col items-center justify-center">
      <Link to="/dashboard" id="dashlink"></Link>
      <Loader2 className="animate-spin mb-4" size={48} />
      <p className="text-xl font-semibold">Starting ScreenExtend</p>
      <AlertDialog open={error}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Setup Error</AlertDialogTitle>
            <AlertDialogDescription>
              There was an error while attepting to start ScreenExtend. This often occurs due to core drivers or libraries not being installed. <b>Click the button below to install the missing components.</b> If this error is recurring, contact support at <a href="mailto:support@screenextend.app" target="_blank" style={{ textDecoration: "underline" }}>support@screenextend.app</a>.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
              onClick={async () => {
                setLoading(true);
                await commands.installDrivers();
                await new Promise(resolve => setTimeout(resolve, 5000));
                setLoading(false);
                setError(false);
                await start();
              }}
              disabled={loading}
            >
              Install Drivers
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
