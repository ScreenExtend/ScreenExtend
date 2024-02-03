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
//import SignUp from "./pages/signup";
import Terms from "./pages/terms";

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
  return <RouterProvider router={router} />;
}

export default App;


/* Things to do:
- Tauri config
- (better) Home screen graphic
- Help guide
- Logo with name
- Cite user icon and Qudusayo
- Dark mode on change
- System dark mode window theme
- Get dark mode and other html ones better natively
*/