import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path, { resolve } from "path";

// https://vitejs.dev/config/
export default defineConfig({
  build: {
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
      },
    },
  },
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
});
