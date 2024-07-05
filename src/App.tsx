import {
  Route,
  RouterProvider,
  createMemoryRouter,
  createRoutesFromElements,
} from "react-router-dom";
import Login from "./pages/login";
import "non.geist";
import Dashboard from "./pages/dashboard";
import Settings from "./pages/settings";
import Devices from "./pages/devices";
// import SignUp from "./pages/signup";
import Terms from "./pages/terms";
import { AuthProviderContext } from "@/components/auth-provider";
import { useState } from "react";
import { ThemeProvider } from "@/components/theme-provider";

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
    <AuthProviderContext.Provider value={{currentUser, setCurrentUser}}>
      <ThemeProvider defaultTheme="system">
        <RouterProvider router={router} />
      </ThemeProvider>
    </AuthProviderContext.Provider>
  );
}

export default App;


/*
Things to do:
- Tauri config
- Cite tauri and other libraries used
- Only one instance running (test on mac/linux)
- Run on port 5000
- https://screenextend.tech/sess/wjduqhsj (build and url)
- Network Name - ScreenExtend{10 random alphanumeric characters} with settable password
- Github action tauri automatic build
- Notification for various actions (logged in, change account settings, etc) - on screen
- On click tool tip show tool tip (also less delay on hover)
- Save state for guest login (maybe way to enable again)?
- Auto collapse side bar when screen is too small
- Arrow collapse side bar
- Save QR code size preference
- Clean up code to use same conventions everywhere
- Slight white border on textboxes
- Move currentUser up in order
- Fix minor Typescript errors

const command = Command.sidecar("ffmpeg", ["-h"]);
const output = await command.execute();
await writeText(output.stdout);
console.log(await invoke("start_hosted_network", {ssid: `ScreenExtend${Array.from({length: 5}, () => Math.floor(Math.random() * 10)).join("")}`, password: "screenextend"}));

Main Website:
- Fix according to comments
- Meta tag info
- Help guide
- Logo with name

Future:
- Better storage instead of just local storage
- (better) Home screen graphic
- Change username too?
- Actual system notifications if window isn't focussed (and notifs for device joining)
- Export/import system data

((theme === "system") ? ((window.document.documentElement.classList.toString() === "light") ? ("LIGHT") : ("DARK")) : (theme === "light") ? ("LIGHT") : ("DARK")) // needs useTheme()
*/