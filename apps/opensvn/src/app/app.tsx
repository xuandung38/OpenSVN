import { lazy, Suspense } from "react";
import { AuthPromptHost } from "../features/remote/auth/auth-prompt-host";
import { AppShell } from "./app-shell";
import { Providers } from "./providers";

// Dev-only routes sit behind import.meta.env.DEV so production bundles never contain them.
// VITE_PERF_SPIKE=1 also keeps the spike in a production build (release-mode WebView measurements).
const PERF_ENABLED = import.meta.env.DEV || import.meta.env.VITE_PERF_SPIKE === "1";
const PerfPage = PERF_ENABLED
  ? lazy(() => import("../perf/perf-page").then((m) => ({ default: m.PerfPage })))
  : null;
const GalleryPage = import.meta.env.DEV
  ? lazy(() => import("../gallery/gallery-page").then((m) => ({ default: m.GalleryPage })))
  : null;

export function App() {
  const path = location.pathname;
  return (
    <Providers>
      <AuthPromptHost />
      <Suspense fallback={null}>
        {PerfPage && path === "/__perf" ? (
          <PerfPage />
        ) : GalleryPage && path === "/__gallery" ? (
          <GalleryPage />
        ) : (
          <AppShell />
        )}
      </Suspense>
    </Providers>
  );
}
