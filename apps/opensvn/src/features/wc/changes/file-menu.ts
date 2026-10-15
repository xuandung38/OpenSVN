import type { MenuEntries } from "../../../components/ui/menu-entries";
import type { FileStatus } from "../../../ipc/bindings";
import { type ShortcutId, shortcutLabel } from "../shortcuts";
import type { WcActions } from "../use-wc-actions";
import { isConflicted } from "../wc-model";
import type { WcSession } from "../wc-session";
import type { wcStrings } from "../wc-strings";

type T = (key: keyof (typeof wcStrings)["en"]) => string;

/**
 * Context menu / "File actions" menu of doc 01 section 6.1 for a set of target files. Entries that do
 * not apply to every target are left out, so the menu never offers an action that would be refused.
 */
export function fileMenuEntries(
  targets: readonly FileStatus[],
  session: Pick<WcSession, "openDialog" | "links">,
  actions: WcActions,
  t: T,
): MenuEntries {
  if (targets.length === 0) return [];
  const paths = targets.map((f) => f.path);
  const first = targets[0] as FileStatus;
  const all = (pred: (f: FileStatus) => boolean) => targets.every(pred);
  const versioned = all((f) => f.text !== "unversioned" && f.text !== "ignored");
  const single = targets.length === 1;
  const sc = (id: ShortcutId) => shortcutLabel(id);

  const entries: MenuEntries = [];
  if (all((f) => f.text === "unversioned")) {
    entries.push({ label: t("add"), onSelect: () => void actions.add(paths) });
    if (single) {
      entries.push({
        label: t("ignore"),
        onSelect: () => session.openDialog({ kind: "ignore", path: first.path }),
      });
    }
  }
  if (all((f) => f.text !== "unversioned" && f.text !== "normal")) {
    entries.push({
      label: t("discard"),
      shortcut: sc("discard"),
      onSelect: () => session.openDialog({ kind: "discard", paths, all: false }),
    });
  }
  if (versioned) {
    entries.push({
      label: t("addToChangelist"),
      onSelect: () => session.openDialog({ kind: "changelist", paths }),
    });
    entries.push("separator");
    entries.push({
      label: t("lock"),
      shortcut: sc("lock"),
      onSelect: () => session.openDialog({ kind: "lock", mode: "lock", paths }),
    });
    entries.push({
      label: t("unlock"),
      disabled: !targets.some((f) => f.lock),
      onSelect: () => session.openDialog({ kind: "lock", mode: "unlock", paths }),
    });
  }
  if (single && isConflicted(first)) {
    entries.push({
      label: t("resolve"),
      onSelect: () => session.openDialog({ kind: "resolver", path: first.path }),
    });
  }
  entries.push("separator");
  if (single && versioned) {
    entries.push({
      label: t("renameMove"),
      onSelect: () => session.openDialog({ kind: "rename", path: first.path }),
    });
  }
  if (versioned) {
    entries.push({
      label: t("deleteAction"),
      onSelect: () => session.openDialog({ kind: "delete", paths }),
    });
  }
  if (versioned && all((f) => f.text !== "missing" && f.text !== "deleted")) {
    entries.push({
      label: t("properties"),
      onSelect: () => session.openDialog({ kind: "properties", paths }),
    });
  }
  if (single) {
    entries.push("separator");
    if (session.links.showLog && versioned) {
      const showLog = session.links.showLog;
      entries.push({
        label: t("showLog"),
        shortcut: sc("showLog"),
        onSelect: () => showLog(first.path),
      });
    }
    if (session.links.blame && versioned && !first.binary) {
      const blame = session.links.blame;
      entries.push({ label: t("blame"), shortcut: sc("blame"), onSelect: () => blame(first.path) });
    }
    entries.push({ label: t("showInFolder"), onSelect: () => void actions.reveal(first.path) });
    entries.push({
      label: t("openDefault"),
      disabled: first.text === "missing",
      onSelect: () => void actions.openWith(first.path, "default"),
    });
    entries.push({
      label: t("openEditor"),
      disabled: first.text === "missing",
      onSelect: () => void actions.openWith(first.path, "editor"),
    });
    entries.push({
      label: t("copyPath"),
      onSelect: () => void actions.copyPath(first.path, false),
    });
    entries.push({
      label: t("copyRelativePath"),
      onSelect: () => void actions.copyPath(first.path, true),
    });
  }
  return trimSeparators(entries);
}

/** Drops leading, trailing and doubled separators left behind by omitted groups. */
function trimSeparators(entries: MenuEntries): MenuEntries {
  const out: MenuEntries = [];
  for (const e of entries) {
    if (e === "separator" && (out.length === 0 || out[out.length - 1] === "separator")) continue;
    out.push(e);
  }
  while (out[out.length - 1] === "separator") out.pop();
  return out;
}
