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
