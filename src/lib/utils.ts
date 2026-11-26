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

// devtools convenience, dev only: nothing in `src/` reads it
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

export type QrEntry = { title: string; value: string };

/// `adaptersUnavailable` separates a failed IPC call from a machine with no usable adapter:
/// after a failed read "no networks were found" would be wrong, since the host may be serving
/// fine on a network that could not be read.
export type QrValues = { entries: QrEntry[]; adaptersUnavailable: boolean };

export async function readQrValues(
  sessionId: string,
  httpPort: number = DEFAULT_STREAMER_HTTP_PORT
): Promise<QrValues> {
  if (!sessionId) return { entries: [], adaptersUnavailable: false };
  const port = Number.isInteger(httpPort) && httpPort > 0 ? httpPort : DEFAULT_STREAMER_HTTP_PORT;
  let adapters: NetworkInfo[] = [];
  try {
    adapters = await commands.getNetworkAdapters();
  } catch (e) {
    console.error("reading the network adapters failed", e);
    return { entries: [], adaptersUnavailable: true };
  }
  const isIpv4 = (ip: string) => /^\d{1,3}(\.\d{1,3}){3}$/.test(ip);
  // zone ids and link-local addresses are dropped: a zone id means something only on this
  // host, and fe80:: typed into a phone's browser cannot reach it
  const isRoutableIpv6 = (ip: string) =>
    ip.indexOf(":") >= 0 && ip.indexOf("%") < 0 && !/^fe[89ab]/i.test(ip) && ip !== "::1";
  const entries = adapters
    .map((adapter) => {
      // IPv4 first where an adapter has both: shorter to read and every client understands it.
      // IPv6 is for an IPv6-only network, and a URL needs it in brackets.
      const ipv4 = adapter.ip_addresses.find(isIpv4);
      const host = ipv4 ?? adapter.ip_addresses.find(isRoutableIpv6);
      if (!host) return null;
      const authority = host === ipv4 ? host : `[${host}]`;
      return {
        title: adapter.network_name,
        value: `http://${authority}:${port}/?id=${sessionId}`,
      };
    })
    .filter((entry): entry is QrEntry => entry !== null);
  return { entries, adaptersUnavailable: false };
}

export async function buildQrValues(sessionId: string, httpPort: number = DEFAULT_STREAMER_HTTP_PORT): Promise<QrEntry[]> {
  return (await readQrValues(sessionId, httpPort)).entries;
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
