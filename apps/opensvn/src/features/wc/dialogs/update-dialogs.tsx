import { useState } from "react";
import { WarnBox } from "../../../components/ui/dialog";
import { Input } from "../../../components/ui/fields";
import { useToast } from "../../../components/ui/toast";
import { ipc, unwrap } from "../../../ipc/client";
import { useT } from "../../../lib/i18n/i18n";
import { parseRevision } from "../../remote/branch/branch-model";
import { hasQueryOrFragment, useWebLinkConversion } from "../../remote/web-link";
import { isCheckoutUrl } from "../wc-model";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";
import { ActionDialog } from "./action-dialog";
import { dialogStrings } from "./dialog-strings";
import { UrlFieldNote } from "./web-link-note";

/** Update the whole working copy to HEAD or a given revision (doc 01 3.2, "Update to revision"). */
export function UpdateToRevisionDialog({ onClose }: { onClose: () => void }) {
  const d = useT(dialogStrings);
  const t = useT(wcStrings);
  const toast = useToast();
  const { wc } = useWcSession();
  const [text, setText] = useState("");
  const rev = parseRevision(text);
  const invalid = rev === "invalid";
  const older = typeof rev === "number" && wc !== null && rev < wc.baseRev;

  return (
    <ActionDialog
      title={d("updateRevTitle")}
      submitLabel={d("updateRevAction")}
      canSubmit={wc !== null && !invalid}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc || rev === "invalid") return;
        await unwrap(ipc().commands.update({ wcId: wc.id, paths: null, rev, depth: null }));
        toast.show(t("updateRequested"));
        onClose();
      }}
    >
      <label className="flex flex-col gap-1">
        {d("revision")}
        <Input
          value={text}
          onChange={(e) => setText(e.target.value)}
          placeholder={d("revisionHint")}
          aria-invalid={invalid}
        />
      </label>
      {invalid && (
        <span className="text-bad-text" role="alert">
          {d("revisionInvalid")}
        </span>
      )}
      {older && <WarnBox>{d("updateRevBody")}</WarnBox>}
    </ActionDialog>
  );
}

/** Switch the working copy to another URL of the same repository, carrying local changes along. */
export function SwitchDialog({ onClose }: { onClose: () => void }) {
  const d = useT(dialogStrings);
  const t = useT(wcStrings);
  const toast = useToast();
  const { wc } = useWcSession();
  const [url, setUrl] = useState(wc?.url ?? "");
  const [revText, setRevText] = useState("");
  const rev = parseRevision(revText);
  const root = wc?.repoRootUrl ?? "";
  const trimmed = url.trim().replace(/\/+$/, "");
  const webLink = useWebLinkConversion(url, (converted, pegRev) => {
    setUrl(converted);
    if (pegRev !== null) setRevText(String(pegRev));
  });
  const inRepo =
    isCheckoutUrl(trimmed) &&
    !hasQueryOrFragment(trimmed) &&
    (trimmed === root || trimmed.startsWith(`${root}/`));
  const same = trimmed === (wc?.url ?? "").replace(/\/+$/, "");

  return (
    <ActionDialog
      title={d("switchTitle")}
      submitLabel={d("switchAction")}
      canSubmit={wc !== null && inRepo && !same && rev !== "invalid"}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc || rev === "invalid") return;
        const h = await unwrap(ipc().commands.switch(wc.id, trimmed, rev));
        toast.show(t("opStarted", { title: h.title }));
        onClose();
      }}
    >
      <label className="flex flex-col gap-1">
        {d("switchUrl")}
        <Input
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          spellCheck={false}
          aria-invalid={url.length > 0 && !inRepo}
        />
      </label>
      <UrlFieldNote
        url={url}
        state={webLink}
        revText={revText}
        fallback={inRepo ? null : d("switchUrlInvalid", { root })}
      />
      <label className="flex flex-col gap-1">
        {d("revision")}
        <Input
          value={revText}
          onChange={(e) => setRevText(e.target.value)}
          placeholder={d("revisionHint")}
          aria-invalid={rev === "invalid"}
        />
      </label>
      <span className="text-[12px] text-muted-text">{d("switchBody")}</span>
    </ActionDialog>
  );
}

/**
 * Cleanup: finish interrupted operations, optionally remove unversioned/ignored files, and, only
 * after an explicit confirmation tick, remove the lock another program left behind (the engine
 * still refuses while it can see that program running, and never deletes files for it).
 */
export function CleanupDialog({
  foreignLock = false,
  onClose,
}: {
  /** Opened because another program holds the lock: say so above the options. */
  foreignLock?: boolean;
  onClose: () => void;
}) {
  const d = useT(dialogStrings);
  const t = useT(wcStrings);
  const toast = useToast();
  const { wc } = useWcSession();
  const [removeUnversioned, setRemoveUnversioned] = useState(false);
  const [removeIgnored, setRemoveIgnored] = useState(false);
  const [vacuum, setVacuum] = useState(false);
  const [breakLock, setBreakLock] = useState(false);
  const [confirmed, setConfirmed] = useState(false);
  const opts = [
    [removeUnversioned, setRemoveUnversioned, "cleanupRemoveUnversioned"],
    [removeIgnored, setRemoveIgnored, "cleanupRemoveIgnored"],
    [vacuum, setVacuum, "cleanupVacuum"],
  ] as const;

  return (
    <ActionDialog
      title={d("cleanupTitle")}
      submitLabel={d("cleanupAction")}
      danger={breakLock}
      canSubmit={wc !== null && (!breakLock || confirmed)}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc) return;
        const h = await unwrap(
          ipc().commands.cleanup({
            wcId: wc.id,
            removeUnversioned,
            removeIgnored,
            vacuumPristines: vacuum,
            breakForeignLocks: breakLock && confirmed,
          }),
        );
        toast.show(t("opStarted", { title: h.title }));
        onClose();
      }}
    >
      <p className="m-0">{d("cleanupBody")}</p>
      {foreignLock && <WarnBox>{d("cleanupBreakLockBusy")}</WarnBox>}
      {opts.map(([on, set, key]) => (
        <label key={key} className="flex items-center gap-2">
          <input
            type="checkbox"
            checked={on}
            onChange={(e) => set(e.target.checked)}
            className="accent-[var(--accent)]"
          />
          {d(key)}
        </label>
      ))}
      <label className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={breakLock}
          onChange={(e) => {
            setBreakLock(e.target.checked);
            setConfirmed(false);
          }}
          className="accent-[var(--accent)]"
        />
        {d("cleanupBreakLock")}
      </label>
      {breakLock && (
        <WarnBox>
          <p className="m-0">{d("cleanupBreakLockWarn")}</p>
          <label className="mt-1.5 flex items-center gap-2 font-semibold">
            <input
              type="checkbox"
              checked={confirmed}
              onChange={(e) => setConfirmed(e.target.checked)}
              className="accent-[var(--accent)]"
            />
            {d("cleanupBreakLockConfirm")}
          </label>
        </WarnBox>
      )}
    </ActionDialog>
  );
}
