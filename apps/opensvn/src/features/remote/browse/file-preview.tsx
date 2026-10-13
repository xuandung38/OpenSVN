import { EmptyState } from "../../../components/ui/empty-state";
import { useT } from "../../../lib/i18n/i18n";
import { browseStrings } from "./browse-strings";
import { useRepoFile } from "./use-repo";

/** Lines rendered at once; larger files show a notice instead of building a huge DOM. */
export const MAX_PREVIEW_LINES = 2000;

/** Text preview of a file at a revision. Always a text node inside `<pre>`: never HTML or markdown. */
export function FilePreview({ url, rev }: { url: string; rev: number | null }) {
  const t = useT(browseStrings);
  const q = useRepoFile(url, rev);
  if (q.isPending) return <p className="m-0 p-3 text-muted-text">…</p>;
  if (q.error) return <EmptyState title={t("loadError")} description={q.error.message} />;
  const lines = q.data.content.split("\n");
  const shown = lines.length > MAX_PREVIEW_LINES ? lines.slice(0, MAX_PREVIEW_LINES) : lines;
  return (
    // Scrollable text must be keyboard-focusable (scroll with arrow keys).
    // biome-ignore lint/a11y/noNoninteractiveTabindex: scrollable region
    <section aria-label={url} tabIndex={0} className="h-full overflow-auto">
      {lines.length > MAX_PREVIEW_LINES && (
        <p className="m-0 border-b border-line bg-warn-soft px-3 py-1.5">
          {t("previewTruncated", { n: MAX_PREVIEW_LINES })}
        </p>
      )}
      <pre className="m-0 p-3 font-mono text-[12px] leading-[1.6]">{shown.join("\n")}</pre>
    </section>
  );
}
