import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render } from "@testing-library/react";
import type { ReactElement } from "react";
import { vi } from "vitest";
import { Providers } from "./app/providers";
import { ToastProvider } from "./components/ui/toast";
import { initIpc } from "./ipc/client";
import { mockAdapter } from "./ipc/mock/adapter";
import { I18nProvider, type Lang } from "./lib/i18n/i18n";

/** jsdom has no layout: give elements a viewport so virtualised lists mount rows. */
export function stubLayout(width = 900, height = 400): void {
  const rect = { width, height, top: 0, left: 0, right: width, bottom: height, x: 0, y: 0 };
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({
    ...rect,
    toJSON: () => rect,
  });
  for (const [prop, value] of [
    ["offsetHeight", height],
    ["offsetWidth", width],
    ["clientHeight", height],
    ["clientWidth", width],
  ] as const) {
    Object.defineProperty(HTMLElement.prototype, prop, { configurable: true, value });
  }
}

/** Selects the mock adapter (jsdom has no Tauri bridge) and pins the UI language. */
export async function setupMockIpc(lang: Lang = "en"): Promise<void> {
  await initIpc();
  await mockAdapter.commands.settingsSet({ "general.language": lang });
}

/** Render with query cache, event bus, i18n (from mock settings) and toasts. */
export function renderApp(ui: ReactElement) {
  return render(<Providers>{ui}</Providers>);
}

/** Lighter wrapper: own QueryClient, fixed language, no event bus. */
export function renderWithQuery(ui: ReactElement, lang: Lang = "en") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <I18nProvider lang={lang}>
        <ToastProvider>{ui}</ToastProvider>
      </I18nProvider>
    </QueryClientProvider>,
  );
}
