import { useContext, useState } from "react";
import { useNavigate } from "react-router-dom";

import { Button } from "@/components/ui/button";
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

import { AuthProviderContext, createUser } from "@/components/auth-provider";
import { setup, installDrivers } from "@/lib/bindings";
import { useTheme } from "@/components/theme-provider";

export function GuestLoginModal() {
  const navigate = useNavigate();
  const { setCurrentUser } = useContext(AuthProviderContext);
  const [error, setError] = useState(false);
  const [loading, setLoading] = useState(false);
  const { theme } = useTheme();

  return (
    <>
      <Button variant="outline" size="sm" className="w-full justify-center" onClick={async () => {
        createUser({username: "", password: "", theme});
        setCurrentUser("");
        const success = await setup();
        if (success) {
          navigate("/dashboard");
        } else {
          setError(true);
        }
      }}>
        Login as Guest
      </Button>
      <p style={{ marginTop: "3px" }} className="opacity-75 dark:opacity-50 text-xs">Guest sessions are not saved - use an account to save settings</p>
      <AlertDialog open={error}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Driver Setup Error</AlertDialogTitle>
            <AlertDialogDescription>
              There was an error while attepting to intialize drivers. This often occurs due to the drivers not being installed. <b>Click the button below to install the necessary drivers and certificates.</b> If this error is recurring, contact support at <a href="mailto:support@screenextend.app" target="_blank" style={{ textDecoration: "underline" }}>support@screenextend.app</a>.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={loading} onClick={() => setError(false)}>Go Back</AlertDialogCancel>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={async () => {
                setLoading(true);
                await installDrivers();
                setLoading(false);
                setError(false);
              }}
              disabled={loading}
            >
              Install Drivers
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}