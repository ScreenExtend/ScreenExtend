import React, { createContext, useContext, useEffect, useState } from "react";

import { getConfig, updateConfig } from "@/components/config-provider";
import { toast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
const appWindow = getCurrentWebviewWindow();

const stopListening = (unlisten: () => void) => {
  void Promise.resolve(unlisten()).catch(e => console.error("theme unlisten failed", e));
};

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
  const { t } = useTranslation();

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

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    void appWindow.onThemeChanged(async ({ payload: newTheme }) => {
      try {
        if ((await getConfig())?.theme === "system") {
          const root = window.document.documentElement;
          if (!root.classList.contains(newTheme)) {
            root.classList.remove("light", "dark");
            root.classList.add(newTheme);
          }
          setTheme(newTheme);
          setTheme("system");
        }
      } catch (e) {
        console.error("OS theme change handler failed", e);
      }
    })
    .then(un => {
      if (disposed) stopListening(un);
      else unlisten = un;
    })
    .catch(e => {
      console.error("failed to listen for OS theme changes", e);
    });
    return () => {
      disposed = true;
      if (unlisten) stopListening(unlisten);
    };
  }, []);

  const value = {
    theme,
    setTheme: async (next: Theme) => {
      const previous = theme;
      setTheme(next);
      try {
        await updateConfig({ theme: next });
      } catch {
        setTheme(previous);
        toast({
          variant: "destructive",
          title: t("toasts.theme.failureTitle"),
          description: t("toasts.theme.failureDescription"),
        });
      }
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
