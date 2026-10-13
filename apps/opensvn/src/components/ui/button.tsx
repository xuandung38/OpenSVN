import type { ButtonHTMLAttributes } from "react";
import { cn } from "../../lib/cn";

export type ButtonVariant = "default" | "primary" | "danger";

const VARIANT: Record<ButtonVariant, string> = {
  default: "border-line bg-surface text-fg hover:bg-surface-2",
  primary: "border-accent bg-accent font-semibold text-on-accent hover:brightness-110",
  danger: "border-bad bg-bad font-semibold text-on-bad hover:brightness-110",
};

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  /** 11px compact button used inside blocks (conflict hunks, banners). */
  small?: boolean;
}

export function Button({
  variant = "default",
  small = false,
  className,
  type = "button",
  ...rest
}: ButtonProps) {
  return (
    <button
      type={type}
      className={cn(
        "inline-flex cursor-pointer items-center justify-center gap-1.5 rounded-md border whitespace-nowrap",
        "disabled:cursor-default disabled:opacity-[.45]",
        small ? "px-2 py-[3px] text-[11px]" : "px-3 py-1.5 text-[13px]",
        VARIANT[variant],
        className,
      )}
      {...rest}
    />
  );
}
