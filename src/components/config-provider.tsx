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
    editNetwork: boolean
  }
};

export const defaultConfig: Config = {
  name: "",
  theme: "system",
  devices: [],
  sessionPassword: "",
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
    editNetwork: false
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

export const createConfig = async (information: Partial<Config> & { name: string }) => {
  await updateConfig({ ...defaultConfig, hostedNetworkCredentials: { name: "ScreenExtend" + ((information.name.length > 0) ? ("-" + information.name) : ""), password: generatePassword(12) }, ...information });
  console.log({ ...defaultConfig, hostedNetworkCredentials: { name: "ScreenExtend" + ((information.name.length > 0) ? ("-" + information.name) : ""), password: generatePassword(12) }, ...information });
};
