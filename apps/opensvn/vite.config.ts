import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// Fixed dev port: must match build.devUrl in src-tauri/tauri.conf.json.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true, host: "127.0.0.1" },
  build: { target: "es2022", outDir: "dist" },
  test: { environment: "jsdom", include: ["src/**/*.test.{ts,tsx}"] },
});
