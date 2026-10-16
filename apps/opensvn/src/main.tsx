import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app/app";
import { FatalError } from "./app/fatal-error";
import { setShortcutOs, shortcutOsFromQuery } from "./features/wc/shortcuts";
import { initIpc, isTauriRuntime, MOCK_ALLOWED } from "./ipc/client";
import { installTauriOpener } from "./lib/external-link";
import "./styles/globals.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("root element missing");
}
const root = createRoot(container);

// Unattended WebView measurement: `pnpm perf:spike` jumps straight to the spike.
if (
  (import.meta.env.DEV || import.meta.env.VITE_PERF_SPIKE === "1") &&
  import.meta.env.VITE_PERF_AUTORUN &&
  location.pathname === "/"
) {
  history.replaceState(null, "", "/__perf?auto=1");
}

// Dev/mock only: `?os=mac` or `?os=win` shows the shortcuts of that platform (labels and keys).
if (MOCK_ALLOWED) setShortcutOs(shortcutOsFromQuery(location.search));

async function boot(): Promise<void> {
  await initIpc();
  if (isTauriRuntime()) await installTauriOpener();
  root.render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

boot().catch((e: unknown) => {
  console.error("startup failed", e);
  root.render(<FatalError message={e instanceof Error ? e.message : String(e)} />);
});
