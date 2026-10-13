import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import type { Connect, Plugin } from "vite";
import { defineConfig } from "vitest/config";

function receivePerfReport(middlewares: Connect.Server): void {
  middlewares.use("/__perf-report", (req, res) => {
    let body = "";
    req.on("data", (chunk) => {
      body += chunk;
      if (body.length > 1_000_000) req.destroy();
    });
    req.on("end", () => {
      console.log(`[perf-report] ${body}`);
      res.statusCode = 204;
      res.end();
    });
  });
}

/** Dev/preview only: prints the JSON posted by /__perf (unattended WebView measurements) to the terminal. */
function perfReportPlugin(): Plugin {
  return {
    name: "opensvn-perf-report",
    configureServer: (server) => receivePerfReport(server.middlewares),
    configurePreviewServer: (server) => receivePerfReport(server.middlewares),
  };
}

// Fixed dev port: must match build.devUrl in src-tauri/tauri.conf.json.
export default defineConfig(({ mode }) => {
  // `pnpm perf:spike` (--mode perf): keep the /__perf route in a production bundle and auto-run it.
  if (mode === "perf") {
    process.env.VITE_PERF_SPIKE = "1";
    process.env.VITE_PERF_AUTORUN = "1";
  }
  return {
    plugins: [react(), tailwindcss(), perfReportPlugin()],
    clearScreen: false,
    server: { port: 1420, strictPort: true, host: "127.0.0.1" },
    // Never inline assets as data: URIs; CSP font-src is 'self' only.
    build: { target: "es2022", outDir: "dist", assetsInlineLimit: 0 },
    test: {
      environment: "jsdom",
      include: ["src/**/*.test.{ts,tsx}"],
      setupFiles: ["src/test-setup.ts"],
      css: false,
    },
  };
});
