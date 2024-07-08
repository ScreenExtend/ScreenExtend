import React, { createContext } from "react";

type AuthContextType = {
  currentUser: { username: string, password: string },
  setCurrentUser: React.Dispatch<React.SetStateAction<{ username: string, password: string }>>
}

declare global {
  interface Window {
  }
}

export const AuthProviderContext = createContext<AuthContextType>({ currentUser: { username: "", password: "" }, setCurrentUser: () => {} });