import type { ElementType, ReactNode } from "react";

/**
 * Renders untrusted strings (paths, commit messages, author names, server text) as plain text.
 * React escapes children, so markup like `<script>` shows literally. This component exists as the
 * single, lint-friendly sink for untrusted content: never use dangerouslySetInnerHTML/innerHTML.
 */
export function SafeText({
  children,
  as: Tag = "span",
  className,
}: {
  children: ReactNode;
  as?: ElementType;
  className?: string;
}) {
  return <Tag className={className}>{children}</Tag>;
}
