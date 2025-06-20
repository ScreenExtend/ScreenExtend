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
import { GlobalProviderContext } from "@/components/global-provider";
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
  const [hostedNetworkOn, setHostedNetworkOn] = useState(false);
  const [otp, setOtp] = useState("");
  const [slug, setSlug] = useState("");
  const [qrValues, setQrValues] = useState([
    {
      title: "Local Hosted Network",
      value: "https://screenextend.app/ascsa",
    },
    {
      title: "Same As Current Device",
      value: "https://screenextend.app/adb",
    },
    {
      title: "Any Wifi Network",
      value: "https://screenextend.app/",
    }
  ] as { title: string; value: string; }[]);
  const [loaded, setLoaded] = useState(false);
  const [authValues, setAuthValues] = useState({ username: "", password: "" });

  const [closing, setClosing] = useState(false);
  const { setTheme } = useTheme();
  const { dismiss } = useToast();

  void appWindow.onCloseRequested(async () => {
    setClosing(true);
    await commands.stopHostedNetwork();
    await commands.removeAllDisplays();
    dismiss();
    await setTheme("system");
    await deleteUser("GUESTGUESTGUESTGUESTGUEST");
    await appWindow.destroy();
  });

  return (
    <GlobalProviderContext.Provider value={{
      windowHostedNetworkOn: [hostedNetworkOn, setHostedNetworkOn],
      windowOtp: [otp, setOtp],
      windowSlug: [slug, setSlug],
      windowQrValues: [qrValues, setQrValues],
      windowLoaded: [loaded, setLoaded],
      windowAuthValues: [authValues, setAuthValues]
    }}>
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
    </GlobalProviderContext.Provider>
  );
}

export default App;
