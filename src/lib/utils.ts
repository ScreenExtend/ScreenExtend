import React, { useEffect, useRef } from "react";
import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";
import { commands, type NetworkInfo } from "./bindings";

const STREAMER_HTTP_PORT = 8080;

export const CLOUD_SESSION_DOMAIN = "session.screenextend.app";

export function buildCloudQrValue(sessionId: string): string {
  if (!sessionId) return "";
  return `https://${CLOUD_SESSION_DOMAIN}/?id=${sessionId}`;
}

window.commands = commands;

declare global {
    interface Window {
        sidebarSize: number;
        otp: string;
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

export async function buildQrValues(sessionId: string): Promise<{ title: string; value: string }[]> {
  if (!sessionId) return [];
  let adapters: NetworkInfo[] = [];
  try {
    adapters = await commands.getNetworkAdapters();
  } catch {
    return [];
  }
  const isIpv4 = (ip: string) => /^\d{1,3}(\.\d{1,3}){3}$/.test(ip);
  return adapters
    .map((adapter) => {
      const ipv4 = adapter.ip_addresses.find(isIpv4);
      if (!ipv4) return null;
      return {
        title: adapter.network_name,
        value: `http://${ipv4}:${STREAMER_HTTP_PORT}/?id=${sessionId}`,
      };
    })
    .filter((entry): entry is { title: string; value: string } => entry !== null);
}

export function generateSlug() {
    let result = "";
    const characters = "abcdefghijklmnopqrstuvwxyz";
    const charactersLength = characters.length;
    let counter = 0;
    while (counter < 8) {
        result += characters.charAt(Math.floor(Math.random() * charactersLength));
        counter += 1;
    }
    return result;
}

export function useInterval(callback: () => void, delay: number | null): void {
    const savedCallback = useRef<() => void>(() => {});
    useEffect(() => {
        savedCallback.current = callback;
    }, [callback]);
    useEffect(() => {
        function func() {
            if (savedCallback.current) {
                savedCallback.current();
            }
        }
        if (delay !== null) {
            const id = setInterval(func, delay);
            return () => clearInterval(id);
        }
    }, [delay]);
}
