import React, { createContext } from "react";
import { type Device } from "@/components/auth-provider";

export const GlobalContextDefault = {
  hostedNetworkOn: false,
  otp: "",
  sessionId: "",
  qrValues: [] as { title: string, value: string }[],
  loaded: false,
  authValues: { username: "", password: "" },
  devices: [] as Device[]
};

export type GlobalContextType = {
  windowHostedNetworkOn: [boolean, React.Dispatch<React.SetStateAction<boolean>>],
  windowOtp: [string, React.Dispatch<React.SetStateAction<string>>],
  windowSessionId: [string, React.Dispatch<React.SetStateAction<string>>],
  windowQrValues: [{ title: string, value: string }[], React.Dispatch<React.SetStateAction<{ title: string, value: string }[]>>],
  windowLoaded: [boolean, React.Dispatch<React.SetStateAction<boolean>>],
  windowAuthValues: [{ username: string, password: string }, React.Dispatch<React.SetStateAction<{ username: string, password: string }>>],
  windowClosing: [boolean, React.Dispatch<React.SetStateAction<boolean>>],
  windowDevices: [Device[], React.Dispatch<React.SetStateAction<Device[]>>]
}

export const GlobalProviderContext = createContext<GlobalContextType>({
  windowHostedNetworkOn: [false, () => {}],
  windowOtp: ["", () => {}],
  windowSessionId: ["", () => {}],
  windowQrValues: [[] as { title: string, value: string }[], () => {}],
  windowLoaded: [false, () => {}],
  windowAuthValues: [{ username: "", password: "" }, () => {}],
  windowClosing: [false, () => {}],
  windowDevices: [[] as Device[], () => {}]
});
