import { build, defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import path, { resolve } from "path";
import { existsSync, readFileSync } from "fs";

const host = process.env.TAURI_DEV_HOST;

function monacoDeps(): string[] {
  const found = new Set<string>(["@monaco-editor/react"]);
  const pattern = /["'](monaco-editor\/esm\/[^"']+)["']/g;
  for (const file of ["src/lib/monaco-setup.ts", "src/components/json-editor.tsx"]) {
    const full = resolve(__dirname, file);
    // Without this, renaming either file kills every vite command -- dev, build, tauri
    // dev, tauri build -- with a bare ENOENT thrown from inside the config, before any
    // project code is reached. The path is in the message but the reason is not.
    if (!existsSync(full)) {
      throw new Error(
        `vite.config: monacoDeps() lists ${file}, which does not exist. ` +
          "Update the list if the file was renamed or moved."
      );
    }
    const text = readFileSync(full, "utf8");
    let hit: RegExpExecArray | null;
    while ((hit = pattern.exec(text))) found.add(hit[1]);
  }
  return [...found];
}

const WORKER_PREFIX = "/@classic-worker/";
const WORKER_ENTRIES: Record<string, string> = {
  "monaco-editor.worker": "src/lib/monaco-editor.worker.ts",
  "monaco-json.worker": "src/lib/monaco-json.worker.ts",
};

// Vite's dev server serves `?worker` as a module worker and safari 13.1 cannot construct
// one, so the two monaco language services are bundled here as classic IIFE scripts and
// served from `/@classic-worker/`. Production does not go through this at all -- there the
// same guarantee comes from `worker.format: "iife"` below.
function classicWorkers(): Plugin {
  const bundled = new Map<string, string>();
  return {
    name: "screenextend:classic-workers",
    apply: "serve",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const url = req.url ?? "";
        if (!url.startsWith(WORKER_PREFIX)) return next();
        const name = url
          .slice(WORKER_PREFIX.length)
          .replace(/[?#].*$/, "")
          .replace(/[.]js$/, "");
        const entry = WORKER_ENTRIES[name];
        if (!entry) return next();

        const send = (code: string) => {
          res.setHeader("Content-Type", "text/javascript");
          res.end(code);
        };

        const cached = bundled.get(name);
        if (cached) {
          send(cached);
          return;
        }

        void (async () => {
          try {
            const result = await build({
              configFile: false,
              logLevel: "warn",
              resolve: { alias: { "@": path.resolve(__dirname, "./src") } },
              build: {
                write: false,
                target: "safari13",
                lib: {
                  entry: path.resolve(__dirname, entry),
                  formats: ["iife"],
                  name: "ScreenExtendWorker",
                  fileName: () => `${name}.js`,
                },
              },
            });
            const first = Array.isArray(result) ? result[0] : result;
            const { output } = first as unknown as { output: { code?: string }[] };
            const code = output[0]?.code ?? "";
            bundled.set(name, code);
            send(code);
          } catch (error) {
            next(error);
          }
        })();
      });
    },
    // The bundle is memoised for the life of the dev server, so without this an edit to a
    // worker entry keeps serving the stale build until the server is restarted.
    handleHotUpdate({ file }) {
      for (const [name, entry] of Object.entries(WORKER_ENTRIES)) {
        if (file === path.resolve(__dirname, entry)) bundled.delete(name);
      }
    },
  };
}

// https://vitejs.dev/config/
export default defineConfig(async () => ({
  plugins: [react(), classicWorkers()],

  build: {
    target: ["es2020", "chrome87", "safari13"],
    // Left at rollup's default on purpose: the monaco chunk is expected to trip it, and
    // the warning is how a new oversized chunk becomes visible in the build output.
    chunkSizeWarningLimit: 500,
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
      },
    },
  },

  // Safari 13.1 cannot construct `new Worker(url, { type: "module" })`, and the monaco
  // language services are workers. `classicWorkers` below only covers the dev server;
  // production correctness rested entirely on vite's default happening to be "iife".
  worker: { format: "iife" },

  // esbuild's "safari13" means 13.0, while browserslist in package.json says 13.1. The
  // difference is `?.` and `??`, which landed in 13.1: esbuild therefore lowers slightly
  // more than strictly necessary, which errs in the safe direction and should stay.
  esbuild: { target: "safari13" },
  optimizeDeps: {
    include: monacoDeps(),
    esbuildOptions: { target: "safari13" },
  },

  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      "next/navigation": path.resolve(__dirname, "./src/lib/next-navigation-stub.ts"),
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
