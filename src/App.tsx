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
    // @ts-ignore
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
- On logout reset defaults (theme, sidebar length, etc)
- Tauri config
- (better) Home screen graphic
- Help guide
- Logo with name
- Cite tauri and other libraries used
- Only one instance running (test on mac/linux)
- Run on port 5000
- https://screenextend.tech/sess/wjduqhsj (build and url)
- Network Name - ScreenExtend{10 random alphanumeric characters} with settable password
- Github action tauri automatic build
Main Website:
- Fix according to comments
- Meta tag info
((theme === "system") ? ((window.document.documentElement.classList.toString() === "light") ? ("LIGHT") : ("DARK")) : (theme === "light") ? ("LIGHT") : ("DARK")) // needs useTheme()
Future:
- Better storage instead of just local storage
*/