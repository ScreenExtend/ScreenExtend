import { commands } from "@/lib/bindings";
import { flushConfig, updateConfig, type Config } from "@/components/config-provider";

export async function applyConfig(config: Config): Promise<Config> {
  const ports = await commands.setServerPorts(config.serverPorts.http, config.serverPorts.https);
  const applied: Config = { ...config, serverPorts: { http: ports.http, https: ports.https } };

  await commands.setTurnConfig(
    applied.turnConfig.urls,
    applied.turnConfig.username,
    applied.turnConfig.credential
  );
  await commands.setDisableGpuEncode(applied.disableGpuEncode);
  await commands.setLegacyVolumeKeyProxy(applied.legacyVolumeKeyProxy);

  for (const device of applied.devices) {
    await commands.setDeviceOverride(
      device.ip,
      device.scale,
      device.orientation,
      device.refreshRate,
      device.videoScale,
      device.videoQuality,
      device.remoteControl ?? false,
      device.dpr ?? device.maxDpr ?? 1,
      device.systemAudio ?? false
    );
    await commands.setDeviceAudioOutput(device.ip, device.audioOutputDeviceId ?? "");
  }

  for (const known of applied.knownDevices) {
    const token = known.token ?? "";
    await commands.setDeviceBanned(token, known.ip, known.banned);
    if (token) await commands.setDeviceApproved(token, !known.banned);
  }

  await updateConfig(applied);
  await flushConfig();
  return applied;
}
