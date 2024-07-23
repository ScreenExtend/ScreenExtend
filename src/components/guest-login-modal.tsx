import { useContext } from "react";
import { useNavigate } from "react-router-dom";

import { Button } from "@/components/ui/button";

import { AuthProviderContext, createUser } from "@/components/auth-provider";
import { useTheme } from "@/components/theme-provider";
import { setup } from "@/lib/bindings";

export function GuestLoginModal() {
  const navigate = useNavigate();
  const { setCurrentUser } = useContext(AuthProviderContext);
  const { theme } = useTheme();

  return (
    <>
      <Button variant="outline" size="sm" className="w-full justify-center" onClick={async () => {
        createUser({username: "", password: "", theme});
        setCurrentUser("");
        await setup();
        navigate("/dashboard");
      }}>
        Login as Guest
      </Button>
      <p style={{ marginTop: "3px" }} className="opacity-75 dark:opacity-50 text-xs">Guest sessions are not saved - use an account to save settings</p>
    </>
  );
}