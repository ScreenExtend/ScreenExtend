import React, { createContext, useContext, useEffect, useState } from "react";
import { appWindow } from "@tauri-apps/api/window";
import { AuthProviderContext } from "@/components/auth-provider";

type Theme = "dark" | "light" | "system";

type ThemeProviderProps = {
  children: React.ReactNode;
  defaultTheme?: Theme;
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
  ...props
}: ThemeProviderProps) {
  // @ts-ignore
  const { currentUser } = useContext(AuthProviderContext);
  const [theme, setTheme] = useState<Theme>(defaultTheme);
  localStorage.setItem(currentUser.username + "-theme", theme);

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

  appWindow.onThemeChanged(({ payload: newTheme }) => {
    if (localStorage.getItem(currentUser.username + "-theme") === "system") {
      const root = window.document.documentElement;
      root.classList.remove("light", "dark");
      root.classList.add(newTheme);
      setTheme(newTheme);
      setTheme("system");
    }
  });

  const value = {
    theme,
    setTheme: (theme: Theme) => {
      localStorage.setItem(currentUser.username + "-theme", theme);
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
