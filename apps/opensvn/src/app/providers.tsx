import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { type ReactNode, useEffect, useMemo } from "react";
import { ToastProvider } from "../components/ui/toast";
import { ipc } from "../ipc/client";
import { createEventBus, type EventBus } from "../ipc/events";
import { applyTheme, readTheme } from "../lib/theme";

export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false, staleTime: 5_000 } },
  });
}

/** App-wide providers: theme bootstrap, TanStack Query, backend event bus -> cache, toasts. */
export function Providers({ children }: { children: ReactNode }) {
  const qc = useMemo(createQueryClient, []);
  const bus: EventBus = useMemo(() => createEventBus(ipc, qc), [qc]);

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
      <ToastProvider>{children}</ToastProvider>
    </QueryClientProvider>
  );
}
