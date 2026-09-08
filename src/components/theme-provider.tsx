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

// A synchronous paint hint for the inline script in index.html, which runs long before
// the store can be read over IPC. `config.json` stays the source of truth: bootstrap
// overwrites whatever is seeded from here as soon as it has read it.
const THEME_HINT_KEY = "se-theme";

const readThemeHint = (): Theme | null => {
  try {
    const stored = localStorage.getItem(THEME_HINT_KEY);
    return stored === "dark" || stored === "light" || stored === "system" ? stored : null;
  } catch {
    return null;
  }
};

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
  const [theme, setTheme] = useState<Theme>(() => readThemeHint() ?? defaultTheme);
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
    try {
      localStorage.setItem(THEME_HINT_KEY, theme);
    } catch {
      // a private-mode webview without storage still gets prefers-color-scheme
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
