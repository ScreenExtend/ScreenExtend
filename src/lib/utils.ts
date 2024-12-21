import React, { useEffect, useRef } from "react";
import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

declare global {
    interface Window {
        hostedNetworkOn?: boolean;
        otp?: string;
        slug?: string;
        qrValues?: { title: string, value: string }[];
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
    const savedCallback = useRef<() => void>();
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