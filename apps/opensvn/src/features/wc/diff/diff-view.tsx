import { useQuery } from "@tanstack/react-query";
import { MoreHorizontal } from "lucide-react";
import { lazy, Suspense, useState } from "react";
import { Button } from "../../../components/ui/button";
import { DropdownMenu } from "../../../components/ui/dropdown-menu";
import { EmptyState } from "../../../components/ui/empty-state";
import { SafeText } from "../../../components/ui/safe-text";
import { SegmentedControl } from "../../../components/ui/segmented-control";
import type { DiffTarget, FileDiff, FileStatus } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { useSettings } from "../../../ipc/queries";
import { queryKeys } from "../../../ipc/query-keys";
import { formatBytes } from "../../../lib/format";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { fileMenuEntries } from "../changes/file-menu";
import { dialogStrings } from "../dialogs/dialog-strings";
import { useUiCommand } from "../ui-commands";
import { useWcActions } from "../use-wc-actions";
import { displayPath } from "../wc-model";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";
import { ImageSides, PropChanges } from "./diff-parts";
import { OfficeNote } from "./office-note";

// CodeMirror is heavy: load it only when a text diff is actually shown.
const DiffMergeEditor = lazy(() =>
  import("./diff-merge-editor").then((m) => ({ default: m.DiffMergeEditor })),
);

/** Diff pane of the Changes tab for the selected file (doc 01 3.3). */
export function DiffView() {
  const { wc, selected, files } = useWcSession();
  const d = useT(dialogStrings);
  const file = files.find((f) => f.path === selected) ?? null;
  if (!wc || !selected) {
    return <EmptyState title={d("diffEmpty")} description={d("diffEmptyHint")} />;
  }
  // Keyed by file so "load full diff" and scroll state never leak between files.
  return <FileDiffPane key={`${wc.id}:${selected}`} wcId={wc.id} path={selected} file={file} />;
}

function FileDiffPane({
  wcId,
  path,
  file,
}: {
  wcId: string;
  path: string;
  file: FileStatus | null;
}) {
  const d = useT(dialogStrings);
  const t = useT(wcStrings);
  const c = useT(commonStrings);
  const session = useWcSession();
  const actions = useWcActions();
  const settings = useSettings();
  const [mode, setMode] = useState<"split" | "unified" | null>(null);
  const [ignoreWs, setIgnoreWs] = useState<boolean | null>(null);
  const [force, setForce] = useState(false);
  const effMode = mode ?? settings.data?.["diff.mode"] ?? "split";
  const effWs = ignoreWs ?? settings.data?.["diff.ignoreWhitespace"] ?? false;

  // Mod+Alt+D / Mod+Alt+W and View > Diff mode / Hide whitespace (doc 01 8.4); routed by
  // `useWcShortcuts`, which drops the key/menu echo so one press toggles once.
  useUiCommand("toggleDiffMode", () => setMode(effMode === "split" ? "unified" : "split"));
  useUiCommand("toggleWhitespace", () => setIgnoreWs(!effWs));

  const q = useQuery({
    // Under the status key so status events refresh the open diff too.
    queryKey: [...queryKeys.status(wcId), "diff", path, effWs, force],
    queryFn: () =>
      unwrap(
        ipc().commands.diffGet({
          target: { kind: "wc", wcId, path },
          ignoreWhitespace: effWs,
          forceLarge: force,
        }),
      ),
  });

  const menu = file ? fileMenuEntries([file], session, actions, t) : [];
  return (
    <section aria-label={d("diffPane", { path })} className="flex h-full min-h-0 flex-col">
      <header className="flex items-center gap-2 border-b border-line px-3 py-1.5">
        <SafeText className="min-w-0 flex-1 truncate font-mono text-[12px] font-semibold">
          {displayPath(path, t("wcRootRow", { name: session.wc?.name ?? "" }))}
        </SafeText>
        <label className="flex items-center gap-1.5 text-[12px]">
          <input
            type="checkbox"
            checked={effWs}
            onChange={(e) => setIgnoreWs(e.target.checked)}
            className="accent-[var(--accent)]"
          />
          {d("hideWhitespace")}
        </label>
        <SegmentedControl<"split" | "unified">
          label={d("diffMode")}
          value={effMode}
          onChange={setMode}
          options={[
            { value: "split", label: d("split") },
            { value: "unified", label: d("unified") },
          ]}
        />
        {menu.length > 0 && (
          <DropdownMenu
            trigger={
              <Button small aria-label={d("fileActions")} title={d("fileActions")}>
                <MoreHorizontal size={14} aria-hidden="true" />
              </Button>
            }
            entries={menu}
          />
        )}
      </header>
      <div className="min-h-0 flex-1 overflow-auto">
        {q.isPending ? (
          <p className="m-0 p-4 text-muted-text">{d("diffLoading")}</p>
        ) : q.isError ? (
          <EmptyState
            title={d("diffError")}
            description={toAppError(q.error).message}
            actions={<Button onClick={() => void q.refetch()}>{c("retry")}</Button>}
          />
        ) : (
          <DiffBody
            diff={q.data}
            file={file}
            mode={effMode}
            path={path}
            onLoadFull={() => setForce(true)}
            onOpenExternal={() => void actions.openExternalDiff(path)}
            onOpenDefault={() => void actions.openWith(path, "default")}
            target={{ kind: "wc", wcId, path }}
          />
        )}
      </div>
    </section>
  );
}

/**
 * Body of the diff pane. Exported for tests.
 *
 * `tooLarge` without text sides always shows the large-file banner (Load full / external tool):
 * the engine may send the changed hunks or nothing at all (doc 03 `FileDiff.hunks`), and either
 * way a large modified file must never read as "no content change".
 */
export function DiffBody({
  diff,
  file,
  mode,
  path,
  onLoadFull,
  onOpenExternal,
  onOpenDefault,
  target,
}: {
  diff: FileDiff;
  file: FileStatus | null;
  mode: "split" | "unified";
  path: string;
  onLoadFull: () => void;
  onOpenExternal: () => void;
  onOpenDefault: () => void;
  /** What was diffed; lets an Office diff open each version in its default app. */
  target?: DiffTarget;
}) {
  const d = useT(dialogStrings);
  const hasText = diff.left !== null || diff.right !== null;

  let content: React.ReactNode;
  if (diff.binary) {
    content = diff.image ? (
      <ImageSides image={diff.image} before={d("imageBefore")} after={d("imageAfter")} />
    ) : (
      <EmptyState
        title={d("binaryTitle")}
        description={[
          d("binaryHint"),
          diff.mime ? d("mime", { mime: diff.mime }) : "",
          file?.size != null ? d("size", { size: formatBytes(file.size) }) : "",
        ]
          .filter(Boolean)
          .join(" ")}
        actions={<Button onClick={onOpenDefault}>{d("openDefault")}</Button>}
      />
    );
  } else if (diff.tooLarge && !hasText) {
    const hunks = diff.hunks && diff.hunks.length > 0 ? diff.hunks : null;
    content = (
      <div>
        <div
          role="status"
          className="flex items-center gap-2 border-b border-line bg-warn-soft px-3 py-2"
        >
          <b className="text-warn-text">{d("tooLarge")}</b>
          <span className="flex-1">{d(hunks ? "tooLargeHint" : "tooLargeNoHunksHint")}</span>
          <Button small onClick={onOpenExternal}>
            {d("openExternalTool")}
          </Button>
          <Button small onClick={onLoadFull}>
            {d("loadFull")}
          </Button>
        </div>
        {hunks && <HunkLines hunks={hunks} />}
      </div>
    );
  } else if (diff.tooComplex) {
    // The backend gave up at its display budget and sent no sides; never retry, hand off instead.
    content = (
      <EmptyState
        title={d("tooComplex")}
        description={d("tooComplexHint")}
        actions={<Button onClick={onOpenExternal}>{d("openExternalTool")}</Button>}
      />
    );
  } else if (hasText) {
    content = (
      <div className="h-full min-h-[200px]">
        <Suspense fallback={<p className="m-0 p-4 text-muted-text">{d("diffLoading")}</p>}>
          <DiffMergeEditor
            left={diff.left?.content ?? ""}
            right={diff.right?.content ?? ""}
            mode={mode}
            label={d("diffPane", { path })}
          />
        </Suspense>
      </div>
    );
  } else if (diff.propChanges.length === 0) {
    content = <p className="m-0 p-4 text-muted-text">{d("noContentChange")}</p>;
  }

  return (
    <div className="flex h-full flex-col">
      {diff.office && <OfficeNote office={diff.office} diff={diff} target={target} />}
      <div className="min-h-0 flex-1">{content}</div>
      <PropChanges changes={diff.propChanges} />
    </div>
  );
}

/** Hunks of a file too large to load in full, as plain text lines. */
function HunkLines({ hunks }: { hunks: NonNullable<FileDiff["hunks"]> }) {
  return (
    <pre className="m-0 overflow-auto p-0 font-mono text-[12px] leading-[1.6]">
      {hunks.map((h) => (
        <div key={`${h.oldStart}:${h.newStart}`}>
          <div className="bg-surface-2 px-3 text-muted-text">
            {`@@ -${h.oldStart},${h.oldLines} +${h.newStart},${h.newLines} @@`}
          </div>
          {h.lines.map((line, i) => (
            <div
              // biome-ignore lint/suspicious/noArrayIndexKey: static list of text lines
              key={i}
              className={`px-3 ${line.startsWith("+") ? "bg-add-bg" : line.startsWith("-") ? "bg-del-bg" : ""}`}
            >
              <SafeText>{line}</SafeText>
            </div>
          ))}
        </div>
      ))}
    </pre>
  );
}
