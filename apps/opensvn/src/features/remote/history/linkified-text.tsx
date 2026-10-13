import { SafeLink } from "../../../components/ui/safe-link";

const URL_RE = /https:\/\/[^\s<>"']+/g;

/** Splits text into plain parts and https URLs (trailing punctuation stays outside the link). */
export function splitLinks(text: string): { text: string; link: boolean }[] {
  const parts: { text: string; link: boolean }[] = [];
  let last = 0;
  for (const m of text.matchAll(URL_RE)) {
    const start = m.index ?? 0;
    const raw = m[0];
    const url = raw.replace(/[.,;:!?)\]]+$/, "");
    if (start > last) parts.push({ text: text.slice(last, start), link: false });
    parts.push({ text: url, link: true });
    last = start + url.length;
  }
  if (last < text.length) parts.push({ text: text.slice(last), link: false });
  return parts;
}

/**
 * Commit message renderer: always text nodes; only absolute https URLs become links, opened in the
 * system browser by SafeLink. Never HTML or markdown.
 */
export function LinkifiedText({ text, className }: { text: string; className?: string }) {
  return (
    <p className={className ?? "m-0 whitespace-pre-wrap break-words"}>
      {splitLinks(text).map((p, i) =>
        p.link ? (
          // biome-ignore lint/suspicious/noArrayIndexKey: static split of an immutable string
          <SafeLink key={i} href={p.text}>
            {p.text}
          </SafeLink>
        ) : (
          // biome-ignore lint/suspicious/noArrayIndexKey: static split of an immutable string
          <span key={i}>{p.text}</span>
        ),
      )}
    </p>
  );
}
