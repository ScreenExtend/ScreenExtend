import { DEFAULT_HTTP_PORT, DEFAULT_HTTPS_PORT } from "@/components/config-provider";

type JsonSchema = Record<string, unknown>;

const str = (description: string, extra: JsonSchema = {}): JsonSchema => ({
  type: "string",
  description,
  ...extra,
});

const bool = (description: string, def: boolean): JsonSchema => ({
  type: "boolean",
  description,
  default: def,
});

const int = (description: string, minimum: number, maximum: number, def?: number): JsonSchema => ({
  type: "integer",
  description,
  minimum,
  maximum,
  ...(def === undefined ? {} : { default: def }),
});

const deviceSchema: JsonSchema = {
  type: "object",
  description: "Saved streaming overrides for one device, keyed by its IP address.",
  required: ["ip"],
  additionalProperties: false,
  properties: {
    ip: str("IP address of the device on the local network. This is what the overrides are matched against when the device rejoins."),
    token: str("Trust token the host minted when this device first joined with the code. Devices with a token skip the code prompt on rejoin. Leave empty for devices that predate tokens."),
    name: str("Display name reported by the device. Saved entries normally leave this empty and take the name the device reports when it joins."),
    scale: int("Display scale in percent — the DPI scaling Windows/macOS applies to the virtual display. 100 is 1:1.", 25, 200, 100),
    orientation: {
      type: "string",
      description: "Orientation of the virtual display created for this device.",
      enum: ["Portrait", "Landscape"],
      enumDescriptions: [
        "Taller than wide — phones held upright.",
        "Wider than tall — tablets and laptops.",
      ],
      default: "Landscape",
    },
    refreshRate: int("Refresh rate of the virtual display, in Hz. Higher is smoother but costs more bandwidth and CPU/GPU.", 15, 500, 60),
    videoScale: int("Percentage of the display resolution actually encoded and streamed. Lower values trade sharpness for bandwidth.", 10, 100, 100),
    videoQuality: int("H.264 quantiser (QP). Lower means better quality and a larger stream; 1 is near-lossless, 51 is worst.", 1, 51, 23),
    remoteControl: bool("Let this device control the host with its keyboard, mouse and touch input.", false),
    systemAudio: bool("Stream the host's system audio to this device.", false),
    audioOutputDeviceId: str("Identifier of the audio output on the device that system audio is played through. Empty means the device's default output."),
    audioOutputDeviceLabel: str("Human-readable label for audioOutputDeviceId, shown in the device settings."),
    os: str("Operating system the device reported when it joined."),
    screenSize: str("Screen size the device reported when it joined, e.g. \"1920x1080\"."),
    dpr: {
      type: "number",
      description: "Device pixel ratio to render at. Must be at least 1 and no more than maxDpr; higher values give a sharper picture on high-density screens.",
      minimum: 1,
      maximum: 4,
      default: 1,
    },
    maxDpr: {
      type: "number",
      description: "Highest device pixel ratio this device reported it can display. Reported by the client — changing it by hand has no effect beyond capping dpr.",
      minimum: 1,
      maximum: 4,
      default: 1,
    },
  },
};

const knownDeviceSchema: JsonSchema = {
  type: "object",
  description: "A device the host has seen before, used for the known-devices list and for bans.",
  required: ["ip"],
  additionalProperties: false,
  properties: {
    token: str("Trust token for this device. Bans and auto-approval are keyed on the token, not the IP."),
    ip: str("Last IP address this device joined from. Only a display hint."),
    name: str("Name the device reported when it last joined."),
    os: str("Operating system the device reported when it last joined."),
    screenSize: str("Screen size the device reported when it last joined."),
    lastSeen: {
      type: "integer",
      description: "When the device last joined, as milliseconds since the Unix epoch.",
      minimum: 0,
      default: 0,
    },
    banned: bool("Refuse connections from this device. Banned devices cannot rejoin even with the correct code.", false),
  },
};

export const configJsonSchema: JsonSchema = {
  $schema: "http://json-schema.org/draft-07/schema#",
  title: "ScreenExtend configuration",
  description: "The contents of config.json. Everything the desktop app persists between launches.",
  type: "object",
  required: [
    "name",
    "theme",
    "devices",
    "knownDevices",
    "sessionPassword",
    "publicSessionsEnabled",
    "zoomFactor",
    "disableGpuEncode",
    "legacyVolumeKeyProxy",
    "walkthroughCompleted",
    "serverPorts",
    "hostedNetworkCredentials",
    "turnConfig",
    "dontShowAgain",
  ],
  additionalProperties: false,
  properties: {
    name: str("Your account name. Shown in the profile menu and used to name the hosted network."),
    theme: {
      type: "string",
      description: "Colour theme of the desktop app.",
      enum: ["system", "light", "dark"],
      enumDescriptions: [
        "Follow the operating system setting.",
        "Always light.",
        "Always dark.",
      ],
      default: "system",
    },
    devices: {
      type: "array",
      description: "Per-device streaming overrides. Each entry is reapplied when that device rejoins.",
      items: deviceSchema,
      default: [],
    },
    knownDevices: {
      type: "array",
      description: "Devices that have joined before, with their ban state.",
      items: knownDeviceSchema,
      default: [],
    },
    sessionPassword: str("Reserved for a fixed session password. Sessions currently use the rotating six-digit code shown in the app, so this is normally empty."),
    publicSessionsEnabled: bool("Register the session with the ScreenExtend relay so devices on other networks can join. Turning this off restricts joining to your local network.", true),
    zoomFactor: {
      type: "number",
      description: "Interface zoom of the desktop app. 1 is 100%; the app's zoom buttons step through 0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75 and 2.",
      minimum: 0.5,
      maximum: 2,
      default: 1,
    },
    disableGpuEncode: bool("Force CPU-only (software) H.264 encoding instead of the GPU encoder. Not recommended — it raises CPU usage and can cost quality or frame rate.", false),
    legacyVolumeKeyProxy: bool("macOS 10.15-12.x only. Keeps the hardware volume keys working while the ScreenExtend virtual audio device is the system output.", false),
    walkthroughCompleted: bool("Whether the onboarding walkthrough has been finished. Set this to false to see it again on the next launch.", false),
    serverPorts: {
      type: "object",
      description: "TCP ports the local-network server listens on. Change these if another app already uses 8080/8443; devices must rejoin with the new link afterwards.",
      required: ["http", "https"],
      additionalProperties: false,
      properties: {
        http: int("Plain HTTP port. This is the port in the join URL and QR code.", 1, 65535, DEFAULT_HTTP_PORT),
        https: int("HTTPS port, served with a self-signed certificate. Must differ from the HTTP port.", 1, 65535, DEFAULT_HTTPS_PORT),
      },
    },
    hostedNetworkCredentials: {
      type: "object",
      description: "Credentials for the Wi-Fi network the host can create for devices to join when there is no shared network.",
      required: ["name", "password"],
      additionalProperties: false,
      properties: {
        name: str("Network name (SSID). Must start with \"ScreenExtend\" and be at most 32 characters.", {
          maxLength: 32,
          pattern: "^(ScreenExtend.*)?$",
          patternErrorMessage: "The network name must start with \"ScreenExtend\".",
        }),
        password: str("Network password. At least 8 characters on Windows, 10 on macOS.", { maxLength: 63 }),
      },
    },
    turnConfig: {
      type: "object",
      description: "TURN relay used when two devices are on different networks and cannot connect directly. Leave the URL empty to disable relaying.",
      required: ["urls", "username", "credential"],
      additionalProperties: false,
      properties: {
        urls: str("TURN server URL, e.g. \"turn:turn.example.com:3478\". Several servers can be listed separated by commas. Empty disables the relay.", {
          pattern: "^( *turns?:.*)?$",
          patternErrorMessage: "Must start with \"turn:\" or \"turns:\", or be empty to disable the relay.",
        }),
        username: str("Username for the TURN server."),
        credential: str("Password or credential for the TURN server."),
      },
    },
    dontShowAgain: {
      type: "object",
      description: "Confirmation dialogs the user has chosen to suppress.",
      required: ["editDevice", "editNetwork", "compatibility"],
      additionalProperties: false,
      properties: {
        editDevice: bool("Suppress the \"unsaved changes\" warning when closing the device editor.", false),
        editNetwork: bool("Suppress the warning shown before restarting the hosted network with new credentials.", false),
        compatibility: bool("Suppress the compatibility report shown at startup.", false),
        configEditor: bool("Suppress the \"unsaved changes\" warning when leaving Settings with unsaved edits in this editor.", false),
      },
    },
  },
};

export interface ConfigError {
  path: string;
  message: string;
}

const label = (path: string) => (path === "" ? "(root)" : path);

const typeOf = (value: unknown): string => {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  return typeof value;
};

const matchesType = (value: unknown, type: string): boolean => {
  switch (type) {
    case "integer":
      return typeof value === "number" && Number.isInteger(value);
    case "number":
      return typeof value === "number" && Number.isFinite(value);
    case "array":
      return Array.isArray(value);
    case "object":
      return typeof value === "object" && value !== null && !Array.isArray(value);
    case "null":
      return value === null;
    default:
      return typeOf(value) === type;
  }
};

function walk(schema: JsonSchema, value: unknown, path: string, errors: ConfigError[]) {
  const type = schema.type as string | undefined;
  if (type && !matchesType(value, type)) {
    errors.push({
      path,
      message: `${label(path)}: expected ${type}, got ${typeOf(value)}`,
    });
    return;
  }

  const allowed = schema.enum as unknown[] | undefined;
  if (allowed && !allowed.includes(value)) {
    errors.push({
      path,
      message: `${label(path)}: ${JSON.stringify(value)} is not one of ${allowed.map(v => JSON.stringify(v)).join(", ")}`,
    });
    return;
  }

  if (typeof value === "number") {
    const { minimum, maximum } = schema as { minimum?: number; maximum?: number };
    if (minimum !== undefined && value < minimum) {
      errors.push({ path, message: `${label(path)}: ${value} is below the minimum of ${minimum}` });
    }
    if (maximum !== undefined && value > maximum) {
      errors.push({ path, message: `${label(path)}: ${value} is above the maximum of ${maximum}` });
    }
  }

  if (typeof value === "string") {
    const { minLength, maxLength, pattern } = schema as {
      minLength?: number;
      maxLength?: number;
      pattern?: string;
    };
    if (minLength !== undefined && value.length < minLength) {
      errors.push({ path, message: `${label(path)}: must be at least ${minLength} characters` });
    }
    if (maxLength !== undefined && value.length > maxLength) {
      errors.push({ path, message: `${label(path)}: must be at most ${maxLength} characters` });
    }
    if (pattern !== undefined && !new RegExp(pattern).test(value)) {
      const hint = (schema as { patternErrorMessage?: string }).patternErrorMessage;
      errors.push({
        path,
        message: hint
          ? `${label(path)}: ${hint}`
          : `${label(path)}: ${JSON.stringify(value)} does not match ${pattern}`,
      });
    }
  }

  if (Array.isArray(value) && schema.items) {
    value.forEach((item, i) => walk(schema.items as JsonSchema, item, `${path}[${i}]`, errors));
    return;
  }

  if (matchesType(value, "object") && schema.properties) {
    const properties = schema.properties as Record<string, JsonSchema>;
    const record = value as Record<string, unknown>;
    for (const key of (schema.required as string[] | undefined) ?? []) {
      if (!(key in record)) {
        errors.push({ path, message: `${label(path)}: missing required key "${key}"` });
      }
    }
    for (const [key, child] of Object.entries(record)) {
      const childPath = path ? `${path}.${key}` : key;
      const childSchema = properties[key];
      if (!childSchema) {
        if (schema.additionalProperties === false) {
          errors.push({ path: childPath, message: `${label(path)}: "${key}" is not a known key` });
        }
        continue;
      }
      walk(childSchema, child, childPath, errors);
    }
  }
}

export function validateConfig(
  value: unknown,
  opts: { minHostedNetworkPasswordLength?: number } = {}
): ConfigError[] {
  const errors: ConfigError[] = [];
  walk(configJsonSchema, value, "", errors);
  if (errors.length > 0) return errors;

  const config = value as {
    serverPorts: { http: number; https: number };
    hostedNetworkCredentials: { password: string };
    devices: { ip: string; dpr: number; maxDpr: number }[];
  };

  if (config.serverPorts.http === config.serverPorts.https) {
    errors.push({
      path: "serverPorts.https",
      message: "serverPorts: the HTTP and HTTPS ports must differ",
    });
  }

  const minPassword = opts.minHostedNetworkPasswordLength ?? 8;
  const password = config.hostedNetworkCredentials.password;
  if (password.length > 0 && password.length < minPassword) {
    errors.push({
      path: "hostedNetworkCredentials.password",
      message: `hostedNetworkCredentials.password: must be empty or at least ${minPassword} characters on this platform`,
    });
  }

  const seen = new Set<string>();
  config.devices.forEach((device, i) => {
    if (seen.has(device.ip)) {
      errors.push({
        path: `devices[${i}].ip`,
        message: `devices[${i}]: duplicate entry for ${device.ip}`,
      });
    }
    seen.add(device.ip);
    if (device.dpr !== undefined && device.maxDpr !== undefined && device.dpr > device.maxDpr) {
      errors.push({
        path: `devices[${i}].dpr`,
        message: `devices[${i}]: dpr ${device.dpr} is above the device's maxDpr of ${device.maxDpr}`,
      });
    }
  });

  return errors;
}
