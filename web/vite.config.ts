import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The console is served by the bot's admin listener under /console/,
// so every asset URL must be relative to that prefix.
export default defineConfig({
  plugins: [react()],
  base: "/console/",
  build: {
    outDir: "dist",
  },
  server: {
    proxy: {
      "/api": "http://127.0.0.1:18081",
    },
  },
});
