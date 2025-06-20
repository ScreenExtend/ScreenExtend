import React, { createContext } from "react";
import { Store } from "@tauri-apps/plugin-store";

export type AuthContextType = {
  currentUser: string,
  setCurrentUser: React.Dispatch<React.SetStateAction<string>>
}

export type Device = {
  ip: string;
  name: string;
  scale: number;
  orientation: "Portrait" | "Landscape";
  refreshRate: number;
  os: string;
  screenSize: string;
};

export type User = {
  username: string,
  password: string,
  theme: string,
  devices: Device[],
  sessionPassword: string,
  hostedNetworkCredentials: {
    name: string,
    password: string
  },
  dontShowAgain: {
    editDevice: boolean,
    editNetwork: boolean
  }
};

export const defaultUser: User = {
  username: "",
  password: "",
  theme: "system",
  devices: [],
  sessionPassword: "",
  hostedNetworkCredentials: {
    name: "",
    password: ""
  },
  dontShowAgain: {
    editDevice: false,
    editNetwork: false
  }
};

export const AuthProviderContext = createContext<AuthContextType>({ currentUser: "", setCurrentUser: () => {} });

const UserDB = await Store.load("config.json");

export const createUser = async (information: Partial<User>) => {
  return await UserDB.set(information.username!, { ...defaultUser, hostedNetworkCredentials: {name: "ScreenExtend-" + (information.username !== "GUESTGUESTGUESTGUESTGUEST" ? information.username : "Guest"), password: "ScreenExtend" + Array.from({length: 5}, () => Math.floor(Math.random() * 10)).join("") + "!"}, ...information });
};

export const getUser = async (username: string) => {
  return await UserDB.get<User>(username);
};

export const updateUser = async (username: string, information: Partial<Omit<User, "username">>) => {
  if ((await getUser(username))!.username === username)
    return await UserDB.set(username, { ...await getUser(username), ...information });
};

export const deleteUser = async (username: string) => {
  return await UserDB.delete(username);
};
