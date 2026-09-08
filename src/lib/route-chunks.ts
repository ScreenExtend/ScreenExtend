// The three non-initial routes, as dynamic imports so Rollup gives each one its own chunk
// instead of parsing all of them (and embla, formik and input-otp with them) before the
// first frame. Kept in their own module because `App.tsx` imports `Bootstrap` and
// `Bootstrap` needs to warm these, which would otherwise be an import cycle.

export const loadDashboard = () => import("@/pages/dashboard");
export const loadDevices = () => import("@/pages/devices");
export const loadSettings = () => import("@/pages/settings");
