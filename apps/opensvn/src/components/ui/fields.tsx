import type { ComponentProps, SelectHTMLAttributes, TextareaHTMLAttributes } from "react";
import { cn } from "../../lib/cn";

const BASE =
  "w-full rounded-md border border-line bg-surface-2 px-2 py-1.5 text-fg placeholder:text-muted-text";

/** Text input; `ref` is forwarded (React 19 passes it as a prop). */
export function Input({ className, ...rest }: ComponentProps<"input">) {
  return <input className={cn(BASE, className)} {...rest} />;
}

export function Select({ className, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  return <select className={cn(BASE, className)} {...rest} />;
}

export function Textarea({ className, ...rest }: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea className={cn(BASE, "resize-y", className)} {...rest} />;
}
