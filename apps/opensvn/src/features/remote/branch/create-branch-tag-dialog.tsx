import { useMutation, useQuery } from "@tanstack/react-query";
import { useId, useMemo, useState } from "react";
import { Button } from "../../../components/ui/button";
import { Dialog, WarnBox } from "../../../components/ui/dialog";
import { Input, Textarea } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { SegmentedControl } from "../../../components/ui/segmented-control";
import { useToast } from "../../../components/ui/toast";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { RepoTree } from "../browse/repo-tree";
import { baseName, joinUrl, repoPath, validateEntryName } from "../repo-url";
import { parseRevision, refContainerUrl } from "./branch-model";
import { branchStrings } from "./branch-strings";

const NAME_ERRORS = {
  empty: "nameEmpty",
  slash: "nameSlash",
  dots: "nameDots",
  chars: "nameChars",
} as const;

type SourceChoice = "head" | "rev";

/**
 * Create branch / tag (repo with trunk/branches/tags layout) or "Copy to new location" (folder-style
 * repo), per doc 01 section 3.9 and 4.5. Both are a server-side `svn copy URL URL`. When the source
 * URL does not follow the layout convention, a requested branch/tag falls back to Location mode.
 */
export function CreateBranchTagDialog({
  kind,
  sourceUrl,
  sourceName,
  repoRootUrl,
  repoUuid,
  currentRefUrl,
  wcId,
  onClose,
}: {
  /** `branch`/`tag` for layout repos; `location` for folder-style repos. */
  kind: "branch" | "tag" | "location";
  /** URL to copy from (current branch, or a node picked in the browser). */
  sourceUrl: string;
  sourceName: string;
  repoRootUrl: string;
  /** Repository identity for the backend layout lookup. */
  repoUuid: string;
  /** The working copy's current branch/tag URL (project detection), if any. */
  currentRefUrl: string | null;
  /** When set, the dialog offers switching this working copy to the copy. */
  wcId: string | null;
  onClose: () => void;
}) {
  const t = useT(branchStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const formId = useId();
  const [refKind, setRefKind] = useState<"branch" | "tag">(kind === "tag" ? "tag" : "branch");
  // The backend knows the layout (incl. custom ones); the naming convention is only a fallback.
  const layoutQ = useQuery({
    queryKey: queryKeys.layout(repoUuid),
    queryFn: () => unwrap(ipc().commands.layoutGet(repoUuid)),
  });
  const nodesQ = useQuery({
    queryKey: queryKeys.branches(repoRootUrl),
    queryFn: () => unwrap(ipc().commands.branchesList(repoRootUrl)),
  });
  const lookupPending = kind !== "location" && (layoutQ.isPending || nodesQ.isPending);
  const container = useMemo(
    () =>
      kind === "location"
        ? null
        : refContainerUrl(refKind, {
            layout: layoutQ.data ?? null,
            nodes: nodesQ.data ?? [],
            currentRefUrl,
            sourceUrl,
            repoRootUrl,
          }),
    [kind, refKind, layoutQ.data, nodesQ.data, currentRefUrl, sourceUrl, repoRootUrl],
  );
  const layoutMode = kind !== "location" && container !== null;

  const [name, setName] = useState(layoutMode ? "" : `${baseName(sourceUrl)}-copy`);
  const [sourceChoice, setSourceChoice] = useState<SourceChoice>("head");
  const [revText, setRevText] = useState("");
  const [destFolder, setDestFolder] = useState(repoRootUrl);
  const [message, setMessage] = useState("");
  const [switchAfter, setSwitchAfter] = useState(false);

  const nameError = validateEntryName(name);
  const rev = sourceChoice === "rev" ? parseRevision(revText) : null;
  const revInvalid = sourceChoice === "rev" && (rev === "invalid" || rev === null);
  const toUrl = layoutMode
    ? joinUrl(container as string, name.trim() || "…")
    : joinUrl(destFolder, name.trim() || "…");
  const defaultMessage = layoutMode
    ? t(refKind === "tag" ? "defaultMessageTag" : "defaultMessageBranch", { name: name.trim() })
    : t("defaultMessageCopy", {
        from: repoPath(sourceUrl, repoRootUrl),
        to: repoPath(toUrl, repoRootUrl),
      });
  const finalMessage = message.trim() || defaultMessage;
  const valid = nameError === null && !revInvalid && finalMessage.length > 0 && !lookupPending;

  const run = useMutation({
    mutationFn: () =>
      unwrap(
        ipc().commands.copyLocation({
          fromUrl: sourceUrl,
          fromRev: typeof rev === "number" ? rev : null,
          toUrl,
          message: finalMessage,
          switchWcId: switchAfter && wcId ? wcId : null,
          fromWc: null,
        }),
      ),
    onSuccess: () => {
      toast.show(t("requested"));
      onClose();
    },
  });

  const title = layoutMode
    ? refKind === "tag"
      ? t("newTag")
      : t("newBranch")
    : t("copyToLocation");

  return (
    <Dialog
      open
      onClose={onClose}
      title={title}
      footer={
        <>
          <Button onClick={onClose}>{c("cancel")}</Button>
          <Button type="submit" form={formId} variant="primary" disabled={!valid || run.isPending}>
            {c("create")}
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
        {kind !== "location" && !layoutMode && !lookupPending && (
          <WarnBox>{t("cannotDerive")}</WarnBox>
        )}
        {layoutMode && (
          <SegmentedControl<"branch" | "tag">
            label={t("kindLabel")}
            value={refKind}
            onChange={setRefKind}
            options={[
              { value: "branch", label: t("newBranch") },
              { value: "tag", label: t("newTag") },
            ]}
          />
        )}
        <div className="flex flex-col gap-1">
          <label className="flex flex-col gap-1">
            {layoutMode ? t("name") : t("destinationName")}
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
        <fieldset className="m-0 flex flex-col gap-1.5 border-0 p-0">
          <legend className="mb-1 p-0">{t("source")}</legend>
          <label className="flex items-center gap-2">
            <input
              type="radio"
              name="source"
              checked={sourceChoice === "head"}
              onChange={() => setSourceChoice("head")}
              className="accent-[var(--accent)]"
            />
            {t("sourceHead", { name: sourceName })}
          </label>
          <label className="flex items-center gap-2">
            <input
              type="radio"
              name="source"
              checked={sourceChoice === "rev"}
              onChange={() => setSourceChoice("rev")}
              className="accent-[var(--accent)]"
            />
            {t("sourceRev")}
            <Input
              aria-label={t("revision")}
              placeholder={t("revisionHint")}
              disabled={sourceChoice !== "rev"}
              value={revText}
              onChange={(e) => setRevText(e.target.value)}
              className="w-32"
            />
          </label>
          {sourceChoice === "rev" && revInvalid && revText.length > 0 && (
            <span className="text-bad-text" role="alert">
              {t("revisionInvalid")}
            </span>
          )}
          <label
            className="flex items-center gap-2 text-muted-text"
            title={t("sourceWcUnavailable")}
          >
            <input type="radio" name="source" disabled className="accent-[var(--accent)]" />
            {t("sourceWc")}
          </label>
        </fieldset>
        {!layoutMode && (
          <div className="flex flex-col gap-1">
            <span>{t("destinationFolder")}</span>
            <RepoTree
              label={t("destinationFolder")}
              rootUrl={repoRootUrl}
              rev={null}
              selectedUrl={destFolder}
              onSelect={setDestFolder}
              className="h-36 rounded-md border border-line bg-surface-2"
            />
          </div>
        )}
        <dl className="m-0 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-[12px]">
          <dt className="text-muted-text">{t("sourceUrl")}</dt>
          <dd className="m-0 break-all font-mono">
            <SafeText>{repoPath(sourceUrl, repoRootUrl)}</SafeText>
          </dd>
          <dt className="text-muted-text">{t("destination")}</dt>
          <dd className="m-0 break-all font-mono">
            <SafeText>{repoPath(toUrl, repoRootUrl)}</SafeText>
          </dd>
        </dl>
        <label className="flex flex-col gap-1">
          {t("message")}
          <Textarea
            rows={2}
            value={message}
            placeholder={defaultMessage}
            onChange={(e) => setMessage(e.target.value)}
          />
        </label>
        {wcId && (refKind !== "tag" || !layoutMode) && (
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={switchAfter}
              onChange={(e) => setSwitchAfter(e.target.checked)}
              className="accent-[var(--accent)]"
            />
            {layoutMode ? t("switchAfter", { kind: t(refKind) }) : t("switchAfterCopy")}
          </label>
        )}
        <WarnBox>{t("createWarning")}</WarnBox>
        {run.error && (
          <p className="m-0 text-bad-text" role="alert">
            {run.error.message}
          </p>
        )}
      </form>
    </Dialog>
  );
}
