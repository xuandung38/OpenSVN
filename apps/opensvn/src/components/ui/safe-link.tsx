import type { MouseEvent, ReactNode } from "react";
import { openExternalUrl } from "../../lib/external-link";

/** Only absolute https URLs are ever linkable. Everything else (javascript:, data:, file:) is rejected. */
export function isSafeHttpsUrl(url: string): boolean {
  try {
    const u = new URL(url);
    return u.protocol === "https:";
  } catch {
    return false;
  }
}

/**
 * Link to server-provided or user-provided URLs. Unsafe schemes render as plain text; safe ones open
 * in the system browser (never inside the app WebView).
 */
export function SafeLink({
  href,
  children,
  className,
}: {
  href: string;
  children: ReactNode;
  className?: string;
}) {
  if (!isSafeHttpsUrl(href)) return <span className={className}>{children}</span>;
  const onClick = (e: MouseEvent) => {
    e.preventDefault();
    void openExternalUrl(href);
  };
  return (
    <a
      href={href}
      onClick={onClick}
      rel="noopener noreferrer"
      className={className ?? "text-accent underline"}
    >
      {children}
    </a>
  );
}
