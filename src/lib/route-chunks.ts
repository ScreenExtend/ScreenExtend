// The non-initial routes as dynamic imports, so Rollup gives each its own chunk instead of
// parsing them (and embla, formik and input-otp) before the first frame. A separate module
// because `App.tsx` imports `Bootstrap`, which warms these: otherwise an import cycle.

export const loadDashboard = () => import("@/pages/dashboard");
export const loadDevices = () => import("@/pages/devices");
export const loadSettings = () => import("@/pages/settings");
