import { commands } from "@/lib/bindings";

// In a release build no devtools are attached to the webview, so `console.error` goes nowhere.
// Forward it to the Rust log bus (the Settings log terminal, `get_log_backlog`,
// `ScreenExtend logs`). Wrapped rather than edited per call site: uncaught exceptions,
// rejected promises and dependencies' errors count too. The original call is made first.

const MAX_CHARS = 2000;

let forwarding = false;

function describe(value: unknown): string {
  if (value instanceof Error) return value.stack || `${value.name}: ${value.message}`;
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}

function forward(parts: unknown[]): void {
  // must not recurse: reporting a failure of the bridge would call console.error again
  if (forwarding) return;
  forwarding = true;
  try {
    const text = parts.map(describe).join(" ").slice(0, MAX_CHARS);
    if (text) void commands.logFrontendError(text).catch(() => {});
  } catch {
    // nowhere left to report it
  } finally {
    forwarding = false;
  }
}

export function installErrorReporting(): void {
  const original = console.error.bind(console);
  console.error = (...args: unknown[]) => {
    original(...args);
    forward(args);
  };

  window.addEventListener("error", event => {
    forward(["uncaught", event.error ?? event.message, `at ${event.filename}:${event.lineno}`]);
  });

  window.addEventListener("unhandledrejection", event => {
    forward(["unhandled rejection", event.reason]);
  });
}
