import { useEffect, useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { Dialog } from "../../../components/ui/dialog";
import { Input, Select } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import type { Depth, PathGrantInfo } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import {
  useOperationOutcome,
  useOperations,
  useRecentUrls,
  useSettings,
} from "../../../ipc/queries";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { parseRevision } from "../../remote/branch/branch-model";
import { RepoTree } from "../../remote/browse/repo-tree";
import { useRepoInfo } from "../../remote/browse/use-repo";
import {
  type FolderNameError,
  joinDisplay,
  suggestFolderName,
  validateFolderName,
} from "../repos/repo-model";
import { isCheckoutUrl } from "../wc-model";
import { pickFolder } from "../welcome/use-path-grant";
import { dialogStrings } from "./dialog-strings";

const DEPTHS: {
  value: Depth;
  label: "depthInfinity" | "depthImmediates" | "depthFiles" | "depthEmpty";
}[] = [
  { value: "infinity", label: "depthInfinity" },
  { value: "immediates", label: "depthImmediates" },
  { value: "files", label: "depthFiles" },
  { value: "empty", label: "depthEmpty" },
];

const NAME_ERRORS: Record<
  FolderNameError,
  "nameEmpty" | "nameSeparator" | "nameDots" | "nameChars" | "nameTrailing" | "nameReserved"
> = {
  empty: "nameEmpty",
  separator: "nameSeparator",
  dots: "nameDots",
  chars: "nameChars",
  trailing: "nameTrailing",
  reserved: "nameReserved",
};

/**
 * Checkout (doc 01 §4.1). The working copy is created in a new folder `name` inside a parent folder:
 * the default checkout folder (`general.checkoutRoot`, granted by Rust at submit time) or a folder
 * chosen in the native dialog. Either way the app only holds an opaque, single-use grant, never a
 * raw path. While the checkout runs the dialog shows its progress with Cancel; on success the shell
 * opens the new working copy (`useOpenCheckedOut`). Sign-in and certificate prompts appear on top
 * through the global prompt host.
 */
export function CheckoutDialog({
  onClose,
  initialUrl = "",
}: {
  onClose: () => void;
  initialUrl?: string;
}) {
  const d = useT(dialogStrings);
  const c = useT(commonStrings);
  const formId = useId();
  const recentId = useId();
  const settings = useSettings();
  const recent = useRecentUrls();
  const [url, setUrl] = useState(initialUrl);
  const [revText, setRevText] = useState("");
  const [depth, setDepth] = useState<Depth>("infinity");
  const [ignoreExternals, setIgnoreExternals] = useState(false);
  // `null` = the default checkout folder; a grant once the user picked another parent.
  const [picked, setPicked] = useState<PathGrantInfo | null>(null);
  const [needsRepick, setNeedsRepick] = useState(false);
  const [name, setName] = useState<string | null>(null);
  const [browseFor, setBrowseFor] = useState<string | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [opId, setOpId] = useState<string | null>(null);

  const trimmedUrl = url.trim();
  const rev = parseRevision(revText);
  const urlOk = isCheckoutUrl(trimmedUrl);
  // The folder name follows the URL until the user edits it.
  const folderName = name ?? suggestFolderName(trimmedUrl);
  const nameError = validateFolderName(folderName);
  const defaultRoot = settings.data?.["general.checkoutRoot"] ?? "";
  const parentLabel = picked ? picked.displayName : defaultRoot;
  const canSubmit =
    urlOk && rev !== "invalid" && nameError === null && !needsRepick && !pending && !opId;

  const browseInfo = useRepoInfo(browseFor);
  const op = useOperations().find((o) => o.handle.opId === opId);
  const outcome = useOperationOutcome(opId ?? undefined);

  useEffect(() => {
    if (!outcome) return;
    if (outcome.status === "ok") {
      onClose();
      return;
    }
    setOpId(null);
    setError(outcome.error.code === "cancelled" ? d("checkoutCancelled") : outcome.error.message);
  }, [outcome, onClose, d]);

  const chooseParent = async () => {
    setError(null);
    try {
      const g = await pickFolder(d("parentFolder"));
      if (g) {
        setPicked(g);
        setNeedsRepick(false);
      }
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  const submit = async () => {
    if (!canSubmit) return;
    setPending(true);
    setError(null);
    const usedPicked = picked !== null;
    try {
      const dest = picked ?? (await unwrap(ipc().commands.checkoutRootGrant()));
      // Grants are single use: whatever happens next, a picked parent must be chosen again.
      if (usedPicked) setPicked(null);
      const handle = await unwrap(
        ipc().commands.checkout({
          url: trimmedUrl,
          dest: dest.id,
          destName: folderName,
          rev,
          depth,
          sparse: null,
          format: 31,
          storePristine: true,
          ignoreExternals,
        }),
      );
      setOpId(handle.opId);
    } catch (e) {
      if (usedPicked) setNeedsRepick(true);
      setError(toAppError(e).message);
    } finally {
      setPending(false);
    }
  };

  const progress = op?.progress;
  const running = opId !== null;

  return (
    <Dialog
      open
      onClose={onClose}
      title={d("checkoutTitle")}
      footer={
        running ? (
          <>
            <Button onClick={onClose}>{d("background")}</Button>
            <Button
              variant="danger"
              onClick={() => {
                if (opId) void unwrap(ipc().commands.opCancel(opId)).catch(() => {});
              }}
            >
              {d("cancelCheckout")}
            </Button>
          </>
        ) : (
          <>
            <Button onClick={onClose}>{c("cancel")}</Button>
            <Button type="submit" form={formId} variant="primary" disabled={!canSubmit}>
              {d("checkoutAction")}
            </Button>
          </>
        )
      }
    >
      {running ? (
        <div className="flex flex-col gap-2" aria-live="polite">
          <SafeText as="p" className="m-0 font-semibold">
            {op?.handle.title ?? d("checkingOut")}
          </SafeText>
          <progress
            className="w-full"
            aria-label={d("checkingOut")}
            value={progress?.current ?? undefined}
            max={progress?.total ?? undefined}
          />
          <SafeText as="p" className="m-0 truncate font-mono text-[12px] text-muted-text">
            {progress?.current != null && progress.total != null
              ? `${d("progressItems", { current: progress.current, total: progress.total })}${
                  progress.currentPath ? ` · ${progress.currentPath}` : ""
                }`
              : d("checkoutPreparing")}
          </SafeText>
        </div>
      ) : (
        <form
          id={formId}
          className="flex flex-col gap-2.5"
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
        >
          <div className="flex flex-col gap-1">
            <label htmlFor={`${formId}-url`}>{d("repoUrl")}</label>
            <div className="flex gap-2">
              <Input
                id={`${formId}-url`}
                value={url}
                onChange={(e) => setUrl(e.target.value)}
                placeholder={d("repoUrlHint")}
                aria-invalid={url.length > 0 && !urlOk}
                list={recentId}
                spellCheck={false}
                className="flex-1"
              />
              <Button
                aria-expanded={browseFor !== null}
                disabled={!urlOk && browseFor === null}
                onClick={() => setBrowseFor((cur) => (cur === null ? trimmedUrl : null))}
              >
                {browseFor === null ? d("browseRepo") : d("hideBrowse")}
              </Button>
            </div>
          </div>
          <datalist id={recentId} aria-label={d("recentUrls")}>
            {(recent.data ?? []).map((u) => (
              <option key={u} value={u} />
            ))}
          </datalist>
          {url.length > 0 && !urlOk && (
            <span className="text-bad-text" role="alert">
              {d("urlInvalid")}
            </span>
          )}
          {browseFor !== null &&
            (browseInfo.data ? (
              <RepoTree
                rootUrl={browseInfo.data.repoRootUrl}
                rev={null}
                selectedUrl={trimmedUrl}
                onSelect={setUrl}
                label={d("browseLabel")}
                className="max-h-48 rounded-md border border-line"
              />
            ) : browseInfo.isError ? (
              <SafeText as="p" className="m-0 text-bad-text">
                {toAppError(browseInfo.error).message}
              </SafeText>
            ) : (
              <span className="text-muted-text">{d("browseLoading")}</span>
            ))}
          <div className="grid grid-cols-[1fr_auto] items-end gap-2">
            <div className="flex min-w-0 flex-col gap-1">
              <span id={`${formId}-parent`}>{d("parentFolder")}</span>
              <SafeText
                as="span"
                className="min-w-0 truncate rounded-md border border-line bg-surface-2 px-2 py-1.5 font-mono text-[12px]"
              >
                {needsRepick
                  ? d("noFolder")
                  : picked
                    ? parentLabel
                    : `${parentLabel} (${d("defaultParent")})`}
              </SafeText>
            </div>
            <Button aria-describedby={`${formId}-parent`} onClick={() => void chooseParent()}>
              {d("changeParent")}
            </Button>
          </div>
          {needsRepick && (
            <span className="text-bad-text" role="alert">
              {d("parentAgain")}
            </span>
          )}
          <label className="flex flex-col gap-1">
            {d("folderName")}
            <Input
              value={folderName}
              onChange={(e) => setName(e.target.value)}
              aria-invalid={nameError !== null && name !== null}
              spellCheck={false}
            />
          </label>
          {nameError !== null && name !== null ? (
            <span className="text-bad-text" role="alert">
              {d(NAME_ERRORS[nameError])}
            </span>
          ) : (
            folderName && (
              <SafeText as="span" className="truncate font-mono text-[12px] text-muted-text">
                {d("willCreate", { path: joinDisplay(parentLabel, folderName) })}
              </SafeText>
            )
          )}
          <div className="grid grid-cols-2 gap-2.5">
            <label className="flex flex-col gap-1">
              {d("revision")}
              <Input
                value={revText}
                onChange={(e) => setRevText(e.target.value)}
                placeholder={d("revisionHint")}
                aria-invalid={rev === "invalid"}
              />
            </label>
            <label className="flex flex-col gap-1">
              {d("depth")}
              <Select value={depth} onChange={(e) => setDepth(e.target.value as Depth)}>
                {DEPTHS.map((o) => (
                  <option key={o.value} value={o.value}>
                    {d(o.label)}
                  </option>
                ))}
              </Select>
            </label>
          </div>
          {rev === "invalid" && (
            <span className="text-bad-text" role="alert">
              {d("revisionInvalid")}
            </span>
          )}
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={ignoreExternals}
              onChange={(e) => setIgnoreExternals(e.target.checked)}
              className="accent-[var(--accent)]"
            />
            {d("ignoreExternals")}
          </label>
          <span className="text-[12px] text-muted-text">{d("formatNote")}</span>
        </form>
      )}
      {error && (
        <SafeText as="p" className="m-0 break-words text-bad-text">
          <span role="alert">{error}</span>
        </SafeText>
      )}
    </Dialog>
  );
}
