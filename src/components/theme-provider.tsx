import React, { createContext, useContext, useEffect, useState } from "react";

import { getConfig, updateConfig } from "@/components/config-provider";
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
  const [theme, setTheme] = useState<Theme>(defaultTheme);

  useEffect(() => {
    const root = window.document.documentElement;
    const resolved =
      theme === "system"
        ? (window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light")
        : theme;
    if (!root.classList.contains(resolved)) {
      root.classList.remove("light", "dark");
      root.classList.add(resolved);
    }
  }, [theme]);

  void appWindow.onThemeChanged(async ({ payload: newTheme }) => {
    if ((await getConfig())!.theme === "system") {
      const root = window.document.documentElement;
      if (!root.classList.contains(newTheme)) {
        root.classList.remove("light", "dark");
        root.classList.add(newTheme);
      }
      setTheme(newTheme);
      setTheme("system");
    }
  });

  const value = {
    theme,
    setTheme: async (theme: Theme) => {
      await updateConfig({ theme });
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
