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
- Notification for various actions (logged in, change account settings, etc)
- On click tool tip show tool tip (also less delay on hover)
- Save preference for guest login
  - Don't show modal again
  - Save preferences on logout (change message too)
- Arrow collapse side bar
- Auto collapse side bar when screen is too small
- Slight white border on textboxes

Release Actions:
- Tauri config
- Github action tauri automatic build

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
- Export/import system data
- Cite tauri and other libraries used + terms and conditions
- Only one instance running (test on mac/linux)

Ffmpeg Usage:
const command = Command.sidecar("ffmpeg", ["-h"]);
const output = await command.execute();
await writeText(output.stdout);

Hosted Network Usage:
console.log(await invoke("start_hosted_network", {ssid: `ScreenExtend${Array.from({length: 5}, () => Math.floor(Math.random() * 10)).join("")}`, password: "screenextend"}));

Get Theme Without State:
((theme === "system") ? ((window.document.documentElement.classList.toString() === "light") ? ("LIGHT") : ("DARK")) : (theme === "light") ? ("LIGHT") : ("DARK"))

Library Order:
react
react-router-dom
cn
./ ui elements
@ ui elements
ui element imports
contexts/etc
*/