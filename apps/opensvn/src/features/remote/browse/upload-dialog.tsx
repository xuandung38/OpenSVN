import { useMutation } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { Dialog, WarnBox } from "../../../components/ui/dialog";
import { Input, Textarea } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { PathGrantInfo } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { joinUrl, validateEntryName } from "../repo-url";
import { browseStrings } from "./browse-strings";
import { useRepoList } from "./use-repo";

const NAME_ERRORS = {
  empty: "nameEmpty",
  slash: "nameSlash",
  dots: "nameDots",
  chars: "nameChars",
} as const;

/**
 * Uploads one local file into `folderUrl` as a direct commit (`RemoteOp::Upload`). The file comes
 * from the native picker as a single-use grant; the name defaults to the file's own and an
 * existing file of that name is replaced (the dialog says so).
 */
export function UploadDialog({
  folderUrl,
  rev,
  onClose,
}: {
  folderUrl: string;
  /** The revision being browsed (`null` = HEAD); the upload always goes to HEAD. */
  rev: number | null;
  onClose: () => void;
}) {
  const t = useT(browseStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const formId = useId();
  const [grant, setGrant] = useState<PathGrantInfo | null>(null);
  const [name, setName] = useState("");
  const [message, setMessage] = useState("");
  const [pickError, setPickError] = useState<string | null>(null);
  // The folder at HEAD, where the upload lands: warns before replacing a file.
  const head = useRepoList(folderUrl, null);
  const nameError = validateEntryName(name);
  const existing = (head.data?.entries ?? []).find((e) => e.name === name.trim()) ?? null;
  const valid =
    grant !== null && nameError === null && message.trim().length > 0 && existing?.kind !== "dir";

  const pick = async () => {
    setPickError(null);
    try {
      const g = await unwrap(
        ipc().commands.pathPick({ kind: "file", title: t("uploadPickTitle") }),
      );
      if (!g) return;
      setGrant(g);
      if (name.trim() === "" || name === grant?.displayName) setName(g.displayName);
    } catch (e) {
      setPickError(toAppError(e).message);
    }
  };

  const run = useMutation({
    mutationFn: () => {
      if (!grant) throw new Error("no file");
      return unwrap(
        ipc().commands.remoteEdit({
          ops: [{ kind: "upload", url: joinUrl(folderUrl, name.trim()), source: grant.id }],
          message: message.trim(),
        }),
      );
    },
    onSuccess: () => {
      toast.show(t("requested"));
      onClose();
    },
    // The grant is spent by the request; a new attempt needs a new choice.
    onError: () => setGrant(null),
  });

  return (
    <Dialog
      open
      onClose={onClose}
      title={t("uploadTitle")}
      footer={
        <>
          <Button onClick={onClose}>{c("cancel")}</Button>
          <Button type="submit" form={formId} variant="primary" disabled={!valid || run.isPending}>
            {t("uploadAction")}
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
        <p className="m-0 text-muted-text">
          <SafeText className="break-all">{t("uploadTarget", { folder: folderUrl })}</SafeText>
        </p>
        <div className="flex items-center gap-2">
          <Button onClick={() => void pick()}>
            {grant ? t("uploadChooseAgain") : t("uploadChoose")}
          </Button>
          <span aria-live="polite" className="min-w-0 truncate">
            {grant ? (
              <SafeText>{t("uploadChosen", { name: grant.displayName })}</SafeText>
            ) : (
              t("uploadNoFile")
            )}
          </span>
        </div>
        {pickError && (
          <p className="m-0 text-bad-text" role="alert">
            {pickError}
          </p>
        )}
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
        {rev !== null && <WarnBox>{t("uploadOldRev", { rev })}</WarnBox>}
        {existing?.kind === "dir" && (
          <p className="m-0 text-bad-text" role="alert">
            <SafeText>{t("uploadIsFolder", { name: existing.name })}</SafeText>
          </p>
        )}
        {existing?.kind === "file" && (
          <WarnBox>
            <SafeText>{t("uploadReplaces", { name: existing.name })}</SafeText>
          </WarnBox>
        )}
        <WarnBox>{t("uploadWarning")}</WarnBox>
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
