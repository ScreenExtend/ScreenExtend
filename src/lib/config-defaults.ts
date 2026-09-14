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
// `all` is the product's intended behaviour: a second monitor whose mouse and keyboard stop at
// its own edge is not a second monitor. `display` is the opt-in restriction, so a device saved
// before the setting existed -- which carries no value at all -- keeps the reach it had.
export const DEFAULT_POINTER_SCOPE: PointerScope = "all";
export const DEFAULT_AUTO_FRAME_RATE = false;
/** Quality is fixed at the slider unless asked otherwise; ratio and orientation follow the device. */
export const DEFAULT_ADAPTIVE_QUALITY = false;
export const DEFAULT_NATIVE_DPR = true;
export const DEFAULT_AUTO_ORIENTATION = true;

export type Device = {
  /**
   * Where the device was last seen. A label, never a key: an address moves under a live
   * session (a roam, a lease renewal) and is handed on to the next device that asks for one,
   * so a row matched by address is a row that can land on the wrong machine.
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
   * Where a pointer-locked client's cursor may travel. Both input backends track their own
   * position and clamp it; `all`, the default, clamps to the union of every attached display,
   * which is what dragging a window back off the virtual display needs. `display` is the
   * restriction: the cursor stays on the session's own screen and cannot be walked onto a
   * monitor the client's user cannot see.
   *
   * Optional because the field post-dates the devices already in `config.json`; a device
   * saved before it reads as `all` through `DEFAULT_POINTER_SCOPE`, i.e. unchanged.
   */
  pointerScope?: PointerScope;
  /**
   * Whether the stream may run below `refreshRate` on its own. Off -- the default -- makes the
   * chosen rate a promise: the host delivers at that cadence and repeats an unchanged frame
   * rather than skipping it, and no controller lowers it. On lets the pipeline adapt.
   *
   * Optional for the same reason as `pointerScope`; absent reads as
   * `DEFAULT_AUTO_FRAME_RATE`.
   */
  autoFrameRate?: boolean;
  /**
   * The adaptive form of three more settings, each off by default so the value shown is the
   * value used. `adaptiveQuality`: `videoQuality` is a QP floor under an adaptive bitrate
   * instead of a constant QP. `nativeDpr`: render at the ratio the device reports on each
   * join instead of `dpr`. `autoOrientation`: always follow the way the device is held
   * instead of `orientation`. Optional for the same reason as `autoFrameRate`; absent reads
   * as the `DEFAULT_*` constant above.
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
};

/**
 * One live session, as the dashboard sees it: a saved device's settings plus the id of the tab
 * holding them. A device with two tabs open produces two of these with one `token` between
 * them -- the settings are shared, the displays are not.
 *
 * Deliberately not part of `Device`: `config.json` stores devices, which outlive every session
 * they ever hold, and a join id written into a saved row would be a name for something that no
 * longer exists the moment the app restarts.
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
  },
  // What has already been tried about the display driver since this machine last booted, so a
  // repair that did not work is not tried again behind another elevation prompt. Bookkeeping
  // rather than a preference, but it is persisted here rather than in localStorage because
  // losing it is what turns "one attempt" into "one attempt per launch, forever".
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
  },
  driverRepair: {
    boot: "",
    install: 0,
    reinstall: 0,
    relaunch: 0
  }
};
