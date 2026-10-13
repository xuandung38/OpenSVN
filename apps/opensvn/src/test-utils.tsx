import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render } from "@testing-library/react";
import userEvent, { type UserEvent } from "@testing-library/user-event";
import type { ReactElement } from "react";
import { afterEach, beforeEach, vi } from "vitest";
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

/**
 * Deterministic clock for a test file: fake timers in every test plus a userEvent bound to them.
 *
 * With real timers, `findBy*`/`waitFor` give up after N wall-clock ms, so a CPU-starved run (the
 * full suite on a loaded machine) fails while the UI is merely slow. On a fake clock, time only
 * moves when Testing Library advances it between checks, so rendering cost no longer eats the
 * budget. Testing Library only recognises fake timers through a global `jest`; the shim forwards to
 * Vitest. Call at module scope and use the returned `user()` instead of the `userEvent` default.
 * `Date` and animation frames stay real so date rendering and virtualised lists behave as before.
 */
export function installFakeClock(): { user: () => UserEvent } {
  let current: UserEvent | null = null;
  const g = globalThis as { jest?: unknown };
  beforeEach(() => {
    vi.useFakeTimers({
      toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval"],
    });
    g.jest = { advanceTimersByTime: (ms: number) => vi.advanceTimersByTime(ms) };
    current = userEvent.setup({ advanceTimers: (ms) => vi.advanceTimersByTime(ms) });
  });
  afterEach(() => {
    vi.runOnlyPendingTimers();
    vi.useRealTimers();
    delete g.jest;
    current = null;
  });
  return {
    user: () => {
      if (!current) throw new Error("installFakeClock().user() called outside a test");
      return current;
    },
  };
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
