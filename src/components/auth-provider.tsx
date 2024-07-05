import React, { createContext, useState } from "react";
const [currentUser, setCurrentUser] = useState({ username: "", password: "" });
type AuthContextType = {
    currentUser: { username: string, password: string },
    setCurrentUser: React.Dispatch<React.SetStateAction<{ username: string, password: string }>>
};
export const AuthProviderContext = createContext<AuthContextType>({ currentUser, setCurrentUser });