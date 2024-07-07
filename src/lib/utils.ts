import React from "react";
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