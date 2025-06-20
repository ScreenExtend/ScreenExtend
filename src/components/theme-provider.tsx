import React, { createContext, useContext, useEffect, useState } from "react";

import { AuthProviderContext, getUser, updateUser } from "@/components/auth-provider";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
const appWindow = getCurrentWebviewWindow();

export type Theme = "dark" | "light" | "system";

type ThemeProviderProps = {
  children: React.ReactNode;
  defaultTheme?: Theme;
};

type ThemeProviderState = {
  theme: Theme;
  setTheme: (theme: Theme) => Promise<void>;
};

const initialState: ThemeProviderState = {
  theme: "system",
  setTheme: () => Promise.resolve()
};

const ThemeProviderContext = createContext<ThemeProviderState>(initialState);

export function ThemeProvider({
  children,
  defaultTheme = "system",
  ...props
}: ThemeProviderProps) {
  const { currentUser } = useContext(AuthProviderContext);

  const [theme, setTheme] = useState<Theme>(defaultTheme);
  void updateUser(currentUser, { theme });

  useEffect(() => {
    const fetchTheme = async () => {
      const root = window.document.documentElement;

      root.classList.remove("light", "dark");

      if (theme === "system") {
        const systemTheme = await appWindow.theme() || "light";
        root.classList.add(systemTheme);
        return;
      }

      root.classList.add(theme);
    }
    void fetchTheme();
  }, [theme]);

  void appWindow.onThemeChanged(async ({ payload: newTheme }) => {
    if ((await getUser(currentUser))!.theme === "system") {
      const root = window.document.documentElement;
      root.classList.remove("light", "dark");
      root.classList.add(newTheme);
      setTheme(newTheme);
      setTheme("system");
    }
  });

  const value = {
    theme,
    setTheme: async (theme: Theme) => {
      await updateUser(currentUser, { theme });
      setTheme(theme);
    },
  };

  return (
    <ThemeProviderContext.Provider {...props} value={value}>
      {children}
    </ThemeProviderContext.Provider>
  );
}

export const useTheme = () => {
  const context = useContext(ThemeProviderContext);

  if (context === undefined)
    throw new Error("useTheme must be used within a ThemeProvider");

  return context;
};
