import { useEffect, useSyncExternalStore } from "react";
import { Store } from "@tauri-apps/plugin-store";
import { generatePassword } from "@/lib/utils";
import {
  defaultConfig,
  type Config,
  type Device,
  type KnownDevice,
} from "@/lib/config-defaults";
import { normalizeConfig } from "@/lib/config-normalize";
import { firstRunGaps } from "@/lib/startup-gate";

// the shape and its defaults live in a module with no imports so the Settings editor's
// schema and the node test that cross-checks them can both load it; re-exported because
// call sites import the config from this file
export {
  DEFAULT_ADAPTIVE_QUALITY,
  DEFAULT_AUTO_FRAME_RATE,
  DEFAULT_AUTO_ORIENTATION,
  DEFAULT_NATIVE_DPR,
  DEFAULT_POINTER_SCOPE,
  DEFAULT_STUN_URL,
  DEFAULT_HTTP_PORT,
  DEFAULT_HTTPS_PORT,
  defaultConfig,
} from "@/lib/config-defaults";
export type { PointerScope, Device, ConnectedDevice, KnownDevice, Config } from "@/lib/config-defaults";

const sameDevice = (d: KnownDevice, token: string, ip: string) =>
  token ? d.token === token : (!d.token && d.ip === ip);

const ConfigDB = Store.load("config.json");
ConfigDB.catch((e: unknown) => { console.error("failed to open config store", e); });

let snapshot: Config | undefined;
let snapshotJson = "";
const listeners = new Set<() => void>();

// gated on the serialized value: an identical config is still a new object to
// useSyncExternalStore, which would re-render every subscriber
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

// Writes the corrected values back so the next reader (the Settings editor, `ScreenExtend
// config get`, the host's own replay) sees a file in the declared shape. Only the malformed
// keys are written; the rest, unknown keys included, is left as found. Never call this after a
// failed read: defaults written over a file that could not be parsed are a lost configuration.
const rewriteRepaired = async (db: Store, config: Config, keys: string[]): Promise<void> => {
  try {
    await Promise.all(keys.map(key => db.set(key, (config as Record<string, unknown>)[key])));
    await db.save();
  } catch (e) {
    // not fatal: the in-memory config is correct, and the file stays wrong until the next
    // successful write
    console.error("[migrate] the corrected config could not be written back", e);
  }
};

const readStore = async (): Promise<Config | undefined> => {
  const db = await ConfigDB;
  // one `plugin:store|entries` invoke instead of an IPC round-trip per key
  const entries = await db.entries<unknown>();
  if (entries.length === 0) {
    publish(undefined);
    return undefined;
  }
  const raw: Record<string, unknown> = {};
  for (const [key, value] of entries) raw[key] = value;
  // The one place a stored value of the wrong container type becomes the declared one. Every
  // consumer downstream reads `config.devices.map(...)` and `config.serverPorts.http`
  // directly, so an object under `devices` would otherwise take startup down.
  const { config, notes, repaired } = normalizeConfig(raw);
  for (const note of notes) console.log(note);
  if (repaired.length > 0) {
    // `console.error` is the only console channel wired to the Rust log bus
    // (`lib/error-reporting.ts`), so this shows in `ScreenExtend logs`
    console.error(
      `[migrate] config.json had ${notes.length} malformed value(s) under ${repaired.join(", ")}; ` +
      `the declared defaults were used and the file corrected`
    );
    await rewriteRepaired(db, config, repaired);
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

const firstRunHostedNetwork = (name: string) => ({
  name: "ScreenExtend" + ((name.length > 0) ? ("-" + name) : ""),
  password: generatePassword(12),
});

export const createConfig = async (information: Partial<Config> & { name: string }) => {
  await updateConfig({ ...defaultConfig, hostedNetworkCredentials: firstRunHostedNetwork(information.name), ...information });
};

// Creates what the first run would have, for a store that was written to before it ran
// (`firstRunGaps`). Keys the store already holds are left alone. True when it wrote something.
export const completeFirstRun = async (username: () => Promise<string>): Promise<boolean> => {
  const existing = await getConfig();
  if (!existing) return false;
  const db = await ConfigDB;
  const gaps = firstRunGaps(await db.keys(), existing.hostedNetworkCredentials);
  if (!gaps.name && !gaps.hostedNetwork) return false;
  // the network is named after the account, whichever of the two is being created
  const name = gaps.name ? await username() : existing.name;
  await updateConfig({
    ...(gaps.name ? { name } : {}),
    ...(gaps.hostedNetwork ? { hostedNetworkCredentials: firstRunHostedNetwork(name) } : {}),
  });
  return true;
};

export const getSavedDevices = async (): Promise<Device[]> => {
  return (await getConfig())?.devices ?? [];
};

// A saved row belongs to a device, and a device is its token: addresses are recycled, so a row
// matched by address hands one device's settings to another. A row written before tokens
// existed still matches by address, the only thing it names, until that device joins again.
const sameSavedDevice = (d: Device, token: string, ip: string) =>
  token && d.token ? d.token === token : (!d.token && !!ip && d.ip === ip);

export const saveDeviceSettings = async (device: Device) => {
  const existing = await getSavedDevices();
  const devices = [
    ...existing.filter(d => !sameSavedDevice(d, device.token, device.ip)),
    { ...device, name: "" },
  ];
  await updateConfig({ devices });
};

export const removeSavedDevice = async (token: string, ip: string) => {
  const existing = await getSavedDevices();
  await updateConfig({ devices: existing.filter(d => !sameSavedDevice(d, token, ip)) });
};

export const findSavedDevice = (
  saved: Device[] | undefined,
  token: string,
  ip: string,
): Device | undefined => saved?.find(d => sameSavedDevice(d, token, ip));

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
