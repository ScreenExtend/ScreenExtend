import { useSyncExternalStore } from "react";

type Dict = { [key: string]: string | Dict };

const modules = import.meta.glob<{ default: Dict }>("./locales/*.json", {
  eager: true,
});

const locales: Record<string, Dict> = {};
for (const path in modules) {
  const code = path.slice(path.lastIndexOf("/") + 1, -".json".length);
  locales[code] = modules[path].default;
}

export const DEFAULT_LOCALE = "en";
const STORAGE_KEY = "locale";

function readStoredLocale(): string {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved && locales[saved]) return saved;
  } catch {
    // localStorage may be unavailable; fall through to the default
  }
  return DEFAULT_LOCALE;
}

let currentLocale = readStoredLocale();
const listeners = new Set<() => void>();

export function getLocale(): string {
  return currentLocale;
}

export function availableLocales(): string[] {
  return Object.keys(locales);
}

export function setLocale(code: string): void {
  if (code === currentLocale || !locales[code]) return;
  currentLocale = code;
  try {
    localStorage.setItem(STORAGE_KEY, code);
  } catch {
    // ignore write failures; the in-memory locale still updates
  }
  listeners.forEach((listener) => listener());
}

function lookup(dict: Dict | undefined, path: string[]): string | undefined {
  let node: string | Dict | undefined = dict;
  for (const key of path) {
    if (node && typeof node === "object" && key in node) {
      node = node[key];
    } else {
      return undefined;
    }
  }
  return typeof node === "string" ? node : undefined;
}

function interpolate(text: string, vars?: Record<string, string | number>): string {
  if (!vars) return text;
  return text.replace(/\{(\w+)\}/g, (match, name: string) =>
    name in vars ? String(vars[name]) : match
  );
}

export function t(key: string, vars?: Record<string, string | number>): string {
  const path = key.split(".");
  const value =
    lookup(locales[currentLocale], path) ?? lookup(locales[DEFAULT_LOCALE], path);
  return value === undefined ? key : interpolate(value, vars);
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

// One frozen handle per locale, so the return value is a stable dependency and
// `availableLocales()` is not re-allocated on every render of every consumer.
const handles = new Map<string, ReturnType<typeof makeHandle>>();
const ALL_LOCALES = Object.keys(locales);

function makeHandle(locale: string) {
  return { t, locale, setLocale, locales: ALL_LOCALES };
}

export function useTranslation() {
  const locale = useSyncExternalStore(subscribe, getLocale, getLocale);
  let handle = handles.get(locale);
  if (!handle) {
    handle = makeHandle(locale);
    handles.set(locale, handle);
  }
  return handle;
}
