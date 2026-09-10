import React from "react";
import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";
import { commands, type NetworkInfo } from "./bindings";

export const DEFAULT_STREAMER_HTTP_PORT = 8080;

export const CLOUD_SESSION_DOMAIN = "session.screenextend.app";

export function buildCloudQrValue(sessionId: string): string {
  if (!sessionId) return "";
  return `https://${CLOUD_SESSION_DOMAIN}/?id=${sessionId}`;
}

export function buildWifiQrValue(ssid: string, password: string): string {
  const escape = (value: string) => value.replace(/([\\;,:"])/g, "\\$1");
  if (!password) {
    return `WIFI:T:nopass;S:${escape(ssid)};;`;
  }
  return `WIFI:T:WPA;S:${escape(ssid)};P:${escape(password)};;`;
}

// A devtools convenience, and nothing in `src/` reads it. In a release build there are no
// devtools attached, so it was only keeping the whole command surface reachable from a
// global.
if (import.meta.env.DEV) window.commands = commands;

declare global {
    interface Window {
        commands: any;
    }
}

export function cn(...inputs: ClassValue[]) {
    return twMerge(clsx(inputs));
}

export function useFocus<T extends HTMLElement>() {
    const inputRef = React.useRef<T>(null);
    const setInputFocus = () => {
        if (inputRef.current) {
            inputRef.current.focus();
        }
    };
    return { inputRef, setInputFocus };
}

export async function buildQrValues(sessionId: string, httpPort: number = DEFAULT_STREAMER_HTTP_PORT): Promise<{ title: string; value: string }[]> {
  if (!sessionId) return [];
  const port = Number.isInteger(httpPort) && httpPort > 0 ? httpPort : DEFAULT_STREAMER_HTTP_PORT;
  let adapters: NetworkInfo[] = [];
  try {
    adapters = await commands.getNetworkAdapters();
  } catch (e) {
    // The dashboard renders an empty list as "no networks were found", which is the wrong
    // message for a failed IPC call -- but the correct one is a copy change, so for now
    // the reason at least reaches the log bus.
    console.error("reading the network adapters failed", e);
    return [];
  }
  const isIpv4 = (ip: string) => /^\d{1,3}(\.\d{1,3}){3}$/.test(ip);
  // Anything with a colon is IPv6. `%25eth0`-style zone identifiers and link-local
  // addresses are dropped: a zone id is meaningful only on the host that wrote it, and
  // fe80:: cannot be typed into a phone's browser and reach us.
  const isRoutableIpv6 = (ip: string) =>
    ip.indexOf(":") >= 0 && ip.indexOf("%") < 0 && !/^fe[89ab]/i.test(ip) && ip !== "::1";
  return adapters
    .map((adapter) => {
      // IPv4 stays first where an adapter has both -- it is shorter to read off a screen
      // and every client understands it. The IPv6 branch is what makes an IPv6-only
      // network show a QR code at all: the host already binds and serves it, and the
      // bracketed authority form is what a URL needs.
      const ipv4 = adapter.ip_addresses.find(isIpv4);
      const host = ipv4 ?? adapter.ip_addresses.find(isRoutableIpv6);
      if (!host) return null;
      const authority = host === ipv4 ? host : `[${host}]`;
      return {
        title: adapter.network_name,
        value: `http://${authority}:${port}/?id=${sessionId}`,
      };
    })
    .filter((entry): entry is { title: string; value: string } => entry !== null);
}

export function generatePassword(length = 12): string {
    const charset = "ABCDEFGHJKMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    const values = new Uint32Array(length);
    crypto.getRandomValues(values);
    let result = "";
    for (let i = 0; i < length; i++) {
        result += charset[values[i] % charset.length];
    }
    return result;
}

export function generateOtp(length = 6): string {
    const digits = "0123456789";
    const limit = Math.floor(0x1_0000_0000 / digits.length) * digits.length;
    const buf = new Uint32Array(length);
    let result = "";
    while (result.length < length) {
        crypto.getRandomValues(buf);
        for (let i = 0; i < buf.length && result.length < length; i++) {
            if (buf[i] < limit) {
                result += digits[buf[i] % digits.length];
            }
        }
    }
    return result;
}
