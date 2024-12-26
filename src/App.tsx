import { useState } from "react";
import {
  Route,
  RouterProvider,
  createMemoryRouter,
  createRoutesFromElements,
} from "react-router-dom";

import Login from "@/pages/login";
import Dashboard from "@/pages/dashboard";
import Settings from "@/pages/settings";
import Devices from "@/pages/devices";
import { Loader2 } from "lucide-react";

import { AuthProviderContext, deleteUser } from "@/components/auth-provider";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { ThemeProvider } from "@/components/theme-provider";
import { useTheme } from "@/components/theme-provider";
import { useToast } from "@/components/ui/use-toast";
import { commands } from "@/lib/bindings";
import "non.geist";
const appWindow = getCurrentWebviewWindow();

const router = createMemoryRouter(
  createRoutesFromElements(
    <>
      <Route path="/" element={<Login />} />
      <Route path="/dashboard" element={<Dashboard />} />
      <Route path="/devices" element={<Devices />} />
      <Route path="/settings" element={<Settings />} />
    </>
  )
);

function App() {
  const [currentUser, setCurrentUser] = useState("");

  const [closing, setClosing] = useState(false);
  const { setTheme } = useTheme();
  const { dismiss } = useToast();

  void appWindow.onCloseRequested(async () => {
    setClosing(true);
    await commands.stopHostedNetwork();
    await commands.removeAllDisplays();
    window.otp = "";
    window.hostedNetworkOn = false;
    dismiss();
    await setTheme("system");
    await deleteUser("");
    await appWindow.destroy();
  });

  return (
    <AuthProviderContext.Provider value={{ currentUser, setCurrentUser }}>
      <ThemeProvider defaultTheme="system">
        <RouterProvider router={router} />
        <div className="fixed inset-0 bg-black bg-opacity-80 flex items-center justify-center" style={{ display: closing ? "flex" : "none", zIndex: 9999 }}>
          <div className="rounded-lg p-6 flex flex-col items-center">
            <Loader2 className="animate-spin text-white mb-4" size={48} />
            <p className="text-xl font-semibold text-white">Closing</p>
          </div>
        </div>
      </ThemeProvider>
    </AuthProviderContext.Provider>
  );
}

export default App;
