import type { MenuEntries } from "../../../components/ui/dropdown-menu";
import type { RepoEntry } from "../../../ipc/bindings";
import type { Vars } from "../../../lib/i18n/i18n";
import type { browseStrings } from "./browse-strings";

type Key = keyof (typeof browseStrings)["en"];
export type BrowseT = (key: Key, vars?: Vars) => string;

export interface BrowseNode {
  url: string;
  entry: RepoEntry;
}

export interface NodeActionHandlers {
  open: (n: BrowseNode) => void;
  showLog: (n: BrowseNode) => void;
  blame: (n: BrowseNode) => void;
  copyUrl: (n: BrowseNode) => void;
  lock: (n: BrowseNode) => void;
  unlock: (n: BrowseNode) => void;
  newFolder: (n: BrowseNode) => void;
  rename: (n: BrowseNode) => void;
  remove: (n: BrowseNode) => void;
  createBranchTag: (n: BrowseNode) => void;
  download: (n: BrowseNode) => void;
  upload: (n: BrowseNode) => void;
}

/**
 * Entries of the node menu (doc 01, 6.4). The same list feeds the right-click menu and the
 * keyboard-reachable "Actions" dropdown. Download and Upload go through the native file picker,
 * which hands out a path grant (the WebView never sees the path).
 */
export function nodeMenuEntries(n: BrowseNode, h: NodeActionHandlers, t: BrowseT): MenuEntries {
  const isFile = n.entry.kind === "file";
  const lock = n.entry.lock;
  const mine = lock?.mine === true;
  const entries: MenuEntries = [
    { label: t("open"), onSelect: () => h.open(n) },
    { label: t("showLog"), onSelect: () => h.showLog(n) },
  ];
  if (isFile) entries.push({ label: t("blame"), onSelect: () => h.blame(n) });
  entries.push({ label: t("copyUrl"), onSelect: () => h.copyUrl(n) });
  if (isFile) {
    entries.push("separator");
    entries.push(
      lock && mine
        ? { label: t("unlock"), onSelect: () => h.unlock(n) }
        : lock
          ? { label: t("unlock"), onSelect: () => h.unlock(n) }
          : { label: t("lock"), onSelect: () => h.lock(n) },
    );
    entries.push({ label: t("download"), onSelect: () => h.download(n) });
  }
  entries.push("separator");
  entries.push({ label: t("newFolder"), onSelect: () => h.newFolder(n) });
  if (!isFile) entries.push({ label: t("upload"), onSelect: () => h.upload(n) });
  entries.push({ label: t("renameAction"), onSelect: () => h.rename(n) });
  entries.push({ label: t("deleteAction"), onSelect: () => h.remove(n) });
  entries.push("separator");
  entries.push({ label: t("createBranchTag"), onSelect: () => h.createBranchTag(n) });
  return entries;
}
