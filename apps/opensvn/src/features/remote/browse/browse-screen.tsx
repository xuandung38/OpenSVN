import { MoreHorizontal } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { Button } from "../../../components/ui/button";
import { DropdownMenu } from "../../../components/ui/dropdown-menu";
import { EmptyState } from "../../../components/ui/empty-state";
import { Input } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { Tabs } from "../../../components/ui/tabs";
import { useToast } from "../../../components/ui/toast";
import type { RepoEntry } from "../../../ipc/bindings";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { BlameView } from "../blame/blame-view";
import { parseRevision } from "../branch/branch-model";
import { CreateBranchTagDialog } from "../branch/create-branch-tag-dialog";
import { HistoryScreen } from "../history/history-screen";
import type { RevisionNav } from "../history/revision-details";
import { LockDialog, type LockTarget } from "../lock/lock-dialog";
import { baseName, crumbs, parentUrl } from "../repo-url";
import { browseStrings } from "./browse-strings";
import { DirTable } from "./dir-table";
import { FilePreview } from "./file-preview";
import { type BrowseNode, type NodeActionHandlers, nodeMenuEntries } from "./node-actions";
import { RemoteEditDialog, type RemoteEditMode } from "./remote-edit-dialog";
import { RepoTree } from "./repo-tree";
import { useRepoList } from "./use-repo";

type FileTab = "preview" | "blame" | "log";

/** Where the browser should go (set by History: "Open file at this revision"). `nonce` re-triggers. */
export interface BrowseLocation {
  url: string;
  rev: number | null;
  tab?: FileTab;
  nonce: number;
}

type Dialog =
  | { kind: "edit"; mode: RemoteEditMode; targetUrl: string }
  | { kind: "lock"; mode: "lock" | "unlock"; targets: LockTarget[] }
  | { kind: "branch"; sourceUrl: string; sourceName: string };

const syntheticDir = (url: string): RepoEntry => ({
  name: baseName(url),
  kind: "dir",
  size: null,
  createdRev: 0,
  lastAuthor: null,
  date: "",
  lock: null,
});

/**
 * Repository browser (doc 01, 3.5): folder tree, directory table or file tabs (Preview/Blame/Log),
 * revision picker, and direct server operations through message dialogs.
 */
export function BrowseScreen({
  repoRootUrl,
  startUrl,
  wcId,
  layoutRepo,
  location,
  onShowLog,
  historyNav,
  onOpenRevision,
}: {
  repoRootUrl: string;
  startUrl: string;
  wcId: string | null;
  /** True when the repository follows trunk/branches/tags (enables branch/tag creation wording). */
  layoutRepo: boolean;
  location?: BrowseLocation | null;
  onShowLog: (url: string) => void;
  historyNav: RevisionNav;
  onOpenRevision: (rev: number) => void;
}) {
  const t = useT(browseStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const [rev, setRev] = useState<number | null>(null);
  const [revText, setRevText] = useState("");
  const [dirUrl, setDirUrl] = useState(startUrl);
  const [selected, setSelected] = useState<BrowseNode | null>(null);
  const [fileTab, setFileTab] = useState<FileTab>("preview");
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [pendingSelect, setPendingSelect] = useState<string | null>(null);

  const list = useRepoList(dirUrl, rev);
  const entries = list.data ?? [];

  // Jump requested from another tab.
  useEffect(() => {
    if (!location) return;
    setRev(location.rev);
    setRevText(location.rev === null ? "" : String(location.rev));
    setDirUrl(parentUrl(location.url, repoRootUrl));
    setPendingSelect(location.url);
    setFileTab(location.tab ?? "preview");
  }, [location, repoRootUrl]);

  // Resolve the pending selection once the parent listing is available.
  useEffect(() => {
    if (!pendingSelect || !list.data) return;
    const name = baseName(pendingSelect);
    const entry = list.data.find((e) => e.name === name);
    if (entry) setSelected({ url: pendingSelect, entry });
    setPendingSelect(null);
  }, [pendingSelect, list.data]);

  const revInvalid = parseRevision(revText) === "invalid";
  const filePath = selected?.entry.kind === "file" ? selected : null;
  const current: BrowseNode = selected ?? { url: dirUrl, entry: syntheticDir(dirUrl) };

  const handlers: NodeActionHandlers = useMemo(
    () => ({
      open: (n) => {
        if (n.entry.kind === "dir") {
          setDirUrl(n.url);
          setSelected(null);
        } else {
          setSelected(n);
          setFileTab("preview");
        }
      },
      showLog: (n) => onShowLog(n.url),
      blame: (n) => {
        setSelected(n);
        setFileTab("blame");
      },
      copyUrl: (n) => {
        navigator.clipboard
          .writeText(n.url)
          .then(() => toast.show(t("copied")))
          .catch((e: unknown) => toast.show(String(e)));
      },
      lock: (n) =>
        setDialog({ kind: "lock", mode: "lock", targets: [{ url: n.url, lock: n.entry.lock }] }),
      unlock: (n) =>
        setDialog({ kind: "lock", mode: "unlock", targets: [{ url: n.url, lock: n.entry.lock }] }),
      newFolder: (n) =>
        setDialog({
          kind: "edit",
          mode: "mkdir",
          targetUrl: n.entry.kind === "dir" ? n.url : parentUrl(n.url, repoRootUrl),
        }),
      rename: (n) => setDialog({ kind: "edit", mode: "rename", targetUrl: n.url }),
      remove: (n) => setDialog({ kind: "edit", mode: "delete", targetUrl: n.url }),
      createBranchTag: (n) =>
        setDialog({ kind: "branch", sourceUrl: n.url, sourceName: n.entry.name }),
    }),
    [onShowLog, repoRootUrl, t, toast],
  );

  const applyRev = () => {
    const p = parseRevision(revText);
    if (p !== "invalid") setRev(p);
  };

  const breadcrumb = crumbs(filePath?.url ?? dirUrl, repoRootUrl);

  return (
    <div className="grid h-full min-h-0 grid-cols-[260px_1fr]">
      <section aria-label={t("folders")} className="flex min-h-0 flex-col border-r border-line">
        <RepoTree
          label={t("folders")}
          rootUrl={repoRootUrl}
          rev={rev}
          selectedUrl={dirUrl}
          onSelect={(u) => {
            setDirUrl(u);
            setSelected(null);
          }}
          className="min-h-0 flex-1"
        />
      </section>
      <section aria-label={t("title")} className="flex min-h-0 min-w-0 flex-col">
        <div className="flex items-center gap-2 border-b border-line px-3 py-1.5">
          <nav aria-label="Breadcrumb" className="min-w-0 flex-1">
            <ol className="m-0 flex list-none flex-wrap items-center gap-1 p-0">
              {breadcrumb.map((b, i) => (
                <li key={b.url} className="flex items-center gap-1">
                  {i > 0 && <span aria-hidden="true">/</span>}
                  <button
                    type="button"
                    onClick={() => {
                      setDirUrl(b.url);
                      setSelected(null);
                    }}
                    aria-current={i === breadcrumb.length - 1 ? "page" : undefined}
                    className="cursor-pointer border-0 bg-transparent p-0 text-accent-text hover:underline aria-[current=page]:font-semibold aria-[current=page]:text-fg"
                  >
                    <SafeText>{b.label}</SafeText>
                  </button>
                </li>
              ))}
            </ol>
          </nav>
          <form
            className="flex items-center gap-1.5"
            onSubmit={(e) => {
              e.preventDefault();
              applyRev();
            }}
          >
            <label className="flex items-center gap-1.5 text-muted-text">
              {t("revision")}
              <Input
                className="w-24"
                value={revText}
                placeholder="HEAD"
                title={t("revisionHint")}
                aria-invalid={revInvalid}
                onChange={(e) => setRevText(e.target.value)}
              />
            </label>
            <Button type="submit" small disabled={revInvalid}>
              {t("go")}
            </Button>
          </form>
          <DropdownMenu
            trigger={
              <Button small aria-label={t("actions")}>
                <MoreHorizontal size={14} />
              </Button>
            }
            entries={nodeMenuEntries(current, handlers, t)}
          />
        </div>
        {revInvalid && (
          <p className="m-0 px-3 py-1 text-bad-text" role="alert">
            {t("revisionInvalid")}
          </p>
        )}
        <div className="min-h-0 flex-1">
          {filePath ? (
            <Tabs
              label={filePath.entry.name}
              className="h-full"
              value={fileTab}
              onValueChange={(v) => setFileTab(v as FileTab)}
              tabs={[
                { id: "preview", label: t("tabPreview") },
                { id: "blame", label: t("tabBlame") },
                { id: "log", label: t("tabLog") },
              ]}
              panels={{
                preview: <FilePreview url={filePath.url} rev={rev} />,
                blame: <BlameView url={filePath.url} rev={rev} onOpenRevision={onOpenRevision} />,
                log: (
                  <HistoryScreen
                    target={{ kind: "url", url: filePath.url }}
                    repoRootUrl={repoRootUrl}
                    scopeLabel={null}
                    nav={historyNav}
                  />
                ),
              }}
            />
          ) : list.isPending ? (
            <p className="m-0 p-3 text-muted-text">{c("loading")}</p>
          ) : list.error ? (
            <EmptyState
              title={t("loadError")}
              description={list.error.message}
              actions={<Button onClick={() => list.refetch()}>{c("retry")}</Button>}
            />
          ) : (
            <DirTable
              dirUrl={dirUrl}
              entries={entries}
              selectedUrl={selected?.url ?? null}
              handlers={handlers}
              onSelect={setSelected}
            />
          )}
        </div>
      </section>
      {dialog?.kind === "edit" && (
        <RemoteEditDialog
          mode={dialog.mode}
          targetUrl={dialog.targetUrl}
          repoRoot={repoRootUrl}
          onClose={() => setDialog(null)}
        />
      )}
      {dialog?.kind === "lock" && (
        <LockDialog mode={dialog.mode} targets={dialog.targets} onClose={() => setDialog(null)} />
      )}
      {dialog?.kind === "branch" && (
        <CreateBranchTagDialog
          kind={layoutRepo ? "branch" : "location"}
          sourceUrl={dialog.sourceUrl}
          sourceName={dialog.sourceName}
          repoRootUrl={repoRootUrl}
          wcId={wcId}
          onClose={() => setDialog(null)}
        />
      )}
    </div>
  );
}
