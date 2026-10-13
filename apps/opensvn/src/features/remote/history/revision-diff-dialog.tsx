import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { SafeText } from "../../../components/ui/safe-text";
import { Sheet } from "../../../components/ui/sheet";
import type { FileDiff } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { cn } from "../../../lib/cn";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { historyStrings } from "./history-strings";

function lineClass(line: string): string {
  if (line.startsWith("+")) return "bg-add-bg";
  if (line.startsWith("-")) return "bg-del-bg";
  if (line.startsWith("@")) return "bg-surface-2 text-info-text";
  return "";
}

/**
 * Minimal read-only diff of one path in one revision (hunks when the backend sends them, otherwise
 * the two sides as plain text). Content is always rendered as text; the full side-by-side diff view
 * belongs to the DiffView component.
 */
export function RevisionDiffDialog({
  url,
  rev,
  path,
  onClose,
}: {
  url: string;
  rev: number;
  path: string;
  onClose: () => void;
}) {
  const t = useT(historyStrings);
  const c = useT(commonStrings);
  const q = useQuery({
    queryKey: ["rev-diff", url, rev],
    queryFn: () =>
      unwrap(
        ipc().commands.diffGet({
          target: { kind: "rev", url, rev },
          ignoreWhitespace: null,
          forceLarge: null,
        }),
      ),
  });
  return (
    <Sheet
      open
      onClose={onClose}
      title={t("diffTitle", { rev })}
      footer={<Button onClick={onClose}>{c("close")}</Button>}
    >
      <div className="p-3">
        <SafeText as="p" className="m-0 mb-2 break-all font-mono text-[12px] text-muted-text">
          {path}
        </SafeText>
        {q.isPending && <p className="m-0">{t("diffLoading")}</p>}
        {q.error && (
          <p className="m-0 text-bad-text" role="alert">
            {q.error.message}
          </p>
        )}
        {q.data && <DiffBody diff={q.data} url={url} rev={rev} />}
      </div>
    </Sheet>
  );
}

function DiffBody({ diff, url, rev }: { diff: FileDiff; url: string; rev: number }) {
  const t = useT(historyStrings);
  if (diff.binary) return <p className="m-0">{t("diffBinary")}</p>;
  if (diff.tooComplex || diff.tooLarge) {
    return (
      <ExternalHandOff
        message={t(diff.tooComplex ? "diffTooComplex" : "diffTooLarge")}
        url={url}
        rev={rev}
      />
    );
  }
  if (diff.hunks && diff.hunks.length > 0) {
    return (
      <pre className="m-0 font-mono text-[12px] leading-[1.6]">
        {diff.hunks.map((h) => (
          <div key={`${h.oldStart}-${h.newStart}`}>
            <div className="bg-surface-2 text-info-text">
              {`@@ -${h.oldStart},${h.oldLines} +${h.newStart},${h.newLines} @@`}
            </div>
            {h.lines.map((l, i) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: lines are positional and immutable
              <div key={i} className={cn("whitespace-pre-wrap", lineClass(l))}>
                {l}
              </div>
            ))}
          </div>
        ))}
      </pre>
    );
  }
  if (diff.left || diff.right) {
    return (
      <div className="grid grid-cols-2 gap-3">
        {(
          [
            [t("diffBefore"), diff.left],
            [t("diffAfter"), diff.right],
          ] as const
        ).map(([label, side]) => (
          <section key={label} aria-label={label}>
            <h3 className="m-0 mb-1 text-[11px] font-medium tracking-[.06em] text-muted-text uppercase">
              {label} {side?.label ? `· ${side.label}` : ""}
            </h3>
            <pre className="m-0 overflow-auto rounded-md border border-line bg-surface-2 p-2 font-mono text-[12px] leading-[1.6]">
              {side?.content ?? ""}
            </pre>
          </section>
        ))}
      </div>
    );
  }
  return <p className="m-0">{t("diffEmpty")}</p>;
}

/** A diff the backend will not show: explain why and offer the external diff tool. */
function ExternalHandOff({ message, url, rev }: { message: string; url: string; rev: number }) {
  const t = useT(historyStrings);
  const [error, setError] = useState<string | null>(null);
  const open = async () => {
    setError(null);
    try {
      await unwrap(ipc().commands.diffExternal({ kind: "rev", url, rev }));
    } catch (e) {
      setError(toAppError(e).message);
    }
  };
  return (
    <div className="flex flex-col items-start gap-2">
      <p className="m-0">{message}</p>
      <Button small onClick={() => void open()}>
        {t("openExternalTool")}
      </Button>
      {error && (
        <p className="m-0 text-bad-text" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
