import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { Button } from "../../../components/ui/button";
import { SafeText } from "../../../components/ui/safe-text";
import { Sheet } from "../../../components/ui/sheet";
import { useToast } from "../../../components/ui/toast";
import type { FileStatus, ResolveChoice } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { dialogStrings } from "../dialogs/dialog-strings";
import { useWcActions } from "../use-wc-actions";
import { isConflicted } from "../wc-model";
import { useWcSession } from "../wc-session";

const PREVIEW_MAX_LINES = 400;

type Choice = { id: ResolveChoice; label: string; hint: string };

/**
 * Conflict resolver (doc 01 section 5). Text conflicts: take mine, theirs, base, or keep the merged
 * file on disk. Tree conflicts: an explanation plus the two outcomes the engine supports (for an
 * item moved away: apply the incoming changes to the destination, or keep it). Binary
 * files only offer mine / theirs. Content is shown as text, never as markup.
 */
export function ConflictResolver({
  initialPath,
  onClose,
}: {
  initialPath: string | null;
  onClose: () => void;
}) {
  const d = useT(dialogStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const actions = useWcActions();
  const { wc, files } = useWcSession();
  const conflicts = useMemo(() => files.filter(isConflicted), [files]);
  const [path, setPath] = useState<string | null>(initialPath);
  const current = conflicts.find((f) => f.path === path) ?? conflicts[0] ?? null;
  const index = current ? conflicts.indexOf(current) : -1;
  const [choice, setChoice] = useState<ResolveChoice | null>(null);

  // A new file starts without a pre-selected choice: resolving is a deliberate act.
  const currentPath = current?.path;
  // biome-ignore lint/correctness/useExhaustiveDependencies: reset the choice only when the file changes
  useEffect(() => setChoice(null), [currentPath]);

  const resolve = useMutation({
    mutationFn: () => {
      if (!wc || !current || !choice) throw new Error("nothing to resolve");
      return unwrap(ipc().commands.resolve({ wcId: wc.id, path: current.path, choice }));
    },
    onSuccess: () => {
      if (current) toast.show(d("resolved", { path: current.path }));
      actions.refreshStatus();
    },
  });

  const go = (delta: number) => {
    const next = conflicts[(index + delta + conflicts.length) % conflicts.length];
    if (next) setPath(next.path);
  };

  return (
    <Sheet
      open
      onClose={onClose}
      title={d("conflictsTitle", { count: conflicts.length })}
      footer={
        <>
          {conflicts.length > 1 && (
            <>
              <Button onClick={() => go(-1)}>{d("previous")}</Button>
              <Button onClick={() => go(1)}>{d("next")}</Button>
            </>
          )}
          <span className="flex-1" />
          <Button onClick={onClose}>{c("close")}</Button>
          <Button
            variant="primary"
            disabled={!current || !choice || resolve.isPending}
            onClick={() => resolve.mutate()}
          >
            {d("resolveAction")}
          </Button>
        </>
      }
    >
      <div className="grid h-full min-h-0 grid-cols-[260px_1fr]">
        <ul
          aria-label={d("conflictFiles")}
          className="m-0 min-h-0 list-none overflow-auto border-r border-line p-0"
        >
          {conflicts.map((f) => (
            <li key={f.path}>
              <button
                type="button"
                aria-current={f.path === current?.path}
                onClick={() => setPath(f.path)}
                className={`block w-full cursor-pointer truncate border-0 px-3 py-1.5 text-left font-mono text-[12px] ${
                  f.path === current?.path ? "bg-sel" : "bg-transparent hover:bg-surface-2"
                }`}
                title={f.path}
              >
                <SafeText>{f.path}</SafeText>
              </button>
            </li>
          ))}
        </ul>
        <div className="min-h-0 overflow-auto p-4">
          {current ? (
            <ConflictDetail
              key={current.path}
              file={current}
              choice={choice}
              onChoice={setChoice}
              onOpenEditor={() => void actions.openWith(current.path, "editor")}
              error={resolve.error ? toAppError(resolve.error).message : null}
            />
          ) : (
            <p className="m-0 text-muted-text">{d("conflictsNone")}</p>
          )}
        </div>
      </div>
    </Sheet>
  );
}

function ConflictDetail({
  file,
  choice,
  onChoice,
  onOpenEditor,
  error,
}: {
  file: FileStatus;
  choice: ResolveChoice | null;
  onChoice: (c: ResolveChoice) => void;
  onOpenEditor: () => void;
  error: string | null;
}) {
  const d = useT(dialogStrings);
  const tree = file.treeConflict;
  const kind = tree
    ? d("conflictTree")
    : file.text === "conflicted"
      ? d("conflictText")
      : d("conflictProps");

  // A tree conflict on an item moved away (its destination is known): svn applies the
  // incoming changes to the destination, or keeps it and breaks the move.
  const movedTo = tree ? file.movedTo : null;
  const choices: Choice[] = movedTo
    ? [
        {
          id: "mineConflict",
          label: d("treeMovedApply", { path: movedTo }),
          hint: d("treeMovedApplyHint"),
        },
        {
          id: "working",
          label: d("treeMovedKeep", { path: movedTo }),
          hint: d("treeMovedKeepHint"),
        },
      ]
    : tree
      ? [
          { id: "working", label: d("treeKeepLocal"), hint: d("treeKeepLocalHint") },
          { id: "theirs", label: d("treeTakeIncoming"), hint: d("treeTakeIncomingHint") },
        ]
      : [
          { id: "mine", label: d("choiceMine"), hint: d("choiceMineHint") },
          { id: "theirs", label: d("choiceTheirs"), hint: d("choiceTheirsHint") },
          ...(file.binary
            ? []
            : [
                { id: "base" as const, label: d("choiceBase"), hint: d("choiceBaseHint") },
                { id: "working" as const, label: d("choiceMerged"), hint: d("choiceMergedHint") },
              ]),
        ];

  return (
    <div className="flex flex-col gap-3">
      <div>
        <SafeText as="h3" className="m-0 break-all font-mono text-[13px]">
          {file.path}
        </SafeText>
        <span className="text-muted-text">{kind}</span>
      </div>
      {tree && (
        <div className="rounded-md bg-warn-soft px-3 py-2">
          <p className="m-0">{d("treeExplain")}</p>
          <p className="m-0">
            {movedTo
              ? d("treeLocalMoved", { path: movedTo })
              : file.text === "missing" || file.text === "deleted"
                ? d("treeLocalDeleted")
                : d("treeLocalMissing")}
          </p>
        </div>
      )}
      <fieldset className="m-0 flex flex-col gap-2 border-0 p-0">
        <legend className="sr-only">{d("resolveAction")}</legend>
        {choices.map((o) => (
          <label key={o.id} className="flex items-start gap-2">
            <input
              type="radio"
              name="resolve-choice"
              checked={choice === o.id}
              onChange={() => onChoice(o.id)}
              className="mt-1 accent-[var(--accent)]"
            />
            <span>
              <b>{o.label}</b>
              <span className="block text-muted-text">{o.hint}</span>
            </span>
          </label>
        ))}
      </fieldset>
      {!tree && !file.binary && (
        <div>
          <Button small onClick={onOpenEditor}>
            {d("openExternalEditor")}
          </Button>
        </div>
      )}
      {!tree && !file.binary && <MergedPreview path={file.path} />}
      {error && (
        <p className="m-0 text-bad-text" role="alert">
          <SafeText>{error}</SafeText>
        </p>
      )}
    </div>
  );
}

/** The file as it is on disk (with conflict markers), capped so a huge file cannot freeze the UI. */
function MergedPreview({ path }: { path: string }) {
  const d = useT(dialogStrings);
  const { wc } = useWcSession();
  const q = useQuery({
    queryKey: [...queryKeys.status(wc?.id ?? ""), "conflict-preview", path],
    enabled: wc !== null,
    queryFn: () =>
      unwrap(
        ipc().commands.diffGet({
          target: { kind: "wc", wcId: (wc as NonNullable<typeof wc>).id, path },
          ignoreWhitespace: false,
          forceLarge: false,
        }),
      ),
  });
  const text = q.data?.right?.content ?? null;
  const shown = useMemo(() => {
    if (text === null) return null;
    const lines = text.split("\n");
    return lines.length > PREVIEW_MAX_LINES
      ? `${lines.slice(0, PREVIEW_MAX_LINES).join("\n")}\n…`
      : text;
  }, [text]);

  return (
    <section aria-label={d("mergedPreview")}>
      <b className="text-[12px]">{d("mergedPreview")}</b>
      {q.isPending ? (
        <p className="m-0 text-muted-text">{d("previewLoading")}</p>
      ) : shown === null ? null : (
        <pre className="m-0 mt-1 max-h-72 overflow-auto rounded-md border border-line bg-surface-2 p-2 font-mono text-[12px] leading-[1.6]">
          <SafeText>{shown}</SafeText>
        </pre>
      )}
    </section>
  );
}
