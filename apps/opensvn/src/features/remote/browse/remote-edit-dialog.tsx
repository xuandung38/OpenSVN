import { useMutation } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { BadBox, Dialog, WarnBox } from "../../../components/ui/dialog";
import { Input, Textarea } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { RemoteOp } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { baseName, joinUrl, parentUrl, validateEntryName } from "../repo-url";
import { browseStrings } from "./browse-strings";

export type RemoteEditMode = "mkdir" | "rename" | "delete";

const NAME_ERRORS = {
  empty: "nameEmpty",
  slash: "nameSlash",
  dots: "nameDots",
  chars: "nameChars",
} as const;

/**
 * Direct server edit (commits immediately, so a message is always required). `targetUrl` is the
 * parent folder for `mkdir`, and the entry itself for `rename`/`delete`.
 */
export function RemoteEditDialog({
  mode,
  targetUrl,
  repoRoot,
  onClose,
}: {
  mode: RemoteEditMode;
  targetUrl: string;
  repoRoot: string;
  onClose: () => void;
}) {
  const t = useT(browseStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const formId = useId();
  const [name, setName] = useState(mode === "rename" ? baseName(targetUrl) : "");
  const [message, setMessage] = useState("");
  const nameError = mode === "delete" ? null : validateEntryName(name);
  const messageOk = message.trim().length > 0;
  const valid = messageOk && (mode === "delete" || (nameError === null && name.trim().length > 0));

  const run = useMutation({
    mutationFn: () => {
      const trimmed = name.trim();
      const op: RemoteOp =
        mode === "mkdir"
          ? { kind: "mkdir", url: joinUrl(targetUrl, trimmed) }
          : mode === "rename"
            ? {
                kind: "rename",
                fromUrl: targetUrl,
                toUrl: joinUrl(parentUrl(targetUrl, repoRoot), trimmed),
              }
            : { kind: "delete", url: targetUrl };
      return unwrap(ipc().commands.remoteEdit({ ops: [op], message: message.trim() }));
    },
    onSuccess: () => {
      toast.show(t("requested"));
      onClose();
    },
  });

  const title =
    mode === "mkdir" ? t("editNewFolder") : mode === "rename" ? t("editRename") : t("editDelete");
  const warning =
    mode === "mkdir"
      ? t("editNewFolderWarning")
      : mode === "rename"
        ? t("editRenameWarning")
        : t("editDeleteWarning");

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
            variant={mode === "delete" ? "danger" : "primary"}
            disabled={!valid || run.isPending}
          >
            {title}
          </Button>
        </>
      }
    >
      <form
        id={formId}
        className="flex flex-col gap-2.5"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) run.mutate();
        }}
      >
        {mode === "delete" ? (
          <BadBox>
            <SafeText className="break-all font-mono text-[12px]">{targetUrl}</SafeText>
          </BadBox>
        ) : (
          <div className="flex flex-col gap-1">
            <label className="flex flex-col gap-1">
              {t("editNameLabel")}
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                aria-invalid={nameError !== null && name.length > 0}
                spellCheck={false}
              />
            </label>
            {nameError && name.length > 0 && (
              <span className="text-bad-text" role="alert">
                {t(NAME_ERRORS[nameError])}
              </span>
            )}
          </div>
        )}
        <WarnBox>{warning}</WarnBox>
        <label className="flex flex-col gap-1">
          {t("editMessageLabel")}
          <Textarea rows={3} value={message} onChange={(e) => setMessage(e.target.value)} />
        </label>
        {run.error && (
          <p className="m-0 text-bad-text" role="alert">
            {run.error.message}
          </p>
        )}
      </form>
    </Dialog>
  );
}
