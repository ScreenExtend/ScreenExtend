// The shape of `config.json` and its defaults, with no imports of any kind.
//
// It was inside `config-provider.tsx`, next to the `tauri-plugin-store` calls that read and
// write it. Splitting it out is what lets two other things see the shape without dragging
// React and the Tauri IPC in with it: `config-schema.ts`, which describes the same keys to
// the Settings editor, and `scripts/test-config-editor.mjs`, which cross-checks the two
// under plain `node` (type stripping only -- no JSX, no bundler). The schema drifting from
// this file is the failure that shipped: `stunUrls` was added here and not there, and the
// editor then declared the user's own config invalid.
//
// `config-provider.tsx` re-exports everything below, so no call site imports from here.

export type PointerScope = "display" | "all";
export const DEFAULT_POINTER_SCOPE: PointerScope = "display";

export type Device = {
  ip: string;
  token: string;
  name: string;
  scale: number;
  orientation: "Portrait" | "Landscape";
  refreshRate: number;
  videoScale: number;
  videoQuality: number;
  remoteControl: boolean;
  /**
   * Where a pointer-locked client's cursor may travel. Both input backends track their own
   * position and clamp it, but today they clamp to the union of every attached display, so
   * accumulated `MOUSE_DELTA` walks the cursor onto screens the client cannot see and can
   * then click on. `display` keeps it on the session's own display; `all` is the old
   * behaviour, and it is what dragging a window back off the virtual display needs.
   *
   * Optional because the field post-dates the devices already in `config.json`; a device
   * saved before it reads as `display` through `DEFAULT_POINTER_SCOPE`.
   */
  pointerScope?: PointerScope;
  systemAudio: boolean;
  audioOutputDeviceId: string;
  audioOutputDeviceLabel: string;
  os: string;
  screenSize: string;
  dpr: number;
  maxDpr: number;
};

export type KnownDevice = {
  token: string;
  ip: string;
  name: string;
  os: string;
  screenSize: string;
  lastSeen: number;
  banned: boolean;
};

export type Config = {
  name: string,
  theme: string,
  devices: Device[],
  knownDevices: KnownDevice[],
  sessionPassword: string,
  publicSessionsEnabled: boolean,
  zoomFactor: number,
  disableGpuEncode: boolean,
  legacyVolumeKeyProxy: boolean,
  walkthroughCompleted: boolean,
  // Seconds a display survives a device dropping off. It lives here rather than in
  // localStorage because `ScreenExtend serve` reads it out of config.json and has no webview.
  disconnectGraceSecs: number,
  serverPorts: {
    http: number,
    https: number
  },
  hostedNetworkCredentials: {
    name: string,
    password: string
  },
  turnConfig: {
    urls: string,
    username: string,
    credential: string
  },
  /**
   * STUN servers offered to a peer that is not already known to be on this subnet, as a
   * comma-separated list. Empty means no STUN at all, which is a valid setting for a LAN-only
   * host. Read on the Rust side at setup; also writable with `ScreenExtend stun set|clear`.
   */
  stunUrls: string,
  dontShowAgain: {
    editDevice: boolean,
    editNetwork: boolean,
    compatibility: boolean,
    configEditor?: boolean
  }
};

export const DEFAULT_STUN_URL = "stun:stun.l.google.com:19302";
export const DEFAULT_HTTP_PORT = 8080;
export const DEFAULT_HTTPS_PORT = 8443;

export const defaultConfig: Config = {
  name: "",
  theme: "system",
  devices: [],
  knownDevices: [],
  sessionPassword: "",
  publicSessionsEnabled: true,
  zoomFactor: 1,
  disableGpuEncode: false,
  legacyVolumeKeyProxy: false,
  walkthroughCompleted: false,
  disconnectGraceSecs: 10,
  serverPorts: {
    http: DEFAULT_HTTP_PORT,
    https: DEFAULT_HTTPS_PORT
  },
  hostedNetworkCredentials: {
    name: "",
    password: ""
  },
  turnConfig: {
    urls: "",
    username: "",
    credential: ""
  },
  stunUrls: DEFAULT_STUN_URL,
  dontShowAgain: {
    editDevice: false,
    editNetwork: false,
    compatibility: false,
    configEditor: false
  }
};
