import { useMutation } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { BadBox, Dialog, WarnBox } from "../../../components/ui/dialog";
import { Textarea } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { LockInfo } from "../../../ipc/bindings";
import { IpcError, ipc, unwrap } from "../../../ipc/client";
import { relativeTime } from "../../../lib/format";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { lockStrings } from "./lock-strings";

export interface LockTarget {
  url: string;
  /** Existing lock on the file, if known. */
  lock: LockInfo | null;
}

/**
 * Lock / unlock dialog (doc 01, 4.7 and 10.4). A lock held by someone else is shown with its owner,
 * time and comment; stealing (lock) or breaking (unlock) needs an explicit tick before the danger
 * button enables. A `locked-by-other` error from the server also reveals the steal option.
 */
export function LockDialog({
  mode,
  targets,
  onClose,
}: {
  mode: "lock" | "unlock";
  targets: LockTarget[];
  onClose: () => void;
}) {
  const t = useT(lockStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const formId = useId();
  const [comment, setComment] = useState("");
  const [confirm, setConfirm] = useState(false);
  const [serverSaysLocked, setServerSaysLocked] = useState(false);

  const others = targets.filter((x) => x.lock && !x.lock.mine);
  const mine = targets.filter((x) => x.lock?.mine);
  const conflict = others.length > 0 || serverSaysLocked;
  const owner = others[0]?.lock?.owner ?? "?";

  const run = useMutation({
    mutationFn: () => {
      const urls = targets.map((x) => x.url);
      return mode === "lock"
        ? unwrap(
            ipc().commands.lock({
              targets: { kind: "url", urls },
              comment: comment.trim() || null,
              steal: conflict ? true : null,
            }),
          )
        : unwrap(
            ipc().commands.unlock({
              targets: { kind: "url", urls },
              breakLock: others.length > 0 ? true : null,
            }),
          );
    },
    onSuccess: () => {
      toast.show(t("requested"));
      onClose();
    },
    onError: (e) => {
      if (e instanceof IpcError && e.appError.code === "locked-by-other") setServerSaysLocked(true);
    },
  });

  const title = mode === "lock" ? t("lockTitle") : t("unlockTitle");
  const needsConfirm = conflict;
  const actionLabel =
    mode === "lock" ? (conflict ? t("stealAction") : t("lockAction")) : t("unlockAction");
  const ready = !needsConfirm || confirm;

  return (
    <Dialog
      open
      onClose={onClose}
      title={title}
      footer={
        <>
          <Button onClick={onClose}>{c("cancel")}</Button>
          <Button
            type="submit"
            form={formId}
            variant={needsConfirm ? "danger" : "primary"}
            disabled={
              !ready || run.isPending || (mode === "lock" && mine.length === targets.length)
            }
          >
            {actionLabel}
          </Button>
        </>
      }
    >
      <form
        id={formId}
        className="flex flex-col gap-2.5"
        onSubmit={(e) => {
          e.preventDefault();
          if (ready) run.mutate();
        }}
      >
        <section aria-label={t("files")}>
          <ul className="m-0 max-h-32 list-none overflow-auto p-0 font-mono text-[12px]">
            {targets.map((x) => (
              <li key={x.url} className="truncate" title={x.url}>
                <SafeText>{decodeURIComponent(x.url.split("/").pop() ?? x.url)}</SafeText>
              </li>
            ))}
          </ul>
        </section>
        {mode === "lock" && mine.length === targets.length && (
          <WarnBox>{t("alreadyYours")}</WarnBox>
        )}
        {conflict && (
          <BadBox>
            {others[0]?.lock && (
              <>
                <p className="m-0">
                  <SafeText>
                    {t("lockedByOther", {
                      owner: others[0].lock.owner,
                      when: relativeTime(others[0].lock.createdAt),
                    })}
                  </SafeText>
                </p>
                {others[0].lock.comment && (
                  <p className="m-0">
                    <SafeText>{t("lockComment", { comment: others[0].lock.comment })}</SafeText>
                  </p>
                )}
              </>
            )}
            <p className="m-0">
              {mode === "lock" ? t("contactOwner", { owner }) : t("breakWarning")}
            </p>
            {mode === "lock" && <p className="m-0">{t("stealWarning", { owner })}</p>}
          </BadBox>
        )}
        {mode === "lock" && (
          <label className="flex flex-col gap-1">
            {t("comment")}
            <Textarea rows={2} value={comment} onChange={(e) => setComment(e.target.value)} />
          </label>
        )}
        {needsConfirm && (
          <label className="flex items-start gap-2">
            <input
              type="checkbox"
              checked={confirm}
              onChange={(e) => setConfirm(e.target.checked)}
              className="mt-0.5 accent-[var(--accent)]"
            />
            {mode === "lock" ? t("stealConfirm") : t("breakLock")}
          </label>
        )}
        {run.error && (
          <p className="m-0 text-bad-text" role="alert">
            {run.error instanceof IpcError && run.error.appError.code === "locked-by-other"
              ? t("errLockedByOther")
              : run.error.message}
          </p>
        )}
      </form>
    </Dialog>
  );
}
