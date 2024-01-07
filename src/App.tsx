import {
  Route,
  RouterProvider,
  createBrowserRouter,
  createRoutesFromElements,
} from "react-router-dom";
import Login from "./pages/login";
import "non.geist";
import Dashboard from "./pages/dashboard";
import Settings from "./pages/settings";
import Devices from "./pages/devices";
import SignUp from "./pages/signup";
import ForgotPassword from "./pages/forgot-password";

const router = createBrowserRouter(
  createRoutesFromElements(
    <>
      <Route path="/" element={<Login />} />
      <Route path="/signup" element={<SignUp />} />
      <Route path="/forgot-password" element={<ForgotPassword />} />
      <Route path="/dashboard" element={<Dashboard />} />
      <Route path="/devices" element={<Devices />} />
      <Route path="/settings" element={<Settings />} />
    </>
  )
);

function App() {
  return <RouterProvider router={router} />;
}

export default App;
