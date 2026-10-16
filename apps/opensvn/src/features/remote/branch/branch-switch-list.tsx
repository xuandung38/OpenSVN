import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { PopoverGroup } from "../../../components/ui/popover";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { BranchNode, WorkingCopy } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { useT } from "../../../lib/i18n/i18n";
import { wcStrings } from "../../wc/wc-strings";
import { projectRootOf } from "./branch-model";
import { branchStrings } from "./branch-strings";

const norm = (url: string) => url.replace(/\/+$/, "");

/**
 * Branches and tags the working copy can switch to (doc 01 §10.5): the ones of the same project as
 * the current branch, without the current one. With local changes, the switch asks first whether
 * to carry them over. Only shown when the working copy has a branch (a trunk/branches/tags layout).
 */
export function BranchSwitchList({
  wc,
  changed,
  onSwitched,
}: {
  wc: WorkingCopy;
  /** Number of local changes that a switch would carry over. */
  changed: number;
  onSwitched: () => void;
}) {
  const t = useT(branchStrings);
  const w = useT(wcStrings);
  const toast = useToast();
  const [pending, setPending] = useState<BranchNode | null>(null);
  const [busy, setBusy] = useState(false);
  const current = wc.branch;
  const atUrl = current?.url ?? null;
  const nodesQ = useQuery({
    queryKey: queryKeys.branches(wc.repoRootUrl, atUrl),
    queryFn: () => unwrap(ipc().commands.branchesList(wc.repoRootUrl, atUrl)),
    // Folder-style working copies (no branch) never list branches.
    enabled: current != null,
  });
  if (!current) return null;

  const project = projectRootOf(norm(current.url));
  const targets = (nodesQ.data ?? []).filter(
    (n) => norm(n.url) !== norm(current.url) && projectRootOf(norm(n.url)) === project,
  );

  const start = async (node: BranchNode) => {
    setBusy(true);
    try {
      const h = await unwrap(ipc().commands.switch(wc.id, node.url, null, false));
      toast.show(w("opStarted", { title: h.title }));
      setPending(null);
      onSwitched();
    } catch (e) {
      toast.show(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (pending) {
    return (
      <div className="flex flex-col gap-1.5 border-t border-line pt-2">
        <SafeText className="px-1.5 text-[12px]">
          {t("carryChanges", { count: changed, name: pending.name })}
        </SafeText>
        <div className="flex gap-1.5">
          <Button disabled={busy} onClick={() => void start(pending)}>
            {t("carryAndSwitch")}
          </Button>
          <Button disabled={busy} onClick={() => setPending(null)}>
            {t("cancel")}
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-0.5 border-t border-line pt-1.5">
      <PopoverGroup>{t("switchTo")}</PopoverGroup>
      {nodesQ.isPending && (
        <span className="px-1.5 text-[12px] text-muted-text">{t("branchesLoading")}</span>
      )}
      {nodesQ.isError && (
        <span className="px-1.5 text-[12px] text-bad-text" role="alert">
          {t("branchesFailed")}
        </span>
      )}
      {nodesQ.isSuccess && targets.length === 0 && (
        <span className="px-1.5 text-[12px] text-muted-text">{t("noOtherBranches")}</span>
      )}
      {targets.length > 0 && (
        <ul className="flex max-h-56 flex-col overflow-y-auto" aria-label={t("switchTo")}>
          {targets.map((n) => (
            <li key={n.url}>
              <button
                type="button"
                disabled={busy}
                className="flex w-full items-center justify-between gap-2 rounded px-1.5 py-1 text-left hover:bg-surface-2 disabled:opacity-50"
                onClick={() => (changed > 0 ? setPending(n) : void start(n))}
              >
                <SafeText className="truncate font-mono text-[12px]">{n.name}</SafeText>
                <span className="shrink-0 text-[11px] text-muted-text" aria-hidden>
                  {n.kind === "tag" ? t("tag") : n.kind === "branch" ? t("branch") : ""}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
