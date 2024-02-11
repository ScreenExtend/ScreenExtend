import React, { createContext, useContext, useEffect, useState } from "react";
import { appWindow } from "@tauri-apps/api/window";

type Theme = "dark" | "light" | "system";

type ThemeProviderProps = {
  children: React.ReactNode;
  defaultTheme?: Theme;
  storageKey?: string;
};

type ThemeProviderState = {
  theme: Theme;
  setTheme: (theme: Theme) => void;
};

const initialState: ThemeProviderState = {
  theme: "system",
  setTheme: () => null,
};

const ThemeProviderContext = createContext<ThemeProviderState>(initialState);

export function ThemeProvider({
  children,
  defaultTheme = "system",
  storageKey = "vite-ui-theme",
  ...props
}: ThemeProviderProps) {
  const [theme, setTheme] = useState<Theme>(
    () => (localStorage.getItem(storageKey) as Theme) || defaultTheme
  );
  localStorage.setItem(storageKey, theme);

  useEffect(() => {
    async function fetchTheme() {
      const root = window.document.documentElement;

      root.classList.remove("light", "dark");

      if (theme === "system") {
        const systemTheme = await appWindow.theme() || "light";
        root.classList.add(systemTheme);
        return;
      }

      root.classList.add(theme);
    }
    fetchTheme();
  }, [theme]);
  
  appWindow.onThemeChanged(async ({ payload: newTheme }) => {
    if (localStorage.getItem(storageKey) === "system") {
      const root = window.document.documentElement;
      root.classList.remove("light", "dark");
      root.classList.add(newTheme);
    }
  });
  
  const value = {
    theme,
    setTheme: (theme: Theme) => {
      localStorage.setItem(storageKey, theme);
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
