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

// The shape and its defaults live in a module with no imports so the Settings editor's
// schema and the node test that cross-checks them can both see it. Re-exported here
// because every call site in the app imports the config from this file.
export {
  DEFAULT_AUTO_FRAME_RATE,
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

// Put the corrected values back, so the next reader — the Settings editor, `ScreenExtend config
// get`, the host's own replay — sees a file in the declared shape rather than repeating this.
// Only the keys that were actually malformed are written: the rest of the document, including
// keys this version does not know about, is left exactly as it was found.
//
// Deliberately after a successful read and never on a failure to read: a store that could not be
// opened has nothing to correct, and writing defaults over a file we could not parse would turn
// a recoverable problem into a lost configuration.
const rewriteRepaired = async (db: Store, config: Config, keys: string[]): Promise<void> => {
  try {
    await Promise.all(keys.map(key => db.set(key, (config as Record<string, unknown>)[key])));
    await db.save();
  } catch (e) {
    // The in-memory config is already correct, so this is not fatal — the file simply stays
    // wrong until the next successful write.
    console.error("[migrate] the corrected config could not be written back", e);
  }
};

const readStore = async (): Promise<Config | undefined> => {
  const db = await ConfigDB;
  // One `plugin:store|entries` invoke. Reading key by key was one IPC round-trip
  // per key of `defaultConfig`, awaited in a loop.
  const entries = await db.entries<unknown>();
  if (entries.length === 0) {
    publish(undefined);
    return undefined;
  }
  const raw: Record<string, unknown> = {};
  for (const [key, value] of entries) raw[key] = value;
  // The one place a stored value of the wrong container type is turned into the declared one.
  // Every consumer downstream — the bootstrap replay, the dashboard, the Settings page — reads
  // `config.devices.map(…)` and `config.serverPorts.http` directly, and did so before this: a
  // `devices` key holding an object took the whole of startup down with `object is not
  // iterable`, reported as a driver fault. Coercing once here is what makes those reads safe.
  const { config, notes, repaired } = normalizeConfig(raw);
  for (const note of notes) console.log(note);
  if (repaired.length > 0) {
    // `console.error` is the only console channel wired to the Rust log bus
    // (`lib/error-reporting.ts`), and a config file the app had to correct is exactly the thing
    // that has to be visible in `ScreenExtend logs` rather than only in devtools nobody has open.
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

export const createConfig = async (information: Partial<Config> & { name: string }) => {
  await updateConfig({ ...defaultConfig, hostedNetworkCredentials: { name: "ScreenExtend" + ((information.name.length > 0) ? ("-" + information.name) : ""), password: generatePassword(12) }, ...information });
};

export const getSavedDevices = async (): Promise<Device[]> => {
  return (await getConfig())?.devices ?? [];
};

// A saved row belongs to a device, and a device is its token. Addresses are recycled — the one
// a phone had this morning is the one somebody's laptop gets this afternoon — so a row matched
// by address hands one device's settings to another. A row written before tokens existed still
// matches by address, which is the only thing it names, until that device joins once more.
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
