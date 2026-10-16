import { useQuery } from "@tanstack/react-query";
import { File, Folder } from "lucide-react";
import { type FormEvent, lazy, Suspense, useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { Input, Select } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { Sheet } from "../../../components/ui/sheet";
import type { CompareEntry, CompareRequest, WorkingCopy } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { cn } from "../../../lib/cn";
import { useT } from "../../../lib/i18n/i18n";
import { wcPathUrl as joinUrl } from "../wc-model";
import { useWcSession } from "../wc-session";
import { hasTextDiff, isCompareTarget, parseRevision, summaryState } from "./compare-model";
import { compareStrings } from "./compare-strings";

const DiffMergeEditor = lazy(() =>
  import("../diff/diff-merge-editor").then((m) => ({ default: m.DiffMergeEditor })),
);

const STATE_TONE: Record<CompareEntry["change"], string> = {
  normal: "text-info-text",
  modified: "text-warn-text",
  deleted: "text-bad-text",
  added: "text-ok-text",
};

/**
 * Branch > Compare with branch… / Location > Compare with another folder… (doc 01 8.6). Lists
 * what differs between the working copy's location (at a chosen revision, or the working copy
 * itself with its local changes) and another folder of the same repository at a chosen revision,
 * like `svn diff --summarize` (`compare_summarize`), and shows the diff of a file that differs.
 * Nothing is changed.
 */
export function CompareDialog({ onClose }: { onClose: () => void }) {
  const { wc } = useWcSession();
  const t = useT(compareStrings);
  if (!wc) return null;
  return (
    <Sheet
      open
      onClose={onClose}
      title={t(wc.branch ? "titleBranch" : "titleFolder")}
      footer={<Button onClick={onClose}>{t("close")}</Button>}
    >
      <CompareBody wc={wc} />
    </Sheet>
  );
}

/** HEAD or a revision number, for one side. */
function RevisionField({
  label,
  text,
  onText,
  disabled,
}: {
  label: string;
  text: string | null;
  onText: (text: string | null) => void;
  disabled?: boolean;
}) {
  const t = useT(compareStrings);
  const id = useId();
  const invalid = text !== null && parseRevision(text) === undefined;
  return (
    <div className="flex flex-col gap-1">
      <label htmlFor={`${id}-mode`}>{label}</label>
      <div className="flex items-center gap-1.5">
        <Select
          id={`${id}-mode`}
          value={text === null ? "head" : "rev"}
          disabled={disabled}
          onChange={(e) => onText(e.target.value === "head" ? null : "")}
        >
          <option value="head">{t("revHead")}</option>
          <option value="rev">{t("revPick")}</option>
        </Select>
        {text !== null && (
          <Input
            aria-label={t("revNumber", { side: label })}
            inputMode="numeric"
            className="w-[110px]"
            value={text}
            disabled={disabled}
            onChange={(e) => onText(e.target.value)}
            aria-invalid={invalid}
            aria-describedby={invalid ? `${id}-err` : undefined}
          />
        )}
      </div>
      {invalid && !disabled && (
        <span id={`${id}-err`} className="text-[12px] text-bad-text" role="alert">
          {t("revInvalid")}
        </span>
      )}
    </div>
  );
}

function CompareBody({ wc }: { wc: WorkingCopy }) {
  const t = useT(compareStrings);
  const urlId = useId();
  const [url, setUrl] = useState("");
  const [leftText, setLeftText] = useState<string | null>(null);
  const [rightText, setRightText] = useState<string | null>(null);
  const [includeLocal, setIncludeLocal] = useState(false);
  const [request, setRequest] = useState<CompareRequest | null>(null);
  const [diff, setDiff] = useState<CompareEntry | null>(null);
  const valid = isCompareTarget(url, wc.repoRootUrl, wc.url);
  const leftRev = leftText === null ? null : parseRevision(leftText);
  const rightRev = rightText === null ? null : parseRevision(rightText);
  const revsValid = (includeLocal || leftRev !== undefined) && rightRev !== undefined;

  const atUrl = wc.branch?.url ?? null;
  const branches = useQuery({
    queryKey: queryKeys.branches(wc.repoRootUrl, atUrl),
    enabled: wc.branch !== null,
    queryFn: () => unwrap(ipc().commands.branchesList(wc.repoRootUrl, atUrl)),
  });
  const others = (branches.data ?? []).filter(
    (b) => b.url.replace(/\/+$/, "") !== wc.url.replace(/\/+$/, ""),
  );

  const summary = useQuery({
    queryKey: ["compare", request],
    enabled: request !== null,
    queryFn: () => unwrap(ipc().commands.compareSummarize(request as CompareRequest)),
  });

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!valid || !revsValid) return;
    setRequest({
      wcId: wc.id,
      leftRev: includeLocal ? null : (leftRev ?? null),
      includeLocal,
      rightUrl: url.trim().replace(/\/+$/, ""),
      rightRev: rightRev ?? null,
    });
    setDiff(null);
  };
  const data = summary.data;
  const leftName = request?.includeLocal
    ? t("leftWc")
    : t("leftAt", { rev: data?.leftRev ?? "HEAD" });
  const rightName = t("rightAt", { rev: data?.rightRev ?? "HEAD" });

  return (
    <div className="flex flex-col gap-3 p-4">
      <SafeText as="p" className="m-0 font-mono text-[12px] text-muted-text">
        {t("current", { name: wc.branch?.name ?? wc.url })}
      </SafeText>
      <form onSubmit={submit} className="flex flex-col gap-3">
        <div className="flex flex-wrap items-end gap-2">
          {wc.branch && (
            <label className="flex flex-col gap-1">
              {t("pickBranch")}
              <Select
                value={others.some((b) => b.url === url) ? url : ""}
                onChange={(e) => {
                  if (e.target.value) setUrl(e.target.value);
                }}
              >
                <option value="">{t("pickBranchNone")}</option>
                {others.map((b) => (
                  <option key={b.url} value={b.url}>
                    {b.name}
                  </option>
                ))}
              </Select>
            </label>
          )}
          <label htmlFor={urlId} className="flex min-w-[260px] flex-1 flex-col gap-1">
            {t("otherUrl")}
            <Input
              id={urlId}
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              spellCheck={false}
              aria-invalid={url.length > 0 && !valid}
              aria-describedby={`${urlId}-note`}
            />
          </label>
        </div>
        {url.length > 0 && !valid && (
          <span className="text-bad-text" role="alert">
            {t("otherUrlInvalid", { root: wc.repoRootUrl })}
          </span>
        )}
        <div className="flex flex-wrap items-start gap-4">
          <RevisionField
            label={t("leftRev")}
            text={leftText}
            onText={setLeftText}
            disabled={includeLocal}
          />
          <RevisionField label={t("rightRev")} text={rightText} onText={setRightText} />
        </div>
        <label className="flex items-center gap-1.5">
          <input
            type="checkbox"
            checked={includeLocal}
            onChange={(e) => setIncludeLocal(e.target.checked)}
            className="accent-[var(--accent)]"
          />
          {t("includeLocal")}
        </label>
        <div>
          <Button type="submit" variant="primary" disabled={!valid || !revsValid}>
            {t("compare")}
          </Button>
        </div>
      </form>
      <p id={`${urlId}-note`} className="m-0 text-[12px] text-muted-text">
        {t("note")}
      </p>

      {request === null ? (
        <p className="m-0 text-muted-text">{t("nothingYet")}</p>
      ) : summary.isPending ? (
        <p className="m-0 text-muted-text" role="status">
          {t("loading")}
        </p>
      ) : summary.isError ? (
        <p className="m-0 text-bad-text" role="alert">
          {toAppError(summary.error).message}
        </p>
      ) : data && data.entries.length === 0 ? (
        <p className="m-0 border-t border-line pt-3 text-muted-text" role="status">
          {t("noDifferences")}
        </p>
      ) : (
        data && (
          <>
            <p className="m-0 border-t border-line pt-3 text-muted-text" role="status">
              {t("summary", { count: data.entries.length, left: leftName, right: rightName })}
            </p>
            <table className="w-full border-collapse text-left text-[12px]">
              <caption className="sr-only">
                {t("rows", { left: leftName, right: rightName })}
              </caption>
              <thead>
                <tr className="border-b border-line text-muted-text">
                  <th scope="col" className="px-2 py-1 font-medium">
                    {t("colPath")}
                  </th>
                  <th scope="col" className="px-2 py-1 font-medium">
                    {t("colState")}
                  </th>
                </tr>
              </thead>
              <tbody>
                {data.entries.map((e) => (
                  <SummaryRow
                    key={`${e.path}:${e.change}`}
                    entry={e}
                    selected={diff?.path === e.path && diff.change === e.change}
                    onDiff={() => setDiff(e)}
                  />
                ))}
              </tbody>
            </table>
            {diff && (
              <FileCompare
                wcId={wc.id}
                includeLocal={request.includeLocal}
                leftUrl={diff.change === "added" ? null : joinUrl(wc.url, diff.path)}
                leftRev={data.leftRev}
                rightUrl={diff.change === "deleted" ? null : joinUrl(request.rightUrl, diff.path)}
                rightRev={data.rightRev}
                path={diff.path}
              />
            )}
          </>
        )
      )}
    </div>
  );
}

function SummaryRow({
  entry,
  selected,
  onDiff,
}: {
  entry: CompareEntry;
  selected: boolean;
  onDiff: () => void;
}) {
  const t = useT(compareStrings);
  const Icon = entry.kind === "dir" ? Folder : File;
  const shown = entry.path === "" ? t("root") : entry.path;
  const name = (
    <span className="inline-flex min-w-0 items-center gap-1.5">
      <Icon size={13} aria-hidden="true" className="shrink-0 text-muted-text" />
      <SafeText className="truncate font-mono">{shown}</SafeText>
    </span>
  );
  return (
    <tr className={cn("border-b border-line", selected && "bg-sel")}>
      <td className="px-2 py-1">
        {hasTextDiff(entry) ? (
          <button
            type="button"
            onClick={onDiff}
            aria-label={t("showDiff", { name: entry.path })}
            aria-pressed={selected}
            className="cursor-pointer border-0 bg-transparent p-0 text-left text-accent-text underline-offset-2 hover:underline focus-visible:outline-2 focus-visible:outline-accent"
          >
            {name}
          </button>
        ) : (
          name
        )}
      </td>
      <td className={cn("px-2 py-1", STATE_TONE[entry.change])}>{t(summaryState(entry))}</td>
    </tr>
  );
}

/**
 * Text diff of one file between the two sides (a missing side shows as empty). With local
 * changes the left side is the working file, read like the Changes diff.
 */
function FileCompare({
  wcId,
  includeLocal,
  leftUrl,
  leftRev,
  rightUrl,
  rightRev,
  path,
}: {
  wcId: string;
  includeLocal: boolean;
  leftUrl: string | null;
  leftRev: number | null;
  rightUrl: string | null;
  rightRev: number;
  path: string;
}) {
  const t = useT(compareStrings);
  const q = useQuery({
    queryKey: ["compare-file", wcId, includeLocal, leftUrl, leftRev, rightUrl, rightRev],
    queryFn: async () => {
      const cat = (u: string | null, rev: number | null) =>
        u ? unwrap(ipc().commands.repoCat(u, rev)).then((s) => s.content) : Promise.resolve("");
      const working = () =>
        leftUrl === null
          ? Promise.resolve("")
          : unwrap(
              ipc().commands.diffGet({
                target: { kind: "wc", wcId, path },
                ignoreWhitespace: false,
                forceLarge: false,
              }),
            ).then((d) => d.right?.content ?? "");
      const [left, right] = await Promise.all([
        includeLocal ? working() : cat(leftUrl, leftRev),
        cat(rightUrl, rightRev),
      ]);
      return { left, right };
    },
  });
  const label = t("diffOf", { path });
  return (
    <section
      aria-label={label}
      className="flex min-h-[260px] flex-col gap-1 border-t border-line pt-3"
    >
      <div className="grid grid-cols-2 text-[11px] font-medium tracking-[.06em] text-muted-text uppercase">
        <span>{t(includeLocal ? "diffLeftWc" : "diffLeft")}</span>
        <span>{t("diffRight")}</span>
      </div>
      {q.isPending ? (
        <p className="m-0 text-muted-text">{t("loading")}</p>
      ) : q.isError ? (
        <p className="m-0 text-bad-text" role="alert">
          {toAppError(q.error).message}
        </p>
      ) : (
        <div className="h-[320px] min-h-0 rounded-md border border-line">
          <Suspense fallback={<p className="m-0 p-4 text-muted-text">{t("loading")}</p>}>
            <DiffMergeEditor left={q.data.left} right={q.data.right} mode="split" label={label} />
          </Suspense>
        </div>
      )}
    </section>
  );
}
