import { useState } from "react";
import { BadBox } from "../../../components/ui/dialog";
import { Input } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { IgnoreScope } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { useT } from "../../../lib/i18n/i18n";
import { useWcActions } from "../use-wc-actions";
import { baseName, extensionPattern, parentDir } from "../wc-model";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";
import { ActionDialog, PathList } from "./action-dialog";
import { dialogStrings } from "./dialog-strings";

/** Discard (revert) local changes after an explicit, consequence-stating confirmation (doc 01 4.6). */
export function DiscardDialog({
  paths,
  all,
  onClose,
}: {
  paths: string[];
  all: boolean;
  onClose: () => void;
}) {
  const d = useT(dialogStrings);
  const t = useT(wcStrings);
  const toast = useToast();
  const { wc, files } = useWcSession();
  const [removeUnversioned, setRemoveUnversioned] = useState(false);
  const unversioned = new Set(files.filter((f) => f.text === "unversioned").map((f) => f.path));
  const hasUnversioned = paths.some((p) => unversioned.has(p));
  const targets = removeUnversioned ? paths : paths.filter((p) => !unversioned.has(p));

  return (
    <ActionDialog
      title={d("discardTitle")}
      submitLabel={d("discardAction")}
      danger
      canSubmit={wc !== null && targets.length > 0}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc) return;
        const h = await unwrap(
          ipc().commands.revert({ wcId: wc.id, paths: targets, removeUnversioned }),
        );
        toast.show(t("opStarted", { title: h.title }));
        onClose();
      }}
    >
      <BadBox>{all ? d("discardAllBody") : d("discardBody", { count: targets.length })}</BadBox>
      <PathList paths={targets} label={d("files")} />
      {hasUnversioned && (
        <label className="flex items-center gap-2">
          <input
            type="checkbox"
            checked={removeUnversioned}
            onChange={(e) => setRemoveUnversioned(e.target.checked)}
            className="accent-[var(--accent)]"
          />
          {d("discardUnversioned")}
        </label>
      )}
    </ActionDialog>
  );
}

type IgnoreChoice = "item" | "ext" | "folder";

/** Ignore a file, an extension in its folder or the folder itself (doc 01 4.9). */
export function IgnoreDialog({ path, onClose }: { path: string; onClose: () => void }) {
  const d = useT(dialogStrings);
  const toast = useToast();
  const { wc, files } = useWcSession();
  const actions = useWcActions();
  const isDir = files.find((f) => f.path === path)?.kind === "dir";
  const dir = parentDir(path);
  const ext = isDir ? null : extensionPattern(path);
  const [choice, setChoice] = useState<IgnoreChoice>("item");
  const [scope, setScope] = useState<IgnoreScope>("svn:ignore");

  const options: { id: IgnoreChoice; label: string; pattern: string; dir: string }[] = [
    {
      id: "item",
      label: d("ignoreThisFile", { name: baseName(path) }),
      pattern: baseName(path),
      dir,
    },
  ];
  if (ext) {
    options.push({ id: "ext", label: d("ignoreExtension", { pattern: ext }), pattern: ext, dir });
  }
  if (dir !== "") {
    options.push({
      id: "folder",
      label: d("ignoreFolder", { name: baseName(dir) }),
      pattern: baseName(dir),
      dir: parentDir(dir),
    });
  }
  const picked = options.find((o) => o.id === choice) ?? (options[0] as (typeof options)[number]);

  return (
    <ActionDialog
      title={d("ignoreTitle")}
      submitLabel={d("ignoreAction")}
      canSubmit={wc !== null}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc) return;
        await unwrap(
          ipc().commands.ignoreAdd({
            wcId: wc.id,
            pattern: picked.pattern,
            dir: picked.dir,
            scope,
          }),
        );
        actions.refreshStatus();
        toast.show(d("ignoreDone"));
        onClose();
      }}
    >
      <fieldset className="m-0 flex flex-col gap-1.5 border-0 p-0">
        {options.map((o) => (
          <label key={o.id} className="flex items-center gap-2">
            <input
              type="radio"
              name="ignore-choice"
              checked={picked.id === o.id}
              onChange={() => setChoice(o.id)}
              className="accent-[var(--accent)]"
            />
            <SafeText>{o.label}</SafeText>
          </label>
        ))}
      </fieldset>
      <fieldset className="m-0 flex flex-col gap-1.5 border-0 p-0">
        <legend className="mb-1 p-0">{d("ignoreScope")}</legend>
        {(
          [
            ["svn:ignore", "scopeShared"],
            ["local", "scopeLocal"],
          ] as const
        ).map(([value, key]) => (
          <label key={value} className="flex items-center gap-2">
            <input
              type="radio"
              name="ignore-scope"
              checked={scope === value}
              onChange={() => setScope(value)}
              className="accent-[var(--accent)]"
            />
            {d(key)}
          </label>
        ))}
      </fieldset>
    </ActionDialog>
  );
}

/** A target path must stay inside the working copy: relative, no `..`, no drive or UNC prefix. */
export function isSafeRelativePath(p: string): boolean {
  if (p.trim() === "" || p.startsWith("/") || p.startsWith("\\") || /^[a-z]:/i.test(p))
    return false;
  return !p.split(/[\\/]/).some((seg) => seg === "..");
}

/** Rename or move a versioned item (`move_path`). */
export function RenameDialog({ path, onClose }: { path: string; onClose: () => void }) {
  const d = useT(dialogStrings);
  const t = useT(wcStrings);
  const toast = useToast();
  const { wc } = useWcSession();
  const [to, setTo] = useState(path);
  const next = to.trim().replaceAll("\\", "/");
  const unsafe = next !== "" && !isSafeRelativePath(next);

  return (
    <ActionDialog
      title={d("renameTitle")}
      submitLabel={d("renameAction")}
      canSubmit={wc !== null && next !== "" && next !== path && !unsafe}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc) return;
        const h = await unwrap(ipc().commands.movePath(wc.id, path, next));
        toast.show(t("opStarted", { title: h.title }));
        onClose();
      }}
    >
      <label className="flex flex-col gap-1">
        {d("newPath")}
        <Input
          value={to}
          onChange={(e) => setTo(e.target.value)}
          spellCheck={false}
          aria-invalid={unsafe}
        />
      </label>
      {unsafe && (
        <span className="text-bad-text" role="alert">
          {d("pathUnsafe")}
        </span>
      )}
    </ActionDialog>
  );
}

/** Schedule versioned files for deletion (`delete_paths`), optionally keeping them on disk. */
export function DeleteDialog({ paths, onClose }: { paths: string[]; onClose: () => void }) {
  const d = useT(dialogStrings);
  const t = useT(wcStrings);
  const toast = useToast();
  const { wc } = useWcSession();
  const [keepLocal, setKeepLocal] = useState(false);
  return (
    <ActionDialog
      title={d("deleteTitle")}
      submitLabel={d("deleteAction")}
      danger
      canSubmit={wc !== null}
      onClose={onClose}
      onSubmit={async () => {
        if (!wc) return;
        const h = await unwrap(ipc().commands.deletePaths(wc.id, paths, keepLocal));
        toast.show(t("opStarted", { title: h.title }));
        onClose();
      }}
    >
      <BadBox>{d("deleteBody", { count: paths.length })}</BadBox>
      <PathList paths={paths} label={d("files")} />
      <label className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={keepLocal}
          onChange={(e) => setKeepLocal(e.target.checked)}
          className="accent-[var(--accent)]"
        />
        {d("deleteKeepLocal")}
      </label>
    </ActionDialog>
  );
}
