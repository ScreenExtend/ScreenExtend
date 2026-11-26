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
    // otherwise a renamed file kills every vite command with a bare ENOENT from the config
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

// dev only: vite serves `?worker` as a module worker, which Safari 13.1 cannot construct,
// so the monaco workers are bundled as classic IIFE scripts (production relies on
// `worker.format: "iife"` below)
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
    // drop the memoised bundle, or an edited worker entry keeps serving the stale build
    handleHotUpdate({ file }) {
      for (const [name, entry] of Object.entries(WORKER_ENTRIES)) {
        if (file === path.resolve(__dirname, entry)) bundled.delete(name);
      }
    },
  };
}

// preload the webfont: declared in the 50 KB render-blocking stylesheet, its fetch would
// otherwise wait for that CSS to parse while the fallback paints (`font-display: swap`).
// The filename carries a content hash, so it is read from the emitted bundle
function preloadWebfont(): Plugin {
  return {
    name: "screenextend:preload-webfont",
    apply: "build",
    enforce: "post",
    transformIndexHtml(html, ctx) {
      const font = Object.keys(ctx.bundle ?? {}).find(name => /Geist.*\.woff2$/.test(name));
      if (!font) return html;
      return {
        html,
        tags: [
          {
            tag: "link",
            attrs: {
              rel: "preload",
              href: `/${font}`,
              as: "font",
              type: "font/woff2",
              crossorigin: "",
            },
            injectTo: "head",
          },
        ],
      };
    },
  };
}

// https://vitejs.dev/config/
export default defineConfig(async () => ({
  plugins: [react(), classicWorkers(), preloadWebfont()],

  build: {
    target: ["es2020", "chrome87", "safari13"],
    // the default, kept so a new oversized chunk shows up as a warning (monaco trips it)
    chunkSizeWarningLimit: 500,
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
      },
    },
  },

  // pinned rather than left to vite's default: Safari 13.1 cannot construct a module
  // worker, and `classicWorkers` only covers the dev server
  worker: { format: "iife" },

  // esbuild's "safari13" is 13.0 while browserslist says 13.1, so `?.` and `??` (13.1) are
  // lowered needlessly. That errs on the safe side: keep it
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

  // keep vite from clearing rust errors off the terminal
  clearScreen: false,
  // tauri expects a fixed port: fail if it is taken
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
      ignored: ["**/src-tauri/**"],
    },
  },
}));
