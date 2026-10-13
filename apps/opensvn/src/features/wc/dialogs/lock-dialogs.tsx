import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { BadBox, WarnBox } from "../../../components/ui/dialog";
import { Input, Textarea } from "../../../components/ui/fields";
import { useToast } from "../../../components/ui/toast";
import { IpcError, ipc, toAppError, unwrap } from "../../../ipc/client";
import { useT } from "../../../lib/i18n/i18n";
import { useWcActions } from "../use-wc-actions";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";
import { ActionDialog, PathList } from "./action-dialog";
import { dialogStrings } from "./dialog-strings";

/**
 * Lock to edit / unlock files of the working copy (doc 01 4.7, flow 10.4). A lock held by someone
 * else can only be stolen (or broken) after an explicit confirmation; a `locked-by-other` answer
 * from the server reveals that option too.
 */
export function WcLockDialog({
  mode,
  paths,
  onClose,
}: {
  mode: "lock" | "unlock";
  paths: string[];
  onClose: () => void;
}) {
  const d = useT(dialogStrings);
  const t = useT(wcStrings);
  const toast = useToast();
  const { wc, files } = useWcSession();
  const targets = files.filter((f) => paths.includes(f.path));
  const others = targets.filter((f) => f.lock && !f.lock.mine);
  const mine = targets.filter((f) => f.lock?.mine);
  const [comment, setComment] = useState("");
  const [serverSaysLocked, setServerSaysLocked] = useState(false);
  const [steal, setSteal] = useState(false);
  const [confirmed, setConfirmed] = useState(false);

  const showSteal = others.length > 0 || serverSaysLocked;
  const owner = others[0]?.lock?.owner ?? "";
  const alreadyMine = mode === "lock" && mine.length === targets.length && targets.length > 0;
  // Stealing/breaking needs the tick; plain lock/unlock does not.
  const needsConfirm = steal;
  const canSubmit = wc !== null && !alreadyMine && (!needsConfirm || confirmed);
  const isLock = mode === "lock";

  return (
    <ActionDialog
      title={isLock ? d("lockTitle") : d("unlockTitle")}
      submitLabel={steal ? d("stealAction") : isLock ? d("lockAction") : d("unlockAction")}
      danger={steal}
      canSubmit={canSubmit}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc) return;
        const t0 = { kind: "wc" as const, wcId: wc.id, paths };
        try {
          const h = isLock
            ? await unwrap(
                ipc().commands.lock({ targets: t0, comment: comment.trim() || null, steal }),
              )
            : await unwrap(ipc().commands.unlock({ targets: t0, breakLock: steal }));
          toast.show(t("opStarted", { title: h.title }));
          onClose();
        } catch (e) {
          if (e instanceof IpcError && e.appError.code === "locked-by-other")
            setServerSaysLocked(true);
          throw e;
        }
      }}
    >
      <PathList paths={paths} label={d("files")} />
      {alreadyMine && <WarnBox>{d("alreadyYours")}</WarnBox>}
      {others.map((f) => (
        <WarnBox key={f.path}>{d("lockedBy", { owner: f.lock?.owner ?? "" })}</WarnBox>
      ))}
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
