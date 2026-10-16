import { useMutation, useQuery } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { SafeText } from "../../../components/ui/safe-text";
import { Sheet } from "../../../components/ui/sheet";
import { useToast } from "../../../components/ui/toast";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { cn } from "../../../lib/cn";
import { formatBytes } from "../../../lib/format";
import { useT } from "../../../lib/i18n/i18n";
import { helpStrings } from "./help-strings";

/**
 * Help > Report a problem… (doc 01 8.8): builds the diagnostics bundle (secrets always removed;
 * paths and commit messages hidden by default), shows every file in full, and saves exactly that
 * bundle as a zip where the user chooses. Nothing is sent anywhere.
 */
export function ReportProblemDialog({ onClose }: { onClose: () => void }) {
  const t = useT(helpStrings);
  const toast = useToast();
  const listId = useId();
  const [hidePaths, setHidePaths] = useState(true);
  const [hideCommitMessages, setHideCommitMessages] = useState(true);
  const [shown, setShown] = useState<string | null>(null);

  const preview = useQuery({
    queryKey: ["diagnostics-preview", hidePaths, hideCommitMessages],
    // Every build has its own single-use token: never serve an old one from the cache.
    gcTime: 0,
    staleTime: Number.POSITIVE_INFINITY,
    queryFn: () => unwrap(ipc().commands.diagnosticsPreview({ hidePaths, hideCommitMessages })),
  });
  const save = useMutation({
    mutationFn: (token: string) => unwrap(ipc().commands.diagnosticsExport(token)),
    onSuccess: (path) => {
      if (path === null) return; // Save dialog cancelled: the preview stays valid.
      toast.show(t("reportSaved", { path }));
      onClose();
    },
  });

  const entries = preview.data?.entries ?? [];
  const current = entries.find((e) => e.name === shown) ?? entries[0] ?? null;
  const options = [
    [hidePaths, setHidePaths, "reportHidePaths"],
    [hideCommitMessages, setHideCommitMessages, "reportHideMessages"],
  ] as const;

  return (
    <Sheet
      open
      onClose={onClose}
      title={t("reportTitle")}
      footer={
        <>
          <Button onClick={onClose}>{t("cancel")}</Button>
          <Button
            variant="primary"
            disabled={!preview.data || save.isPending}
            onClick={() => preview.data && save.mutate(preview.data.token)}
          >
            {t("reportSave")}
          </Button>
        </>
      }
    >
      <div className="grid h-full min-h-0 grid-rows-[auto_1fr] gap-3 p-4">
        <div className="flex flex-col gap-2">
          <p className="m-0">{t("reportIntro")}</p>
          <fieldset className="m-0 flex flex-wrap gap-4 border-0 p-0">
            <legend className="sr-only">{t("reportOptions")}</legend>
            {options.map(([value, set, label]) => (
              <label key={label} className="flex items-center gap-1.5">
                <input
                  type="checkbox"
                  checked={value}
                  onChange={(e) => set(e.target.checked)}
                  className="accent-[var(--accent)]"
                />
                {t(label)}
              </label>
            ))}
          </fieldset>
          {save.error && (
            <p className="m-0 text-bad-text" role="alert">
              {toAppError(save.error).message}
            </p>
          )}
        </div>
        {preview.isPending ? (
          <p className="m-0 text-muted-text">{t("reportBuilding")}</p>
        ) : preview.isError ? (
          <div className="flex flex-col items-start gap-2">
            <p className="m-0 text-bad-text" role="alert">
              {toAppError(preview.error).message}
            </p>
            <Button small onClick={() => void preview.refetch()}>
              {t("retry")}
            </Button>
          </div>
        ) : (
          <div className="grid min-h-0 grid-cols-[minmax(160px,240px)_1fr] gap-3">
            <ul
              aria-label={t("reportFiles")}
              id={listId}
              className="m-0 flex list-none flex-col gap-0.5 overflow-auto p-0"
            >
              {entries.map((e) => (
                <li key={e.name}>
                  <button
                    type="button"
                    aria-pressed={current?.name === e.name}
                    onClick={() => setShown(e.name)}
                    className={cn(
                      "flex w-full cursor-pointer items-baseline justify-between gap-2 rounded-md border-0 px-2 py-1 text-left",
                      current?.name === e.name ? "bg-sel" : "bg-transparent hover:bg-surface-2",
                    )}
                  >
                    <SafeText className="min-w-0 truncate font-mono text-[12px]">{e.name}</SafeText>
                    <span className="shrink-0 text-[11px] text-muted-text">
                      {formatBytes(e.size)}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
            {current && (
              <section aria-label={t("reportContent", { name: current.name })} className="min-h-0">
                {/* The bundle as it will be saved, rendered as text (React escapes it). */}
                <pre
                  // biome-ignore lint/a11y/noNoninteractiveTabindex: a scrollable region must be reachable by keyboard
                  tabIndex={0}
                  className="m-0 h-full overflow-auto rounded-md border border-line bg-surface-2 p-2 font-mono text-[12px] leading-[1.6] whitespace-pre-wrap"
                >
                  {current.content}
                </pre>
              </section>
            )}
          </div>
        )}
      </div>
    </Sheet>
  );
}
