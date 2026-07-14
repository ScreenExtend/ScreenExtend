import { Store } from "@tauri-apps/plugin-store";
import { generatePassword } from "@/lib/utils";

export type Device = {
  ip: string;
  name: string;
  scale: number;
  orientation: "Portrait" | "Landscape";
  refreshRate: number;
  videoScale: number;
  videoQuality: number;
  os: string;
  screenSize: string;
};

export type Config = {
  name: string,
  theme: string,
  devices: Device[],
  sessionPassword: string,
  publicSessionsEnabled: boolean,
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
  sessionPassword: "",
  publicSessionsEnabled: true,
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
  const devices = [...existing.filter(d => d.ip !== device.ip), device];
  await updateConfig({ devices });
};

export const removeSavedDevice = async (ip: string) => {
  const existing = await getSavedDevices();
  await updateConfig({ devices: existing.filter(d => d.ip !== ip) });
};
