import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Input } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { useT } from "../../../lib/i18n/i18n";
import { ActionDialog } from "../dialogs/action-dialog";
import { repoStrings } from "./repo-strings";

/** Display name rules of the backend (`wc_rename`): 1-200 characters, no control characters. */
export function isValidDisplayName(name: string): boolean {
  const n = name.trim();
  // biome-ignore lint/suspicious/noControlCharactersInRegex: control characters are refused
  return n.length > 0 && [...n].length <= 200 && !/[\u0000-\u001f\u007f-\u009f]/.test(n);
}

/** Renames the list entry only (app database); the folder on disk keeps its name. */
export function RenameRepoDialog({
  wcId,
  name,
  onClose,
}: {
  wcId: string;
  name: string;
  onClose: () => void;
}) {
  const r = useT(repoStrings);
  const toast = useToast();
  const qc = useQueryClient();
  const [value, setValue] = useState(name);
  const valid = isValidDisplayName(value);
  return (
    <ActionDialog
      title={r("renameTitle")}
      submitLabel={r("renameAction")}
      canSubmit={valid && value.trim() !== name}
      onClose={onClose}
      onSubmit={async () => {
        const next = value.trim();
        await unwrap(ipc().commands.wcRename(wcId, next));
        await qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
        toast.show(r("renamed", { name: next }));
        onClose();
      }}
    >
      <label className="flex flex-col gap-1">
        {r("renameLabel")}
        <Input
          value={value}
          onChange={(e) => setValue(e.target.value)}
          aria-invalid={!valid}
          maxLength={400}
          spellCheck={false}
        />
      </label>
      {!valid && (
        <span className="text-bad-text" role="alert">
          {r("renameInvalid")}
        </span>
      )}
      <span className="text-[12px] text-muted-text">{r("renameHint")}</span>
    </ActionDialog>
  );
}

/** Removes the entry from the list after confirmation. Never deletes files. */
export function RemoveRepoDialog({
  wcId,
  name,
  onClose,
}: {
  wcId: string;
  name: string;
  onClose: () => void;
}) {
  const r = useT(repoStrings);
  const toast = useToast();
  const qc = useQueryClient();
  return (
    <ActionDialog
      title={r("removeTitle")}
      submitLabel={r("removeAction")}
      danger
      onClose={onClose}
      onSubmit={async () => {
        await unwrap(ipc().commands.wcRemove(wcId));
        qc.removeQueries({ queryKey: queryKeys.status(wcId) });
        await qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
        toast.show(r("removed", { name }));
        onClose();
      }}
    >
      <SafeText as="p" className="m-0">
        {r("removeBody", { name })}
      </SafeText>
    </ActionDialog>
  );
}
