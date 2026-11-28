// One unit for every refresh and frame rate the app shows, so a display and a device read alike
export function formatHz(hz: number | null | undefined): string {
  if (hz == null || !Number.isFinite(hz) || hz <= 0) return "";
  return `${Math.round(hz)} Hz`;
}
