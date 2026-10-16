import { useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { Select } from "../../../components/ui/fields";
import { useToast } from "../../../components/ui/toast";
import type { Depth, SparseDepth } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { useT } from "../../../lib/i18n/i18n";
import { ActionDialog } from "../dialogs/action-dialog";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";
import { FolderDepthTree } from "./folder-depth-tree";
import { applyEntries, choicesFromWc, choose, prune } from "./sparse-model";
import { sparseStrings } from "./sparse-strings";

const ROOT_DEPTHS: {
  value: Depth;
  label: "depthInfinity" | "depthImmediates" | "depthFiles" | "depthEmpty";
}[] = [
  { value: "infinity", label: "depthInfinity" },
  { value: "immediates", label: "depthImmediates" },
  { value: "files", label: "depthFiles" },
  { value: "empty", label: "depthEmpty" },
];

/**
 * "Choose folders" for the open working copy (`menu://wc/sparse`): the same folder tree as the
 * checkout dialog, starting from the depths the working copy records (`wc_depths`). Only what
 * changes is sent, as one `wc_sparse_apply` operation.
 */
export function SparseDialog({ onClose }: { onClose: () => void }) {
  const t = useT(sparseStrings);
  const w = useT(wcStrings);
  const toast = useToast();
  const { wc } = useWcSession();
  const depths = useQuery({
    queryKey: ["wc-depths", wc?.id ?? ""],
    enabled: wc !== null,
    gcTime: 0,
    queryFn: () => unwrap(ipc().commands.wcDepths(wc?.id as string)),
  });
  const before = useMemo(() => (depths.data ? choicesFromWc(depths.data) : null), [depths.data]);
  const [edited, setEdited] = useState<{
    rootDepth: Depth;
    choices: ReadonlyMap<string, SparseDepth>;
  } | null>(null);
  const after = edited ?? before;
  const entries = before && after ? applyEntries(before, after) : [];

  return (
    <ActionDialog
      title={t("dialogTitle")}
      submitLabel={t("apply")}
      canSubmit={wc !== null && entries.length > 0}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc) return;
        const h = await unwrap(ipc().commands.wcSparseApply(wc.id, entries));
        toast.show(w("opStarted", { title: h.title }));
        onClose();
      }}
    >
      <p className="m-0">{t("dialogHint")}</p>
      {depths.error ? (
        <p className="m-0 text-bad-text" role="alert">
          {toAppError(depths.error).message}
        </p>
      ) : !wc || !after ? (
        <p className="m-0 text-muted-text">{t("loadingWc")}</p>
      ) : (
        <>
          <label className="flex flex-col gap-1">
            {t("rootDepth")}
            <Select
              value={after.rootDepth}
              onChange={(e) => {
                const rootDepth = e.target.value as Depth;
                setEdited({ rootDepth, choices: prune(new Map(after.choices), rootDepth) });
              }}
            >
              {ROOT_DEPTHS.map((o) => (
                <option key={o.value} value={o.value}>
                  {t(o.label)}
                </option>
              ))}
            </Select>
          </label>
          <FolderDepthTree
            rootUrl={wc.url}
            rev={null}
            rootDepth={after.rootDepth}
            choices={after.choices}
            onChoose={(path, value) =>
              setEdited({
                rootDepth: after.rootDepth,
                choices: choose(after.choices, after.rootDepth, path, value),
              })
            }
            className="max-h-80"
          />
          <p className="m-0 text-[12px] text-muted-text" aria-live="polite">
            {entries.length === 0 ? t("nothingToApply") : t("willApply", { n: entries.length })}
          </p>
        </>
      )}
    </ActionDialog>
  );
}
