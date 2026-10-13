import type { ReactNode } from "react";

/** Centred pane placeholder: bold title, one sentence, up to 3 actions. */
export function EmptyState({
  title,
  description,
  actions,
}: {
  title: string;
  description: string;
  actions?: ReactNode;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-center">
      <b className="text-[15px]">{title}</b>
      <p className="m-0 max-w-[44ch] text-muted">{description}</p>
      {actions && <div className="mt-1 flex gap-2">{actions}</div>}
    </div>
  );
}
