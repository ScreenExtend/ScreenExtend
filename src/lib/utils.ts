import React from "react";

import React, { useState, useEffect, useRef } from "react";
import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

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

export function useInterval(callback, delay) {
    const savedCallback = useRef();
    useEffect(() => {
        savedCallback.current = callback;
    }, [callback]);
    useEffect(() => {
        function func() {
            savedCallback.current();
        }
        if (delay !== null) {
            let id = setInterval(func, delay);
            return () => clearInterval(id);
        }
    }, [delay]);
}