// The shape of `config.json` and its defaults, with no imports of any kind, so
// `config-schema.ts` and `scripts/test-config-editor.mjs` (plain `node`, type stripping only)
// can load it without React or the Tauri IPC. The schema must not drift from this file.
// `config-provider.tsx` re-exports everything below, so no call site imports from here.

export type PointerScope = "display" | "all";
// `display` is the opt-in restriction, so a device saved without the field keeps full reach
export const DEFAULT_POINTER_SCOPE: PointerScope = "all";
export const DEFAULT_AUTO_FRAME_RATE = false;
/** Quality is fixed at the slider unless asked otherwise; ratio and orientation follow the device. */
export const DEFAULT_ADAPTIVE_QUALITY = false;
export const DEFAULT_NATIVE_DPR = true;
export const DEFAULT_AUTO_ORIENTATION = true;

export type Device = {
  /**
   * Where the device was last seen. A label, never a key: an address moves under a live
   * session and is recycled between devices.
   */
  ip: string;
  /** The device. Every setting below is stored under it, on both sides. */
  token: string;
  name: string;
  scale: number;
  orientation: "Portrait" | "Landscape";
  refreshRate: number;
  videoScale: number;
  videoQuality: number;
  remoteControl: boolean;
  /**
   * Where a pointer-locked client's cursor may travel. `all`, the default, clamps to the union
   * of every attached display (dragging a window back off the virtual display needs that);
   * `display` keeps it on the session's own screen. Absent reads as `DEFAULT_POINTER_SCOPE`.
   */
  pointerScope?: PointerScope;
  /**
   * Whether the stream may run below `refreshRate` on its own. Off (the default): the host
   * delivers at that cadence, repeating an unchanged frame rather than skipping it.
   * Absent reads as `DEFAULT_AUTO_FRAME_RATE`.
   */
  autoFrameRate?: boolean;
  /**
   * `adaptiveQuality`: `videoQuality` is a QP floor under an adaptive bitrate instead of a
   * constant QP. `nativeDpr`: render at the ratio the device reports on each join instead of
   * `dpr`. `autoOrientation`: follow the way the device is held instead of `orientation`.
   * Absent reads as the `DEFAULT_*` constant above.
   */
  adaptiveQuality?: boolean;
  nativeDpr?: boolean;
  autoOrientation?: boolean;
  systemAudio: boolean;
  audioOutputDeviceId: string;
  audioOutputDeviceLabel: string;
  os: string;
  screenSize: string;
  dpr: number;
  maxDpr: number;
  /**
   * Where this device's display sits relative to the main display, as last placed on the Devices
   * page. Restored when the device joins again. Absent: wherever the system puts it.
   */
  placement?: { x: number; y: number };
};

/**
 * One live session as the dashboard sees it: a saved device's settings plus the id of the tab
 * holding them. Two tabs of one device are two of these sharing one `token`.
 *
 * Not part of `Device`: `config.json` stores devices, and a join id does not survive a restart.
 */
export type ConnectedDevice = Device & { joinId: string };

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
  disableLinkGovernor: boolean,
  legacyVolumeKeyProxy: boolean,
  walkthroughCompleted: boolean,
  // seconds a display survives a device dropping off. Kept in config.json, not localStorage:
  // `ScreenExtend serve` reads it and has no webview
  disconnectGraceSecs: number,
  serverPorts: {
    http: number,
    https: number
  },
  hostedNetworkCredentials: {
    name: string,
    password: string
  },
  // a session id and OTP that never rotate, accepted beside the live pair while enabled
  permanentLink: {
    enabled: boolean,
    sessionId: string,
    otp: string
  },
  turnConfig: {
    urls: string,
    username: string,
    credential: string
  },
  /**
   * Comma-separated STUN servers offered to a peer not known to be on this subnet. Empty means
   * no STUN, valid for a LAN-only host. Read by Rust at setup; `ScreenExtend stun set|clear`
   * writes it too.
   */
  stunUrls: string,
  dontShowAgain: {
    editDevice: boolean,
    editNetwork: boolean,
    compatibility: boolean,
    configEditor?: boolean
  },
  // display-driver repairs already tried since this boot, so a failed one is not retried
  // behind another elevation prompt. Must persist, or it becomes one attempt per launch.
  driverRepair: {
    boot: string,
    install: number,
    reinstall: number,
    relaunch: number
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
  disableLinkGovernor: false,
  legacyVolumeKeyProxy: false,
  walkthroughCompleted: false,
  disconnectGraceSecs: 3,
  serverPorts: {
    http: DEFAULT_HTTP_PORT,
    https: DEFAULT_HTTPS_PORT
  },
  hostedNetworkCredentials: {
    name: "",
    password: ""
  },
  permanentLink: {
    enabled: false,
    sessionId: "",
    otp: ""
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
  },
  driverRepair: {
    boot: "",
    install: 0,
    reinstall: 0,
    relaunch: 0
  }
};
