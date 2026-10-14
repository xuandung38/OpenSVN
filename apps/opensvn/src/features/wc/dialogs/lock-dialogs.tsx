import { useEffect, useState } from "react";
import { Button } from "../../../components/ui/button";
import { BadBox, WarnBox } from "../../../components/ui/dialog";
import { Input, Textarea } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { AppError, FileStatus, LockConflict } from "../../../ipc/bindings";
import { IpcError, ipc, toAppError, unwrap } from "../../../ipc/client";
import { useOperationOutcome } from "../../../ipc/queries";
import { relativeTime } from "../../../lib/format";
import { useT } from "../../../lib/i18n/i18n";
import { useWcActions } from "../use-wc-actions";
import { useWcSession } from "../wc-session";
import { ActionDialog, PathList } from "./action-dialog";
import { dialogStrings } from "./dialog-strings";

/**
 * Lock to edit / unlock files of the working copy (doc 01 4.7, flow 10.4). The dialog stays open
 * until the operation ends: the server's refusal arrives with the operation result, and a lock
 * held by someone else is then shown with its owner, comment and date (`lockConflicts`). Stealing
 * (or breaking) is never done silently: it needs the steal box and an explicit confirmation.
 */
export function WcLockDialog({
  mode,
  paths,
  conflicts: initialConflicts = [],
  onClose,
}: {
  mode: "lock" | "unlock";
  paths: string[];
  /** Locks already known to refuse this request (a failed lock reopened from the activity). */
  conflicts?: readonly LockConflict[];
  onClose: () => void;
}) {
  const d = useT(dialogStrings);
  const toast = useToast();
  const { wc, files } = useWcSession();
  const targets = files.filter((f) => paths.includes(f.path));
  const mine = targets.filter((f) => f.lock?.mine);
  const [comment, setComment] = useState("");
  const [conflicts, setConflicts] = useState<readonly LockConflict[]>(initialConflicts);
  const [serverSaysLocked, setServerSaysLocked] = useState(initialConflicts.length > 0);
  const [steal, setSteal] = useState(false);
  const [confirmed, setConfirmed] = useState(false);
  const [opId, setOpId] = useState<string | undefined>();
  const [opError, setOpError] = useState<AppError | null>(null);
  const outcome = useOperationOutcome(opId);
  const isLock = mode === "lock";

  // Locks of others: what the server just said, else what the status knows.
  const held = heldElsewhere(targets, conflicts);
  const showSteal = held.length > 0 || serverSaysLocked;
  // Every owner the steal would take a lock from, named in the steal box and its warning.
  const owner = [...new Set(held.map((c) => c.owner))].join(", ");
  const alreadyMine = isLock && mine.length === targets.length && targets.length > 0;
  const running = opId !== undefined;
  // Stealing/breaking needs the tick; plain lock/unlock does not.
  const needsConfirm = steal;
  const canSubmit = wc !== null && !running && !alreadyMine && (!needsConfirm || confirmed);

  // React once to the end of the lock/unlock operation.
  useEffect(() => {
    if (!opId || !outcome) return;
    setOpId(undefined);
    if (outcome.status === "ok") {
      toast.show(d(isLock ? "lockDone" : "unlockDone", { count: paths.length }));
      onClose();
      return;
    }
    if (outcome.error.code === "cancelled") return;
    setOpError(outcome.error);
    if (refusedByLock(outcome.error, isLock)) {
      setServerSaysLocked(true);
      setConflicts(outcome.error.lockConflicts ?? []);
    }
  }, [opId, outcome, onClose, toast, d, isLock, paths.length]);

  return (
    <ActionDialog
      title={isLock ? d("lockTitle") : d("unlockTitle")}
      submitLabel={
        running
          ? d(isLock ? "lockRunning" : "unlockRunning")
          : steal
            ? d("stealAction")
            : isLock
              ? d("lockAction")
              : d("unlockAction")
      }
      danger={steal}
      canSubmit={canSubmit}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc) return;
        setOpError(null);
        const t0 = { kind: "wc" as const, wcId: wc.id, paths };
        try {
          const h = isLock
            ? await unwrap(
                ipc().commands.lock({ targets: t0, comment: comment.trim() || null, steal }),
              )
            : await unwrap(ipc().commands.unlock({ targets: t0, breakLock: steal }));
          setOpId(h.opId);
        } catch (e) {
          if (e instanceof IpcError && refusedByLock(e.appError, isLock)) {
            setServerSaysLocked(true);
            setConflicts(e.appError.lockConflicts ?? []);
          }
          throw e;
        }
      }}
    >
      <PathList paths={paths} label={d("files")} />
      {alreadyMine && <WarnBox>{d("alreadyYours")}</WarnBox>}
      {held.map((c) => (
        <WarnBox key={c.path}>
          <SafeText as="span" className="block">
            {c.createdAt
              ? d("lockedBySince", {
                  path: c.path,
                  owner: c.owner,
                  when: relativeTime(c.createdAt),
                })
              : `${c.path}: ${d("lockedBy", { owner: c.owner })}`}
          </SafeText>
          {c.comment && (
            <SafeText as="span" className="block">
              {d("lockConflictComment", { comment: c.comment })}
            </SafeText>
          )}
        </WarnBox>
      ))}
      {held.length > 0 && isLock && (
        <SafeText as="p" className="m-0 text-muted-text">
          {d("contactOwner", { owner })}
        </SafeText>
      )}
      {opError && (
        <SafeText as="p" className="m-0 break-words text-bad-text">
          <span role="alert">{opError.message}</span>
        </SafeText>
      )}
      {isLock && !alreadyMine && (
        <label className="flex flex-col gap-1">
          {d("lockComment")}
          <Textarea rows={2} value={comment} onChange={(e) => setComment(e.target.value)} />
        </label>
      )}
      {showSteal && (
        <div className="flex flex-col gap-2">
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={steal}
              disabled={running}
              onChange={(e) => {
                setSteal(e.target.checked);
                setConfirmed(false);
              }}
              className="accent-[var(--accent)]"
            />
            {isLock ? d("stealLock", { owner: owner || "…" }) : d("breakLock")}
          </label>
          {steal && (
            <>
              <BadBox>{d("stealWarning", { owner: owner || "…" })}</BadBox>
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={confirmed}
                  disabled={running}
                  onChange={(e) => setConfirmed(e.target.checked)}
                  className="accent-[var(--accent)]"
                />
                {d("stealConfirm")}
              </label>
            </>
          )}
        </div>
      )}
    </ActionDialog>
  );
}

/** Whether `e` refused the request because of someone else's lock (unlock: no own token). */
function refusedByLock(e: AppError, isLock: boolean): boolean {
  return e.code === "locked-by-other" || (!isLock && e.code === "lock-token-invalid");
}

/**
 * Locks held by someone else on the targets: the server's answer per path wins over the status
 * (which only knows a foreign lock after a remote check).
 */
function heldElsewhere(
  targets: readonly FileStatus[],
  conflicts: readonly LockConflict[],
): LockConflict[] {
  const out = [...conflicts];
  for (const f of targets) {
    if (f.lock && !f.lock.mine && !out.some((c) => c.path === f.path)) {
      out.push({
        path: f.path,
        owner: f.lock.owner,
        comment: f.lock.comment,
        createdAt: f.lock.createdAt,
      });
    }
  }
  return out;
}

/** Put files into a named changelist, or remove them from theirs (`changelist_set`). */
export function ChangelistDialog({ paths, onClose }: { paths: string[]; onClose: () => void }) {
  const d = useT(dialogStrings);
  const { wc, files } = useWcSession();
  const actions = useWcActions();
  const [name, setName] = useState("");
  const [removeError, setRemoveError] = useState<string | null>(null);
  const existing = [
    ...new Set(files.map((f) => f.changelist).filter((n): n is string => n !== null)),
  ];
  const trimmed = name.trim();

  const apply = async (value: string | null) => {
    if (!wc) return;
    await unwrap(ipc().commands.changelistSet(wc.id, paths, value));
    actions.refreshStatus();
    onClose();
  };

  return (
    <ActionDialog
      title={d("changelistTitle")}
      submitLabel={d("changelistAction")}
      canSubmit={wc !== null && trimmed.length > 0}
      onClose={onClose}
      onSubmit={() => apply(trimmed)}
    >
      <PathList paths={paths} label={d("files")} />
      <label className="flex flex-col gap-1">
        {d("changelistName")}
        <Input
          list="wc-changelists"
          value={name}
          onChange={(e) => setName(e.target.value)}
          spellCheck={false}
        />
        <datalist id="wc-changelists" aria-label={d("changelistExisting")}>
          {existing.map((n) => (
            <option key={n} value={n} />
          ))}
        </datalist>
      </label>
      <div>
        <Button
          small
          onClick={() =>
            void apply(null).catch((e: unknown) => setRemoveError(toAppError(e).message))
          }
        >
          {d("changelistRemove")}
        </Button>
        {removeError && (
          <span className="ml-2 text-bad-text" role="alert">
            {removeError}
          </span>
        )}
      </div>
    </ActionDialog>
  );
}
