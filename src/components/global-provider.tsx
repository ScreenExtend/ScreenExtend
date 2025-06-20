import React, { createContext } from "react";

export const GlobalContextDefault = {
  hostedNetworkOn: false,
  otp: "",
  slug: "",
  qrValues: [] as { title: string, value: string }[],
  loaded: false,
  authValues: { username: "", password: "" }
};

export type GlobalContextType = {
  windowHostedNetworkOn: [boolean, React.Dispatch<React.SetStateAction<boolean>>],
  windowOtp: [string, React.Dispatch<React.SetStateAction<string>>],
  windowSlug: [string, React.Dispatch<React.SetStateAction<string>>],
  windowQrValues: [{ title: string, value: string }[], React.Dispatch<React.SetStateAction<{ title: string, value: string }[]>>],
  windowLoaded: [boolean, React.Dispatch<React.SetStateAction<boolean>>],
  windowAuthValues: [{ username: string, password: string }, React.Dispatch<React.SetStateAction<{ username: string, password: string }>>]
}

export const GlobalProviderContext = createContext<GlobalContextType>({
  windowHostedNetworkOn: [false, () => {}],
  windowOtp: ["", () => {}],
  windowSlug: ["", () => {}],
  windowQrValues: [[] as { title: string, value: string }[], () => {}],
  windowLoaded: [false, () => {}],
  windowAuthValues: [{ username: "", password: "" }, () => {}]
});
