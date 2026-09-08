import { useEffect, useSyncExternalStore } from "react";
import { Store } from "@tauri-apps/plugin-store";
import { generatePassword } from "@/lib/utils";

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

const sameDevice = (d: KnownDevice, token: string, ip: string) =>
  token ? d.token === token : (!d.token && d.ip === ip);

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

const ConfigDB = Store.load("config.json");
ConfigDB.catch((e: unknown) => { console.error("failed to open config store", e); });

let snapshot: Config | undefined;
let snapshotJson = "";
const listeners = new Set<() => void>();

// A read is not a change. Publishing an identical config still hands
// useSyncExternalStore a new object, which re-renders every subscriber, so gate
// the fan-out on the serialized value.
const publish = (config: Config | undefined) => {
  const next = config === undefined ? "" : JSON.stringify(config);
  if (next === snapshotJson && (config === undefined) === (snapshot === undefined)) return;
  snapshotJson = next;
  snapshot = config;
  listeners.forEach(listener => listener());
};

export const subscribeToConfig = (listener: () => void): (() => void) => {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
};

export const getConfigSnapshot = (): Config | undefined => snapshot;

let reading: Promise<Config | undefined> | null = null;

const readStore = async (): Promise<Config | undefined> => {
  const db = await ConfigDB;
  // One `plugin:store|entries` invoke. Reading key by key was one IPC round-trip
  // per key of `defaultConfig`, awaited in a loop.
  const entries = await db.entries<unknown>();
  if (entries.length === 0) {
    publish(undefined);
    return undefined;
  }
  const config = { ...defaultConfig };
  for (const [key, value] of entries) {
    if (value !== undefined && key in defaultConfig) {
      (config as Record<string, unknown>)[key] = value;
    }
  }
  publish(config);
  return config;
};

// Forces a store read even when a snapshot is already held. Only needed after a
// write fails, where the in-memory copy may no longer match the file.
export const reloadConfig = async (): Promise<Config | undefined> => {
  if (!reading) reading = readStore().finally(() => { reading = null; });
  return reading;
};

export const getConfig = async (): Promise<Config | undefined> =>
  snapshot !== undefined ? snapshot : reloadConfig();

export const updateConfig = async (information: Partial<Config>) => {
  try {
    const db = await ConfigDB;
    const keys = Object.keys(information) as (keyof Config)[];
    await Promise.all(keys.map(key => db.set(key, information[key])));
  } catch (e) {
    await reloadConfig().catch(() => undefined);
    throw e;
  }
  if (snapshot) publish({ ...snapshot, ...information });
  else await reloadConfig();
};

let loading: Promise<Config | undefined> | null = null;
let loadFailed = false;

export function useConfig(): Config | undefined {
  const config = useSyncExternalStore(subscribeToConfig, getConfigSnapshot);
  useEffect(() => {
    if (config !== undefined || loading !== null || loadFailed) return;
    loading = getConfig()
      .catch((e: unknown) => {
        if (getConfigSnapshot() === undefined) loadFailed = true;
        console.error("failed to load config", e);
        return undefined;
      })
      .finally(() => { loading = null; });
  }, [config]);
  return config;
}

export const flushConfig = async () => {
  const db = await ConfigDB;
  await db.save();
};

export const createConfig = async (information: Partial<Config> & { name: string }) => {
  await updateConfig({ ...defaultConfig, hostedNetworkCredentials: { name: "ScreenExtend" + ((information.name.length > 0) ? ("-" + information.name) : ""), password: generatePassword(12) }, ...information });
};

export const getSavedDevices = async (): Promise<Device[]> => {
  return (await getConfig())?.devices ?? [];
};

export const saveDeviceSettings = async (device: Device) => {
  const existing = await getSavedDevices();
  const devices = [...existing.filter(d => d.ip !== device.ip), { ...device, name: "" }];
  await updateConfig({ devices });
};

export const removeSavedDevice = async (ip: string) => {
  const existing = await getSavedDevices();
  await updateConfig({ devices: existing.filter(d => d.ip !== ip) });
};

export const getKnownDevices = async (): Promise<KnownDevice[]> => {
  const raw = (await getConfig())?.knownDevices ?? [];
  return raw.map(d => ({ ...d, token: d.token ?? "" }));
};

export const recordKnownDevice = async (
  info: { token: string; ip: string; name: string; os: string; screenSize: string }
) => {
  const existing = await getKnownDevices();
  const prev = existing.find(d => sameDevice(d, info.token, info.ip));
  const merged: KnownDevice = {
    token: info.token || prev?.token || "",
    ip: info.ip || prev?.ip || "",
    name: info.name.trim() || prev?.name || "",
    os: info.os.trim() || prev?.os || "",
    screenSize: info.screenSize.trim() || prev?.screenSize || "",
    lastSeen: Date.now(),
    banned: prev?.banned ?? false,
  };
  await updateConfig({
    knownDevices: [...existing.filter(d => !sameDevice(d, info.token, info.ip)), merged],
  });
};

export const setKnownDeviceBanned = async (token: string, ip: string, banned: boolean) => {
  const existing = await getKnownDevices();
  let knownDevices: KnownDevice[];
  if (existing.some(d => sameDevice(d, token, ip))) {
    knownDevices = existing.map(d => (sameDevice(d, token, ip) ? { ...d, banned } : d));
  } else if (banned) {
    knownDevices = [
      ...existing,
      { token, ip, name: "", os: "", screenSize: "", lastSeen: Date.now(), banned: true },
    ];
  } else {
    return;
  }
  await updateConfig({ knownDevices });
  await flushConfig();
};

export const removeKnownDevice = async (token: string, ip: string) => {
  const existing = await getKnownDevices();
  await updateConfig({ knownDevices: existing.filter(d => !sameDevice(d, token, ip)) });
  await flushConfig();
};
