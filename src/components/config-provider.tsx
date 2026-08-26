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
  dontShowAgain: {
    editDevice: boolean,
    editNetwork: boolean,
    compatibility: boolean
  }
};

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
  dontShowAgain: {
    editDevice: false,
    editNetwork: false,
    compatibility: false
  }
};

const ConfigDB = Store.load("config.json");

export const getConfig = async (): Promise<Config | undefined> => {
  const db = await ConfigDB;
  if ((await db.length()) === 0) return undefined;
  const config = { ...defaultConfig };
  for (const key of Object.keys(defaultConfig) as (keyof Config)[]) {
    const value = await db.get(key);
    if (value !== undefined) (config as Record<string, unknown>)[key] = value;
  }
  return config;
};

export const updateConfig = async (information: Partial<Config>) => {
  const db = await ConfigDB;
  for (const key of Object.keys(information) as (keyof Config)[]) {
    await db.set(key, information[key]);
  }
};

export const flushConfig = async () => {
  const db = await ConfigDB;
  await db.save();
};

export const createConfig = async (information: Partial<Config> & { name: string }) => {
  await updateConfig({ ...defaultConfig, hostedNetworkCredentials: { name: "ScreenExtend" + ((information.name.length > 0) ? ("-" + information.name) : ""), password: generatePassword(12) }, ...information });
  console.log({ ...defaultConfig, hostedNetworkCredentials: { name: "ScreenExtend" + ((information.name.length > 0) ? ("-" + information.name) : ""), password: generatePassword(12) }, ...information });
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
