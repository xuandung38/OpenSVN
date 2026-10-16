import { useQuery } from "@tanstack/react-query";
import { ArrowUp, File, Folder } from "lucide-react";
import { type FormEvent, lazy, Suspense, useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { Input, Select } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { Sheet } from "../../../components/ui/sheet";
import type { WorkingCopy } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { cn } from "../../../lib/cn";
import { useT } from "../../../lib/i18n/i18n";
import { wcPathUrl as joinUrl } from "../wc-model";
import { useWcSession } from "../wc-session";
import {
  type CompareRow,
  type CompareState,
  compareListings,
  isCompareTarget,
} from "./compare-model";
import { compareStrings } from "./compare-strings";

const DiffMergeEditor = lazy(() =>
  import("../diff/diff-merge-editor").then((m) => ({ default: m.DiffMergeEditor })),
);

const STATE_LABEL = {
  same: "stateSame",
  changed: "stateChanged",
  onlyLeft: "stateOnlyLeft",
  onlyRight: "stateOnlyRight",
  kindChanged: "stateKindChanged",
} as const satisfies Record<CompareState, keyof (typeof compareStrings)["en"]>;

const STATE_TONE: Record<CompareState, string> = {
  same: "text-muted-text",
  changed: "text-warn-text",
  onlyLeft: "text-bad-text",
  onlyRight: "text-ok-text",
  kindChanged: "text-info-text",
};

/**
 * Branch > Compare with branch… / Location > Compare with another folder… (doc 01 8.6). Lists one
 * folder at a time of the working copy's URL and another URL of the same repository, both at
 * HEAD, marks what differs (by last-changed revision and size, see `compareListings`), lets the
 * user open changed folders and shows the diff of a changed file. Everything is read from the
 * server; nothing is changed.
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

function CompareBody({ wc }: { wc: WorkingCopy }) {
  const t = useT(compareStrings);
  const urlId = useId();
  const [url, setUrl] = useState("");
  const [target, setTarget] = useState<string | null>(null);
  const [path, setPath] = useState("");
  const [showSame, setShowSame] = useState(false);
  const [diff, setDiff] = useState<CompareRow | null>(null);
  const valid = isCompareTarget(url, wc.repoRootUrl, wc.url);

  const atUrl = wc.branch?.url ?? null;
  const branches = useQuery({
    queryKey: queryKeys.branches(wc.repoRootUrl, atUrl),
    enabled: wc.branch !== null,
    queryFn: () => unwrap(ipc().commands.branchesList(wc.repoRootUrl, atUrl)),
  });
  const others = (branches.data ?? []).filter(
    (b) => b.url.replace(/\/+$/, "") !== wc.url.replace(/\/+$/, ""),
  );

  const listing = useQuery({
    queryKey: ["compare", wc.url, target, path],
    enabled: target !== null,
    queryFn: async () => {
      const [left, right] = await Promise.all([
        unwrap(ipc().commands.repoList(joinUrl(wc.url, path), null)),
        unwrap(ipc().commands.repoList(joinUrl(target as string, path), null)),
      ]);
      // Entries with unsafe names never reach the UI; only their count does.
      return {
        rows: compareListings(left.entries, right.entries),
        hidden: left.hidden + right.hidden,
      };
    },
  });

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!valid) return;
    setTarget(url.trim().replace(/\/+$/, ""));
    setPath("");
    setDiff(null);
  };
  const rows = (listing.data?.rows ?? []).filter((r) => showSame || r.state !== "same");
  const hidden = listing.data?.hidden ?? 0;
  const folderLabel = path ? `/${path}` : t("root");

  return (
    <div className="flex flex-col gap-3 p-4">
      <SafeText as="p" className="m-0 font-mono text-[12px] text-muted-text">
        {t("current", { name: wc.branch?.name ?? wc.url })}
      </SafeText>
      <form onSubmit={submit} className="flex flex-wrap items-end gap-2">
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
        <Button type="submit" variant="primary" disabled={!valid}>
          {t("compare")}
        </Button>
      </form>
      {url.length > 0 && !valid && (
        <span className="text-bad-text" role="alert">
          {t("otherUrlInvalid", { root: wc.repoRootUrl })}
        </span>
      )}
      <p id={`${urlId}-note`} className="m-0 text-[12px] text-muted-text">
        {t("note")}
      </p>

      {target === null ? (
        <p className="m-0 text-muted-text">{t("nothingYet")}</p>
      ) : (
        <>
          <div className="flex flex-wrap items-center gap-2 border-t border-line pt-3">
            <span className="text-muted-text">{t("folder")}</span>
            <SafeText className="min-w-0 flex-1 truncate font-mono text-[12px]">
              {folderLabel}
            </SafeText>
            <Button
              small
              disabled={path === ""}
              onClick={() => {
                setPath(path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "");
                setDiff(null);
              }}
            >
              <ArrowUp size={13} aria-hidden="true" />
              {t("up")}
            </Button>
            <label className="flex items-center gap-1.5 text-[12px]">
              <input
                type="checkbox"
                checked={showSame}
                onChange={(e) => setShowSame(e.target.checked)}
                className="accent-[var(--accent)]"
              />
              {t("showSame")}
            </label>
          </div>
          {listing.isPending ? (
            <p className="m-0 text-muted-text">{t("loading")}</p>
          ) : listing.isError ? (
            <p className="m-0 text-bad-text" role="alert">
              {toAppError(listing.error).message}
            </p>
          ) : rows.length === 0 ? (
            <p className="m-0 text-muted-text">{t("noDifferences")}</p>
          ) : (
            <table className="w-full border-collapse text-left text-[12px]">
              <caption className="sr-only">{t("rows", { folder: folderLabel })}</caption>
              <thead>
                <tr className="border-b border-line text-muted-text">
                  <th scope="col" className="px-2 py-1 font-medium">
                    {t("colName")}
                  </th>
                  <th scope="col" className="px-2 py-1 font-medium">
                    {t("colState")}
                  </th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <CompareRowView
                    key={r.name}
                    row={r}
                    selected={diff?.name === r.name}
                    onOpen={() => {
                      setPath(path ? `${path}/${r.name}` : r.name);
                      setDiff(null);
                    }}
                    onDiff={() => setDiff(r)}
                  />
                ))}
              </tbody>
            </table>
          )}
          {hidden > 0 && (
            <p className="m-0 text-[12px] text-muted-text">
              {t("hiddenEntries", { count: hidden })}
            </p>
          )}
          {diff && (
            <FileCompare
              leftUrl={
                diff.left ? joinUrl(wc.url, path ? `${path}/${diff.name}` : diff.name) : null
              }
              rightUrl={
                diff.right ? joinUrl(target, path ? `${path}/${diff.name}` : diff.name) : null
              }
              path={path ? `${path}/${diff.name}` : diff.name}
            />
          )}
        </>
      )}
    </div>
  );
}

function CompareRowView({
  row,
  selected,
  onOpen,
  onDiff,
}: {
  row: CompareRow;
  selected: boolean;
  onOpen: () => void;
  onDiff: () => void;
}) {
  const t = useT(compareStrings);
  const Icon = row.kind === "dir" ? Folder : File;
  // Folders on both sides can be opened; any file that differs has a diff (one side may be empty).
  const canOpen = row.kind === "dir" && row.left !== null && row.right !== null;
  const canDiff = row.kind === "file" && row.state !== "same" && row.state !== "kindChanged";
  const name = (
    <span className="inline-flex min-w-0 items-center gap-1.5">
      <Icon size={13} aria-hidden="true" className="shrink-0 text-muted-text" />
      <SafeText className="truncate font-mono">{row.name}</SafeText>
    </span>
  );
  return (
    <tr className={cn("border-b border-line", selected && "bg-sel")}>
      <td className="px-2 py-1">
        {canOpen || canDiff ? (
          <button
            type="button"
            onClick={canOpen ? onOpen : onDiff}
            aria-label={t(canOpen ? "open" : "showDiff", { name: row.name })}
            aria-pressed={canDiff ? selected : undefined}
            className="cursor-pointer border-0 bg-transparent p-0 text-left text-accent-text underline-offset-2 hover:underline focus-visible:outline-2 focus-visible:outline-accent"
          >
            {name}
          </button>
        ) : (
          name
        )}
      </td>
      <td className={cn("px-2 py-1", STATE_TONE[row.state])}>{t(STATE_LABEL[row.state])}</td>
    </tr>
  );
}

/** Text diff of one file between the two locations (a missing side shows as empty). */
function FileCompare({
  leftUrl,
  rightUrl,
  path,
}: {
  leftUrl: string | null;
  rightUrl: string | null;
  path: string;
}) {
  const t = useT(compareStrings);
  const q = useQuery({
    queryKey: ["compare-file", leftUrl, rightUrl],
    queryFn: async () => {
      const cat = (u: string | null) =>
        u ? unwrap(ipc().commands.repoCat(u, null)).then((s) => s.content) : Promise.resolve("");
      const [left, right] = await Promise.all([cat(leftUrl), cat(rightUrl)]);
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
        <span>{t("diffLeft")}</span>
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
