import { commands } from "@/lib/bindings";
import {
  DEFAULT_ADAPTIVE_QUALITY,
  DEFAULT_AUTO_FRAME_RATE,
  DEFAULT_AUTO_ORIENTATION,
  DEFAULT_NATIVE_DPR,
  DEFAULT_POINTER_SCOPE,
  flushConfig,
  updateConfig,
  type Config,
  type Device,
  type KnownDevice,
} from "@/components/config-provider";

async function at<T>(path: string, run: () => Promise<T>): Promise<T> {
  try {
    return await run();
  } catch (e) {
    throw new Error(`${path}: ${e instanceof Error ? e.message : String(e)}`);
  }
}

const resolveDevice = (device: Device): Device => ({
  ...device,
  scale: device.scale ?? 100,
  orientation: device.orientation ?? "Landscape",
  refreshRate: device.refreshRate ?? 60,
  videoScale: device.videoScale ?? 100,
  videoQuality: device.videoQuality ?? 23,
  remoteControl: device.remoteControl ?? false,
  pointerScope: device.pointerScope ?? DEFAULT_POINTER_SCOPE,
  autoFrameRate: device.autoFrameRate ?? DEFAULT_AUTO_FRAME_RATE,
  adaptiveQuality: device.adaptiveQuality ?? DEFAULT_ADAPTIVE_QUALITY,
  nativeDpr: device.nativeDpr ?? DEFAULT_NATIVE_DPR,
  autoOrientation: device.autoOrientation ?? DEFAULT_AUTO_ORIENTATION,
  systemAudio: device.systemAudio ?? false,
  audioOutputDeviceId: device.audioOutputDeviceId ?? "",
  dpr: device.dpr ?? device.maxDpr ?? 1,
});

const resolveKnownDevice = (known: KnownDevice): KnownDevice => ({
  ...known,
  token: known.token ?? "",
  banned: known.banned ?? false,
});

export async function applyConfig(config: Config): Promise<Config> {
  const ports = await at("serverPorts", () =>
    commands.setServerPorts(config.serverPorts.http, config.serverPorts.https)
  );
  const applied: Config = {
    ...config,
    serverPorts: { http: ports.http, https: ports.https },
    devices: config.devices.map(resolveDevice),
    knownDevices: config.knownDevices.map(resolveKnownDevice),
  };

  await at("turnConfig", () =>
    commands.setTurnConfig(
      applied.turnConfig.urls,
      applied.turnConfig.username,
      applied.turnConfig.credential
    )
  );
  await at("disableGpuEncode", () => commands.setDisableGpuEncode(applied.disableGpuEncode));
  await at("legacyVolumeKeyProxy", () =>
    commands.setLegacyVolumeKeyProxy(applied.legacyVolumeKeyProxy)
  );
  await at("disconnectGraceSecs", () =>
    commands.setDisconnectGrace(applied.disconnectGraceSecs)
  );

  for (const device of applied.devices) {
    // Settings belong to the device, and a device is its token. A row saved before tokens
    // existed names only an address, and an address is handed on to the next machine that
    // asks for one — so it waits for that device to join once more rather than being
    // replayed onto whoever is there now.
    if (!device.token) continue;
    await at(`devices[${device.token}]`, async () => {
      await commands.setDeviceOverride(
        device.token,
        device.scale,
        device.orientation,
        device.refreshRate,
        device.videoScale,
        device.videoQuality,
        device.remoteControl,
        device.dpr,
        device.systemAudio
      );
      // Their own commands rather than `setDeviceOverride` arguments: that one is at
      // specta's ten-parameter ceiling.
      await commands.setDevicePointerScope(device.token, device.pointerScope ?? DEFAULT_POINTER_SCOPE);
      await commands.setDeviceAutoFrameRate(
        device.token,
        device.autoFrameRate ?? DEFAULT_AUTO_FRAME_RATE
      );
      await commands.setDeviceAdaptiveModes(
        device.token,
        device.adaptiveQuality ?? DEFAULT_ADAPTIVE_QUALITY,
        device.nativeDpr ?? DEFAULT_NATIVE_DPR,
        device.autoOrientation ?? DEFAULT_AUTO_ORIENTATION
      );
      await commands.setDeviceAudioOutput(device.token, device.audioOutputDeviceId);
    });
  }

  for (const known of applied.knownDevices) {
    await at(`knownDevices[${known.token || known.ip}]`, async () => {
      await commands.setDeviceBanned(known.token, known.ip, known.banned);
      if (known.token) await commands.setDeviceApproved(known.token, !known.banned);
    });
  }

  await at("config.json", async () => {
    await updateConfig(applied);
    await flushConfig();
  });
  return applied;
}
