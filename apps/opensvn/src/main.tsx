import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app/app";
import "./styles/globals.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("root element missing");
}

// Unattended WebView measurement: `VITE_PERF_AUTORUN=1 pnpm tauri dev` jumps straight to the spike.
if (
  (import.meta.env.DEV || import.meta.env.VITE_PERF_SPIKE === "1") &&
  import.meta.env.VITE_PERF_AUTORUN &&
  location.pathname === "/"
) {
  history.replaceState(null, "", "/__perf?auto=1");
}

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
