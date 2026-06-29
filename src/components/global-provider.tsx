import React, { createContext } from "react";
import { type Device } from "@/components/config-provider";

export const GlobalContextDefault = {
  hostedNetworkOn: false,
  otp: "",
  sessionId: "",
  qrValues: [] as { title: string, value: string }[],
  loaded: false,
  devices: [] as Device[]
};

export type GlobalContextType = {
  windowHostedNetworkOn: [boolean, React.Dispatch<React.SetStateAction<boolean>>],
  windowOtp: [string, React.Dispatch<React.SetStateAction<string>>],
  windowSessionId: [string, React.Dispatch<React.SetStateAction<string>>],
  windowQrValues: [{ title: string, value: string }[], React.Dispatch<React.SetStateAction<{ title: string, value: string }[]>>],
  windowLoaded: [boolean, React.Dispatch<React.SetStateAction<boolean>>],
  windowClosing: [boolean, React.Dispatch<React.SetStateAction<boolean>>],
  windowDevices: [Device[], React.Dispatch<React.SetStateAction<Device[]>>]
}

export const GlobalProviderContext = createContext<GlobalContextType>({
  windowHostedNetworkOn: [false, () => {}],
  windowOtp: ["", () => {}],
  windowSessionId: ["", () => {}],
  windowQrValues: [[] as { title: string, value: string }[], () => {}],
  windowLoaded: [false, () => {}],
  windowClosing: [false, () => {}],
  windowDevices: [[] as Device[], () => {}]
});
