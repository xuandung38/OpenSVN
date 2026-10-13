import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createContext, type ReactNode, useContext, useEffect, useMemo } from "react";
import { ToastProvider } from "../components/ui/toast";
import { ipc } from "../ipc/client";
import { createEventBus, type EventBus } from "../ipc/events";
import { useSettings } from "../ipc/queries";
import { detectLang, I18nProvider } from "../lib/i18n/i18n";
import { applyTheme, readTheme } from "../lib/theme";

export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false, staleTime: 5_000 } },
  });
}

const BusContext = createContext<EventBus | null>(null);

/** The app-wide backend event bus (auth prompts, menu events, ...). */
export function useEventBus(): EventBus {
  const bus = useContext(BusContext);
  if (!bus) throw new Error("useEventBus must be used inside <Providers>");
  return bus;
}

/** UI language: the saved setting once loaded, otherwise the OS locale. */
function SettingsI18n({ children }: { children: ReactNode }) {
  const { data } = useSettings();
  const lang = data?.["general.language"] ?? detectLang(navigator.language);
  useEffect(() => {
    document.documentElement.lang = lang;
  }, [lang]);
  return <I18nProvider lang={lang}>{children}</I18nProvider>;
}

/** App-wide providers: theme bootstrap, TanStack Query, backend event bus -> cache, toasts. */
export function Providers({ children }: { children: ReactNode }) {
  const qc = useMemo(createQueryClient, []);
  const bus: EventBus = useMemo(() => createEventBus(ipc(), qc), [qc]);

  useEffect(() => {
    applyTheme(readTheme());
  }, []);

  useEffect(() => {
    let cancelled = false;
    bus.start().catch((e: unknown) => {
      if (!cancelled) console.error("event bus failed to start", e);
    });
    return () => {
      cancelled = true;
      bus.stop();
    };
  }, [bus]);

  return (
    <QueryClientProvider client={qc}>
      <BusContext.Provider value={bus}>
        <SettingsI18n>
          <ToastProvider>{children}</ToastProvider>
        </SettingsI18n>
      </BusContext.Provider>
    </QueryClientProvider>
  );
}
