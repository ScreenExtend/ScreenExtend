import { useEffect, useState } from "react";
import {
  Route,
  RouterProvider,
  createMemoryRouter,
  createRoutesFromElements,
} from "react-router-dom";

import Bootstrap from "@/pages/bootstrap";
import Dashboard from "@/pages/dashboard";
import Settings from "@/pages/settings";
import Devices from "@/pages/devices";
import { Loader2 } from "lucide-react";

import { AuthProviderContext, type Device } from "@/components/auth-provider";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { GlobalProviderContext } from "@/components/global-provider";
import { ThemeProvider } from "@/components/theme-provider";
import { commands, events } from "@/lib/bindings";
import "non.geist";
const appWindow = getCurrentWebviewWindow();

const router = createMemoryRouter(
  createRoutesFromElements(
    <>
      <Route path="/" element={<Bootstrap />} />
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
  const [sessionId, setSessionId] = useState("");
  const [qrValues, setQrValues] = useState([] as { title: string; value: string; }[]);
  const [loaded, setLoaded] = useState(false);
  const [authValues, setAuthValues] = useState({ username: "", password: "" });
  const [devices, setDevices] = useState<Device[]>([]);

  const [closing, setClosing] = useState(false);

  useEffect(() => {
    if (sessionId && otp) {
      void commands.setSessionCredentials(sessionId, otp);
    }
  }, [sessionId, otp]);

  useEffect(() => {
    const unlisteners: (() => void)[] = [];
    const start_listener = async () => {
      unlisteners.push(await events.deviceJoin.listen(event => {
        const device = event.payload as Device;
        setDevices(prev => {
          const next = prev.filter(d => d.ip !== device.ip);
          next.push(device);
          return next;
        });
      }));
      unlisteners.push(await events.deviceModify.listen(event => {
        const device = event.payload as Device;
        setDevices(prev => prev.map(d => d.ip === device.ip ? device : d));
      }));
      unlisteners.push(await events.deviceRemove.listen(event => {
        const device = event.payload as Device;
        setDevices(prev => prev.filter(d => d.ip !== device.ip));
      }));
    }
    void start_listener();
    return () => unlisteners.forEach(unlisten => unlisten());
  }, []);

  void appWindow.onCloseRequested(async () => {
    setClosing(true);
    await commands.stopHostedNetwork();
    await commands.exitApp();
  });

  return (
    <GlobalProviderContext.Provider value={{
      windowHostedNetworkOn: [hostedNetworkOn, setHostedNetworkOn],
      windowOtp: [otp, setOtp],
      windowSessionId: [sessionId, setSessionId],
      windowQrValues: [qrValues, setQrValues],
      windowLoaded: [loaded, setLoaded],
      windowAuthValues: [authValues, setAuthValues],
      windowClosing: [closing, setClosing],
      windowDevices: [devices, setDevices]
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
