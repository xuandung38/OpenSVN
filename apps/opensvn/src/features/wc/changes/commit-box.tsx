import { useMutation } from "@tanstack/react-query";
import { type KeyboardEvent, useEffect, useMemo, useRef, useState } from "react";
import { Button } from "../../../components/ui/button";
import { DropdownMenu } from "../../../components/ui/dropdown-menu";
import { Input, Textarea } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { AppError } from "../../../ipc/bindings";
import { IpcError, ipc, unwrap } from "../../../ipc/client";
import { useOperationOutcome } from "../../../ipc/queries";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { matchShortcut } from "../shortcuts";
import { useWcActions } from "../use-wc-actions";
import { useViewPrefs } from "../view-prefs";
import {
  checkCommit,
  isTickable,
  joinMessage,
  listedFiles,
  loadMessageHistory,
  pushMessageHistory,
  SUMMARY_SOFT_LIMIT,
  splitMessage,
} from "../wc-model";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";

/**
 * Commit box (doc 01 3.3): summary + description, recent messages (menu, or Up/Down in the summary),
 * keep-locks option, needs-lock / locked-by-other warnings, conflict block and the out-of-date
 * recovery path. `Mod+Enter` commits from either field.
 */
export function CommitBox() {
  const { wc, files, checked, setChecked } = useWcSession();
  const t = useT(wcStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const actions = useWcActions();
  const [summary, setSummary] = useState("");
  const [description, setDescription] = useState("");
  const [keepLocks, setKeepLocks] = useState(false);
  const [tagOk, setTagOk] = useState(false);
  const [history, setHistory] = useState<string[]>(() => loadMessageHistory());
  const histIdx = useRef(-1);
  const [opId, setOpId] = useState<string | undefined>();
  const [error, setError] = useState<AppError | null>(null);
  const handled = useRef<string | null>(null);
  /** Message of the commit in flight (the fields stay editable while it runs). */
  const sent = useRef("");
  const outcome = useOperationOutcome(opId);

  const prefs = useViewPrefs();
  // Only rows the list shows can be committed: a ticked unversioned file that View > Show
  // unversioned files then hid is not added behind the user's back.
  const selected = useMemo(
    () => listedFiles(files, prefs).filter((f) => isTickable(f) && checked.has(f.path)),
    [files, checked, prefs],
  );
  const check = useMemo(() => checkCommit(selected), [selected]);
  const onTag = wc?.branch?.kind === "tag";
  const where = wc?.branch?.name ?? wc?.pathInRepo ?? "";
  const message = joinMessage(summary, description);

  const commit = useMutation({
    mutationFn: () => {
      if (!wc) throw new Error("no working copy");
      return unwrap(
        ipc().commands.commit({
          wcId: wc.id,
          paths: selected.map((f) => f.path),
          message,
          keepLocks,
          autoAddRemove: true,
        }),
      );
    },
    onSuccess: (h) => {
      sent.current = message;
      setError(null);
      setOpId(h.opId);
    },
    onError: (e) => setError(e instanceof IpcError ? e.appError : null),
  });

  const running = commit.isPending || (opId !== undefined && outcome === undefined);
  const blocked = check.conflicts.length > 0;
  const canCommit =
    wc !== null &&
    !running &&
    !blocked &&
    selected.length > 0 &&
    summary.trim().length > 0 &&
    (!onTag || tagOk);

  // React once to the final result of the commit operation.
  useEffect(() => {
    if (!opId || !outcome || handled.current === opId) return;
    handled.current = opId;
    if (outcome.status === "ok") {
      const rev = outcome.result.kind === "newRev" ? outcome.result.newRev : null;
      toast.show(rev !== null ? t("committed", { rev }) : t("commitDone"));
      setHistory(pushMessageHistory(sent.current));
      setSummary("");
      setDescription("");
      setKeepLocks(false);
      setTagOk(false);
      setChecked(new Set());
      histIdx.current = -1;
      setError(null);
    } else {
      setError(outcome.error);
    }
    setOpId(undefined);
  }, [opId, outcome, setChecked, t, toast]);

  const submit = () => {
    if (canCommit) commit.mutate();
  };
  const onKeyDown = (e: KeyboardEvent) => {
    if (matchShortcut(e.nativeEvent, "commit")) {
      e.preventDefault();
      submit();
    }
  };
  const pick = (m: string) => {
    const s = splitMessage(m);
    setSummary(s.summary);
    setDescription(s.description);
  };
  const onSummaryKey = (e: KeyboardEvent<HTMLInputElement>) => {
    onKeyDown(e);
    if (e.key === "ArrowUp" && (summary === "" || histIdx.current >= 0)) {
      const next = Math.min(history.length - 1, histIdx.current + 1);
      if (next >= 0 && history[next] !== undefined) {
        histIdx.current = next;
        pick(history[next] as string);
        e.preventDefault();
      }
    } else if (e.key === "ArrowDown" && histIdx.current >= 0) {
      histIdx.current -= 1;
      const m = history[histIdx.current];
      if (m !== undefined) pick(m);
      else {
        setSummary("");
        setDescription("");
      }
      e.preventDefault();
    }
  };

  const label =
    onTag && !tagOk
      ? t("commitToTag")
      : selected.length === 1
        ? t("commitButtonOne", { where })
        : t("commitButton", { count: selected.length, where });
  const outOfDate = error?.code === "out-of-date" || error?.actions?.includes("update") === true;
  // The server confirmed the commit left nothing behind: the same commit can simply be sent again.
  const notApplied = error?.code === "commit-not-applied";

  return (
    <div className="flex flex-col gap-2 border-t border-line bg-surface-2 p-2.5">
      <div className="flex gap-1.5">
        <Input
          aria-label={t("summary")}
          placeholder={t("summary")}
          value={summary}
          aria-invalid={summary.length > SUMMARY_SOFT_LIMIT}
          onChange={(e) => {
            histIdx.current = -1;
            setSummary(e.target.value);
          }}
          onKeyDown={onSummaryKey}
        />
        <DropdownMenu
          trigger={
            <Button aria-label={t("recentMessages")} title={t("recentMessages")}>
              ▾
            </Button>
          }
          entries={
            history.length === 0
              ? [{ label: t("noRecentMessages"), disabled: true }]
              : history.slice(0, 10).map((m, i) => ({
                  label: `${i + 1}. ${m.split("\n")[0]?.slice(0, 60) ?? ""}`,
                  onSelect: () => pick(m),
                }))
          }
        />
      </div>
      {summary.length > SUMMARY_SOFT_LIMIT && (
        <span className="text-[12px] text-warn-text" role="status">
          {t("longSummary", { max: SUMMARY_SOFT_LIMIT })}
        </span>
      )}
      <Textarea
        aria-label={t("description")}
        placeholder={t("description")}
        rows={3}
        value={description}
        onChange={(e) => setDescription(e.target.value)}
        onKeyDown={onKeyDown}
      />
      <label className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={keepLocks}
          onChange={(e) => setKeepLocks(e.target.checked)}
          className="accent-[var(--accent)]"
        />
        {t("keepLocks")}
      </label>
      {onTag && (
        <label className="flex items-start gap-2 text-warn-text">
          <input
            type="checkbox"
            checked={tagOk}
            onChange={(e) => setTagOk(e.target.checked)}
            className="mt-0.5 accent-[var(--accent)]"
          />
          {t("tagWarning")}
        </label>
      )}
      <ul className="m-0 flex list-none flex-col gap-0.5 p-0 text-[12px]" aria-live="polite">
        {check.willAdd > 0 && (
          <li className="text-muted-text">{t("willAdd", { count: check.willAdd })}</li>
        )}
        {blocked && (
          <li className="text-bad-text">
            {t("blockedConflicts", { count: check.conflicts.length })}
          </li>
        )}
        {check.needsLock.length > 0 && (
          <li className="text-warn-text">
            {t("warnNeedsLock", { count: check.needsLock.length })}
          </li>
        )}
        {check.lockedByOther.length > 0 && (
          <li className="text-warn-text">
            {t("warnLockedOther", { count: check.lockedByOther.length })}
          </li>
        )}
      </ul>
      {error && (
        <div role="alert" className="rounded-md bg-bad-soft px-2.5 py-2">
          <b className="text-bad-text">{t("commitFailed")}</b>
          <SafeText as="p" className="m-0 break-words">
            {outOfDate ? t("outOfDate") : notApplied ? t("nothingCommitted") : error.message}
          </SafeText>
          {outOfDate && (
            <Button small className="mt-1" onClick={() => void actions.update(null)}>
              {t("updateNow")}
            </Button>
          )}
          {notApplied && (
            <Button small className="mt-1" disabled={!canCommit} onClick={submit}>
              {c("retry")}
            </Button>
          )}
        </div>
      )}
      <Button variant="primary" data-wc-commit="" disabled={!canCommit} onClick={submit}>
        {running ? t("committing") : label}
      </Button>
    </div>
  );
}
