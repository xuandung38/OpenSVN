import { VirtualList } from "../../../components/list/virtual-list";
import { SafeText } from "../../../components/ui/safe-text";
import { cn } from "../../../lib/cn";
import { relativeTime } from "../../../lib/format";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { blameStrings } from "./blame-strings";
import { useBlame } from "./use-blame";

const COLS = "grid-cols-[64px_96px_84px_44px_1fr]";

/** Per-line blame. Revision/author show only on the first line of each run; code renders as text. */
export function BlameView({
  url,
  rev,
  onOpenRevision,
}: {
  url: string;
  rev: number | null;
  onOpenRevision?: (rev: number) => void;
}) {
  const t = useT(blameStrings);
  const c = useT(commonStrings);
  const blame = useBlame(url, rev);

  if (blame.status === "loading") return <p className="m-0 p-3 text-muted-text">{t("running")}</p>;
  if (blame.status === "error") {
    return (
      <p className="m-0 p-3 text-bad-text" role="alert">
        {t("failed")}: {blame.error?.message ?? ""}
      </p>
    );
  }
  const lines = blame.lines;
  if (lines.length === 0) return <p className="m-0 p-3 text-muted-text">{t("empty")}</p>;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div
        aria-hidden="true"
        className={cn(
          "grid gap-2 border-b border-line bg-surface-2 px-3 py-1.5 text-[10px] font-medium tracking-[.06em] text-muted-text uppercase",
          COLS,
        )}
      >
        <span>{t("colRev")}</span>
        <span>{t("colAuthor")}</span>
        <span>{t("colDate")}</span>
        <span>{t("colLine")}</span>
        <span />
      </div>
      <VirtualList
        label={t("title")}
        className="min-h-0 flex-1 font-mono text-[12px]"
        count={lines.length}
        rowHeight={20}
        onActivate={(i) => {
          const l = lines[i];
          if (l) onOpenRevision?.(l.rev);
        }}
        renderRow={(i, ctx) => {
          const l = lines[i];
          if (!l) return null;
          const first = i === 0 || lines[i - 1]?.rev !== l.rev;
          return (
            // biome-ignore lint/a11y/useFocusableInteractive: focus stays on the listbox (aria-activedescendant)
            // biome-ignore lint/a11y/useKeyWithClickEvents: keyboard handled by the listbox
            <div
              id={ctx.id}
              role="option"
              aria-selected={false}
              onClick={() => {
                ctx.activate();
                onOpenRevision?.(l.rev);
              }}
              className={cn(
                "grid h-full cursor-default items-center gap-2 px-3 hover:bg-surface-2",
                COLS,
                ctx.active &&
                  "group-focus-visible:outline-2 group-focus-visible:-outline-offset-2 group-focus-visible:outline-accent",
              )}
            >
              <span className="tnum text-accent-text" title={t("openRevision", { rev: l.rev })}>
                {first ? `r${l.rev}` : ""}
              </span>
              <SafeText className="truncate font-sans">
                {first ? (l.author ?? c("unknownAuthor")) : ""}
              </SafeText>
              <span className="tnum font-sans text-muted-text">
                {first ? relativeTime(l.date) : ""}
              </span>
              <span className="tnum text-right text-muted-text">{i + 1}</span>
              <SafeText className="truncate whitespace-pre">{l.text}</SafeText>
            </div>
          );
        }}
      />
    </div>
  );
}
