import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { Input, Textarea } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { Layout } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { useT } from "../../../lib/i18n/i18n";
import { ActionDialog } from "../dialogs/action-dialog";
import { useWcSession } from "../wc-session";
import { describeLayout, folderLines, layoutProblem, normalizeFolder } from "./layout-model";
import { propsStrings } from "./props-strings";

type Mode = Layout["mode"];

/**
 * Where this repository keeps trunk, branches and tags (doc 01 section 5, "Repository layout…"),
 * stored per repository through `layout_set`. Auto keeps detecting; custom takes folder patterns
 * (`{project}` stands for any one folder); none treats the repository as plain folders. What
 * detection finds is offered as a starting point. Saving refreshes the working copy's branch.
 */
export function LayoutDialog({ onClose }: { onClose: () => void }) {
  const t = useT(propsStrings);
  const toast = useToast();
  const qc = useQueryClient();
  const { wc } = useWcSession();
  const repoUuid = wc?.repoUuid ?? "";
  const stored = useQuery({
    queryKey: queryKeys.layout(repoUuid),
    enabled: wc !== null,
    queryFn: () => unwrap(ipc().commands.layoutGet(repoUuid)),
  });
  // Unattended on the backend (never prompts); a failure just offers nothing.
  const detected = useQuery({
    queryKey: ["layout-detect", wc?.repoRootUrl ?? "", wc?.url ?? ""],
    enabled: wc !== null,
    retry: false,
    queryFn: () => unwrap(ipc().commands.layoutDetect(wc?.repoRootUrl ?? "", wc?.url ?? "")),
  });

  const [mode, setMode] = useState<Mode>("auto");
  const [trunk, setTrunk] = useState("/trunk");
  const [branches, setBranches] = useState("/branches");
  const [tags, setTags] = useState("/tags");
  const [loaded, setLoaded] = useState(false);
  const fill = (l: Layout) => {
    setMode(l.mode);
    if (l.mode === "custom") {
      setTrunk(l.trunk);
      setBranches(l.branches.join("\n"));
      setTags(l.tags.join("\n"));
    }
  };
  if (stored.data && !loaded) {
    // Adjusting state while rendering: the form starts from the stored layout once it is read.
    setLoaded(true);
    fill(stored.data);
  }

  const layout: Layout =
    mode === "custom"
      ? {
          mode,
          trunk: normalizeFolder(trunk),
          branches: folderLines(branches),
          tags: folderLines(tags),
        }
      : { mode };
  const problem = layoutProblem(layout);
  const found = detected.data?.found ? detected.data.layout : null;

  return (
    <ActionDialog
      title={t("layoutTitle")}
      submitLabel={t("layoutSave")}
      canSubmit={wc !== null && stored.isSuccess && problem === null}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc) return;
        await unwrap(ipc().commands.layoutSet(repoUuid, layout));
        await qc.invalidateQueries({ queryKey: queryKeys.layout(repoUuid) });
        void qc.invalidateQueries({ queryKey: ["branches"] });
        void qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
        toast.show(t("layoutSaved"));
        onClose();
      }}
    >
      <p className="m-0">{t("layoutBody")}</p>
      <fieldset className="m-0 flex flex-col gap-1 border-0 p-0">
        <legend className="mb-1 font-semibold">{t("layoutMode")}</legend>
        {(
          [
            ["auto", "layoutAuto"],
            ["custom", "layoutCustom"],
            ["none", "layoutNone"],
          ] as const
        ).map(([m, key]) => (
          <label key={m} className="flex items-center gap-2">
            <input
              type="radio"
              name="layout-mode"
              value={m}
              checked={mode === m}
              onChange={() => setMode(m)}
              className="accent-[var(--accent)]"
            />
            {t(key)}
          </label>
        ))}
      </fieldset>
      {found && (
        <div className="flex items-center gap-2 text-[12px]">
          <SafeText as="span" className="min-w-0 flex-1 truncate text-muted-text">
            {t("layoutDetected", { layout: describeLayout(found) })}
          </SafeText>
          <Button small onClick={() => fill(found)}>
            {t("layoutUseDetected")}
          </Button>
        </div>
      )}
      {mode === "custom" && (
        <>
          <label className="flex flex-col gap-1">
            {t("layoutTrunk")}
            <Input
              value={trunk}
              spellCheck={false}
              className="font-mono"
              aria-invalid={problem === "layoutTrunkRequired"}
              onChange={(e) => setTrunk(e.target.value)}
            />
          </label>
          <label className="flex flex-col gap-1">
            {t("layoutBranches")}
            <Textarea
              rows={2}
              value={branches}
              spellCheck={false}
              className="font-mono"
              onChange={(e) => setBranches(e.target.value)}
            />
          </label>
          <label className="flex flex-col gap-1">
            {t("layoutTags")}
            <Textarea
              rows={2}
              value={tags}
              spellCheck={false}
              className="font-mono"
              onChange={(e) => setTags(e.target.value)}
            />
          </label>
          <span className="text-[12px] text-muted-text">{t("layoutPatternHint")}</span>
          {problem && (
            <span role="alert" className="text-bad-text">
              {t(problem)}
            </span>
          )}
        </>
      )}
    </ActionDialog>
  );
}
