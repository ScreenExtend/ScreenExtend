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
      <Route path="/dashboard" element={<Dashboard />} />
      <Route path="/devices" element={<Devices />} />
      <Route path="/settings" element={<Settings />} />
      <Route path="/terms" element={<Terms />} />
    </>
  )
);

function App() {
  const [currentUser, setCurrentUser] = useState("");

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
- Add option to disable public screenextend.app sessions
- Rate limit server (password)
- Install drivers dialog
- Only push main.rs after checking
- For client - dialog page from modal example, otp input, full screen webrtc on interaction (https://github.com/redpangilinan/credenza.git)

Release Actions:
- Tauri config
- Github action tauri automatic build
- Spell check all text
- Copyright at the top of each file (or license)
- Cite tauri and other libraries used + terms and conditions
- Post install script for drivers
- Run SourceGraph - compare code against public repos and check
- Run GitGaurdian - ensure no api keys or other private information is being pushed

Website Fixes:
- Meta tag info for SEO
- Help guide
- Logo with name

Future Fixes:
- Better storage instead of just local storage
- Home screen graphic
- Actual system notifications if window isn't focussed
- Export/import user data
- Only one instance running
- Ban device by browser/IP
- Resizable side bar
- HDR Support: https://github.com/itsmikethetech/Virtual-Display-Driver, BetterDisplay
- Remote control features: keyboard (+clipboard), mouse
- Bluetooth support
- Audio support
- Standardize async mutex support
- Auto updates
- Walkthrough app with Joyride:
import Joyride from "react-joyride";
callback={console.log}
continuous
run
scrollToFirstStep
showProgress
showSkipButton

Ffmpeg Usage:
const command = Command.sidecar("ffmpeg", ["-h"]);
const output = await command.execute();
await writeText(output.stdout);

Get Theme Without State:
window.document.documentElement.classList.toString()

Library Order:
react
react-router-dom
cn
./ ui elements
@ ui elements
any ui element
contexts/etc
*/