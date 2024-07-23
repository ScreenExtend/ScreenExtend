import React, { createContext } from "react";
// @ts-ignore
import * as localStorageDBModule from "localstoragedb";

declare global {
  interface Window {
    hostedNetworkOn: boolean;
    otp: string;
  }
}

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
  sidebarOpen: boolean,
  devices: Device[],
  sessionPassword: string,
  hostedNetworkCredentials: {
    name: string,
    password: string
  },
  dontShowAgain: {
    editDevice: boolean,
    removeDevice: boolean,
    editNetwork: boolean
  }
};

export const defaultUser: User = {
  username: "",
  password: "",
  theme: "system",
  sidebarOpen: true,
  devices: [],
  sessionPassword: "",
  hostedNetworkCredentials: {
    name: "",
    password: ""
  },
  dontShowAgain: {
    editDevice: false,
    removeDevice: false,
    editNetwork: false
  }
};

export const AuthProviderContext = createContext<AuthContextType>({ currentUser: "", setCurrentUser: () => {} });

// @ts-ignore
export const UserDB = new localStorageDBModule.default("database");

if (!UserDB.tableExists("users")) {
  UserDB.createTable("users", ["username", "password", "theme", "sidebarOpen", "devices", "sessionPassword", "hostedNetworkCredentials", "dontShowAgain"]);
  UserDB.commit();
}

// CRUD Create Operation
export const createUser = (information: Partial<User>): void => {
  const results = UserDB.insert("users", { ...defaultUser, hostedNetworkCredentials: {name: "ScreenExtend" + (information.username ? "-" + information.username : ""), password: "ScreenExtend" + Array.from({length: 5}, () => Math.floor(Math.random() * 10)).join("") + "!"}, ...information });
  UserDB.commit();
  return results;
};

// CRUD Read Operation
export const getUser = (username: string) => {
  const results = UserDB.queryAll("users", {query: {username}});
  return results.length > 0 ? (results[0] as User) : null;
};

// CRUD Update Operation
export const updateUser = (username: string, information: Partial<Omit<User, "username">>) => {
  const results = UserDB.update("users", { username }, (user: User) => {return { ...user, ...information }});
  UserDB.commit();
  return results;
};

// CRUD Delete Operation
export const deleteUser = (username: string) => {
  const results = UserDB.deleteRows("users", { username });
  UserDB.commit();
  return results;
};