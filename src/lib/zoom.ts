import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

const appWindow = getCurrentWebviewWindow();

export const ZOOM_STEPS = [0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2];
export const DEFAULT_ZOOM = 1;
export const MIN_ZOOM = ZOOM_STEPS[0];
export const MAX_ZOOM = ZOOM_STEPS[ZOOM_STEPS.length - 1];

export const clampZoom = (z: number): number =>
  Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, z));

const nearestIndex = (z: number): number => {
  let best = 0;
  let bestDiff = Infinity;
  ZOOM_STEPS.forEach((step, i) => {
    const diff = Math.abs(step - z);
    if (diff < bestDiff) {
      bestDiff = diff;
      best = i;
    }
  });
  return best;
};

export const zoomIn = (z: number): number =>
  ZOOM_STEPS[Math.min(ZOOM_STEPS.length - 1, nearestIndex(z) + 1)];

export const zoomOut = (z: number): number =>
  ZOOM_STEPS[Math.max(0, nearestIndex(z) - 1)];

export const formatZoom = (z: number): string => `${Math.round(z * 100)}%`;

export const applyZoom = (z: number): Promise<void> => appWindow.setZoom(clampZoom(z));
