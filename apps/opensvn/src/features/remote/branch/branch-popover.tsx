import { Button } from "../../../components/ui/button";
import { PopoverGroup } from "../../../components/ui/popover";
import { SafeText } from "../../../components/ui/safe-text";
import type { WorkingCopy } from "../../../ipc/bindings";
import { useT } from "../../../lib/i18n/i18n";
import { repoPath } from "../repo-url";
import { branchStrings } from "./branch-strings";

/**
 * Contents of the Branch/Location popover for the actions owned by the remote screens: creating a
 * branch/tag (repo with layout) or copying to a new location (folder-style repo). Switching is part
 * of the working-copy flows and is added there.
 */
export function BranchPopoverContent({
  wc,
  onCreate,
}: {
  wc: WorkingCopy;
  onCreate: (kind: "branch" | "tag" | "location") => void;
}) {
  const t = useT(branchStrings);
  return (
    <>
      <PopoverGroup>{wc.branch ? wc.branch.name : t("destination")}</PopoverGroup>
      <SafeText className="px-1.5 font-mono text-[12px] text-muted-text">
        {repoPath(wc.url, wc.repoRootUrl)}
      </SafeText>
      <div className="flex gap-1.5 border-t border-line pt-2">
        {wc.branch ? (
          <>
            <Button onClick={() => onCreate("branch")}>{t("newBranch")}…</Button>
            <Button onClick={() => onCreate("tag")}>{t("newTag")}…</Button>
          </>
        ) : (
          <Button onClick={() => onCreate("location")}>{t("copyToLocation")}…</Button>
        )}
      </div>
    </>
  );
}
