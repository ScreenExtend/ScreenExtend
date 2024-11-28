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
Tasks:
- Build up app/social media
  - Ask the airdrop guy
- Build website
  - Rate limit client after 5 tries
  - For client - dialog page from modal example, otp input, full screen webrtc on interaction (https://github.com/redpangilinan/credenza.git)
  - Number input for OTP on iOS
  - Disable buttons on modal until action is done
  - Meta tag info for SEO
- Push code
  - Move as much code out of main.rs as possible
  - Describe LICENSE
  - Write README
    - Future changelog
  - Spell check all text
  - Add Copyright at top of each file
  - Run SourceGraph - compare code against public repos and check
  - Run GitGaurdian - ensure no api keys or other private information is being pushed
- Release Build
  - Remove ts::export on build and remove command line for non-windows
  - Optimize dependencies and Tauri config
  - Use Github action tauri automatic build
- Main app
  - Edit device
  - WebRTC + website server
  - Other platform implementations

device:
setup()
startHostedNetwork(name: string, password: string)
stopHostedNetwork()
installDrivers()
createDisplay(config: VirtualDisplayConfig)
updateDisplay(displayId: number, config: VirtualDisplayConfig)
removeDisplay(displayId: number)
removeAllDisplays()

type VirtualDisplayConfig = { name: string; width: number; height: number; refresh_rate: number }

global:
getPrivateIpAddresses()

testing:
fetchUrls()
getDevices()

Future Fixes:
- GPU video encoding support
- Add option to disable public screenextend.app sessions
- Better storage instead of just local storage
- Home screen graphic
- Actual system notifications if window isn't focussed
- Export/import user data
- Only one instance running IMPORTANT
- Uninstall script drivers IMPORTANT
- Ban device by browser/IP
- Resizable side bar
- HDR Support: https://github.com/itsmikethetech/Virtual-Display-Driver, BetterDisplay
- Remote control features: keyboard (+clipboard), mouse
- Bluetooth support
- Audio support
- Standardize async mutex support
- Remote diagnostics
- Auto updates
- Walkthrough app with Joyride:
import Joyride from "react-joyride";
callback={console.log}
continuous
run
scrollToFirstStep
showProgress
showSkipButton

Get Theme Without State:
window.document.documentElement.classList.toString()

Library Order:
react
react-router-dom
cn
@ ui elements
any ui element
contexts/etc
*/