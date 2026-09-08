import { commands } from "@/lib/bindings";

// Roughly seventy `console.error` calls across the desktop UI, and in a release build every
// one of them goes nowhere: no devtools are attached to the webview and nothing captures
// its console. Forwarding them to the Rust log bus puts them where every other diagnostic
// already is -- the Settings log terminal, `get_log_backlog` and `ScreenExtend logs`.
//
// Done by wrapping `console.error` rather than by editing each call site, because the call
// sites are not the only source: an uncaught exception or a rejected promise never reaches
// `console.error` at all, and a dependency's error message is worth just as much as ours.
// The original console call is always made first, so devtools behave exactly as before.

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
  // The bridge itself failing is the one thing that must not recurse: reporting it would
  // call console.error again, which would try to report that.
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
