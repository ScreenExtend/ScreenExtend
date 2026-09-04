import { commands } from "@/lib/bindings";
import {
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

  for (const device of applied.devices) {
    await at(`devices[${device.ip}]`, async () => {
      await commands.setDeviceOverride(
        device.ip,
        device.scale,
        device.orientation,
        device.refreshRate,
        device.videoScale,
        device.videoQuality,
        device.remoteControl,
        device.dpr,
        device.systemAudio
      );
      await commands.setDeviceAudioOutput(device.ip, device.audioOutputDeviceId);
    });
  }

  for (const known of applied.knownDevices) {
    await at(`knownDevices[${known.ip}]`, async () => {
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
