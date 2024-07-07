import { useState } from "react";
import {
  Route,
  RouterProvider,
  createMemoryRouter,
  createRoutesFromElements,
} from "react-router-dom";
import Login from "./pages/login";
import Dashboard from "./pages/dashboard";
import Settings from "./pages/settings";
import Devices from "./pages/devices";
import Terms from "./pages/terms";
import { AuthProviderContext } from "@/components/auth-provider";
import { ThemeProvider } from "@/components/theme-provider";
import "non.geist";

const router = createMemoryRouter(
  createRoutesFromElements(
    <>
      <Route path="/" element={<Login />} />
      {/*<Route path="/signup" element={<SignUp />} />*/}
      <Route path="/dashboard" element={<Dashboard />} />
      <Route path="/devices" element={<Devices />} />
      <Route path="/settings" element={<Settings />} />
      <Route path="/terms" element={<Terms />} />
    </>
  )
);

function App() {
  const [currentUser, setCurrentUser] = useState({username: "", password: ""});

  return (
    <AuthProviderContext.Provider value={{ currentUser, setCurrentUser }}>
      <ThemeProvider defaultTheme="system">
        <RouterProvider router={router} />
      </ThemeProvider>
    </AuthProviderContext.Provider>
  );
}

export default App;


/*
Fixes:
- Save preference for guest login and in general
- Cleaning Up
  - Same indentation everywhere
  - Better variable names
  - Optimize imports
  - Optimize useState actions

Release Actions:
- Tauri config
- Github action tauri automatic build
- Spell check all text
- Copyright at the top of each file (or license)

Metadata:
- https://screenextend.tech/sess/wjduqhsj (build and url)
- Network Name - ScreenExtend{10 random alphanumeric characters} with settable password

Website Fixes:
- Meta tag info for SEO
- Help guide
- Logo with name

Future Fixes:
- Better storage instead of just local storage
- Home screen graphic
- Actual system notifications if window isn't focussed (and notifs for device joining)
- Export/import user data
- Cite tauri and other libraries used + terms and conditions
- Only one instance running (test on mac/linux)
- Ban device by browser/IP
- Resizable side bar

Ffmpeg Usage:
const command = Command.sidecar("ffmpeg", ["-h"]);
const output = await command.execute();
await writeText(output.stdout);

Hosted Network Usage:
console.log(await invoke("start_hosted_network", {ssid: `ScreenExtend${Array.from({length: 5}, () => Math.floor(Math.random() * 10)).join("")}`, password: "screenextend"}));

Get Theme Without State:
((theme === "system") ? ((window.document.documentElement.classList.toString() === "light") ? ("LIGHT") : ("DARK")) : (theme === "light") ? ("LIGHT") : ("DARK"))

Add Attribute to Window (auth-provider.tsx):
declare global {
    interface Window {
        changeSidebarState: () => void;
    }
};

Library Order:
react
react-router-dom
cn
./ ui elements
@ ui elements
any ui element
contexts/etc
*/