import { createContext, type ReactNode, useCallback, useContext, useMemo, useState } from "react";
import { SafeText } from "./safe-text";

interface ToastItem {
  id: number;
  text: string;
}
interface ToastApi {
  /** Show a toast; auto-hides after `ms` (default 4000, within the 2.6-5s guideline). */
  show: (text: string, ms?: number) => void;
}

const ToastContext = createContext<ToastApi | null>(null);
let nextId = 1;

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const show = useCallback((text: string, ms = 4000) => {
    const id = nextId++;
    setItems((cur) => [...cur, { id, text }]);
    setTimeout(() => setItems((cur) => cur.filter((t) => t.id !== id)), ms);
  }, []);
  const api = useMemo(() => ({ show }), [show]);
  return (
    <ToastContext.Provider value={api}>
      {children}
      <div
        role="status"
        aria-live="polite"
        className="pointer-events-none fixed right-4 bottom-4 z-10 flex flex-col gap-2"
      >
        {items.map((t) => (
          <SafeText
            key={t.id}
            as="div"
            className="rounded-[7px] bg-fg px-3 py-2 text-surface shadow-float"
          >
            {t.text}
          </SafeText>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

export function useToast(): ToastApi {
  const ctx = useContext(ToastContext);
  if (!ctx) throw new Error("useToast must be used inside <ToastProvider>");
  return ctx;
}
