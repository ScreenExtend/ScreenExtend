import { build, defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import path, { resolve } from "path";
import { readFileSync } from "fs";

const host = process.env.TAURI_DEV_HOST;

function monacoDeps(): string[] {
  const found = new Set<string>(["@monaco-editor/react"]);
  const pattern = /["'](monaco-editor\/esm\/[^"']+)["']/g;
  for (const file of ["src/lib/monaco-setup.ts", "src/components/json-editor.tsx"]) {
    const text = readFileSync(resolve(__dirname, file), "utf8");
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
  };
}

// https://vitejs.dev/config/
export default defineConfig(async () => ({
  plugins: [react(), classicWorkers()],

  build: {
    target: ["es2020", "chrome87", "safari13"],
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
      },
    },
  },

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
