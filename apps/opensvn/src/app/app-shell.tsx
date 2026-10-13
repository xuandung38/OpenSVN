import { useMemo, useState } from "react";
import { ActivityPanel } from "../components/layout/activity-panel";
import { AppToolbar } from "../components/layout/app-toolbar";
import { MainWindow } from "../components/layout/main-window";
import { Button } from "../components/ui/button";
import { EmptyState } from "../components/ui/empty-state";
import { Popover, PopoverGroup } from "../components/ui/popover";
import { SegmentedControl } from "../components/ui/segmented-control";
import { StatusBar, StatusBarItem } from "../components/ui/status-bar";
import { Tabs } from "../components/ui/tabs";
import { BranchPopoverContent } from "../features/remote/branch/branch-popover";
import { CreateBranchTagDialog } from "../features/remote/branch/create-branch-tag-dialog";
import {
  RemoteBrowse,
  RemoteHistory,
  type RemoteView,
  useRemoteLinks,
} from "../features/remote/remote-pane";
import { SettingsSheet } from "../features/remote/settings/settings-sheet";
import { WcSidebar } from "../features/wc/changes/wc-sidebar";
import { WcDialogHost } from "../features/wc/dialogs/wc-dialog-host";
import { DiffView } from "../features/wc/diff/diff-view";
import { useOpNotifications } from "../features/wc/use-op-notifications";
import { useWcActions } from "../features/wc/use-wc-actions";
import { useWcShortcuts } from "../features/wc/use-wc-shortcuts";
import { WcBanners } from "../features/wc/wc-banners";
import { changedFiles, wcPathUrl } from "../features/wc/wc-model";
import { useWcSession, type WcLinks, WcSessionProvider } from "../features/wc/wc-session";
import { wcStrings } from "../features/wc/wc-strings";
import {
  DropToAddWorkingCopy,
  useAddExistingWorkingCopy,
  WelcomeScreen,
} from "../features/wc/welcome/welcome-screen";
import type { FileStatus, WorkingCopy } from "../ipc/bindings";
import { useOperations, useStatus, useWorkingCopies } from "../ipc/queries";
import { useT } from "../lib/i18n/i18n";
import { applyTheme, readTheme, type ThemeChoice } from "../lib/theme";
import { shellStrings } from "./shell-strings";

type View = "changes" | RemoteView;
const NO_FILES: readonly FileStatus[] = [];

/**
 * Main window wired to the IPC client (real bindings in Tauri, mock adapter in a browser). Loads the
 * working copies and the status of the current one, then hands them to the working-copy session.
 */
export function AppShell() {
  const wcs = useWorkingCopies();
  const [wcId, setWcId] = useState<string | null>(null);
  const current = wcs.data?.find((w) => w.id === wcId) ?? wcs.data?.[0] ?? null;
  const status = useStatus(current?.id ?? null);
  const [view, setView] = useState<View>("changes");
  const links = useRemoteLinks(setView);

  const sessionLinks = useMemo<WcLinks>(
    () => ({
      showLog: (path) => {
        if (current) links.showLogOf(wcPathUrl(current.url, path));
      },
      blame: (path) => {
        if (current) links.nav.blameAt?.(wcPathUrl(current.url, path), current.baseRev);
      },
    }),
    [current, links],
  );

  if (wcs.isError) {
    return (
      <EmptyState
        title="Cannot load working copies"
        description={wcs.error instanceof Error ? wcs.error.message : "Unknown error"}
        actions={<Button onClick={() => wcs.refetch()}>Retry</Button>}
      />
    );
  }

  return (
    <WcSessionProvider wc={current} files={status.data ?? NO_FILES} links={sessionLinks}>
      <ShellBody
        workingCopies={wcs.data ?? []}
        loaded={wcs.isSuccess}
        onSelectWc={setWcId}
        view={view}
        onView={setView}
        links={links}
      />
    </WcSessionProvider>
  );
}

function ShellBody({
  workingCopies,
  loaded,
  onSelectWc,
  view,
  onView,
  links,
}: {
  workingCopies: WorkingCopy[];
  loaded: boolean;
  onSelectWc: (id: string) => void;
  view: View;
  onView: (v: View) => void;
  links: ReturnType<typeof useRemoteLinks>;
}) {
  const { wc: current, files, openDialog } = useWcSession();
  const ops = useOperations();
  const actions = useWcActions();
  const addExisting = useAddExistingWorkingCopy();
  const [showActivity, setShowActivity] = useState(true);
  const sh = useT(shellStrings);
  const t = useT(wcStrings);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [createKind, setCreateKind] = useState<"branch" | "tag" | "location" | null>(null);
  const [theme, setTheme] = useState<ThemeChoice>(readTheme);

  useOpNotifications();
  useWcShortcuts({
    onView,
    onToggleActivity: () => setShowActivity((v) => !v),
    onAddExisting: () => void addExisting(),
  });

  const changed = changedFiles(files).length;
  const running = ops.filter((o) => o.state === "running").length;

  const wcPicker = (item: React.ReactNode) => (
    <Popover trigger={item}>
      <PopoverGroup>{sh("workingCopies")}</PopoverGroup>
      {workingCopies.map((w) => (
        <button
          key={w.id}
          type="button"
          onClick={() => onSelectWc(w.id)}
          className={`flex cursor-pointer justify-between rounded-[5px] border-0 bg-transparent p-1.5 text-left hover:bg-sel ${
            w.id === current?.id ? "font-semibold" : ""
          }`}
        >
          <span>{w.name}</span>
          <span className="font-mono text-[11px] text-muted-text">r{w.baseRev}</span>
        </button>
      ))}
      <div className="mt-1 flex flex-col gap-1 border-t border-line pt-1.5">
        <Button small onClick={() => openDialog({ kind: "checkout" })}>
          {t("checkoutAction")}
        </Button>
        <Button small onClick={() => void addExisting()}>
          {t("addExistingAction")}
        </Button>
      </div>
    </Popover>
  );

  const noWc = <EmptyState title={sh("noWorkingCopy")} description={sh("noWorkingCopyHint")} />;

  return (
    <>
      <DropToAddWorkingCopy />
      <MainWindow
        toolbar={
          <AppToolbar
            workingCopyName={current?.name ?? sh("noWorkingCopy")}
            locationLabel={current?.branch ? "Branch" : "Location"}
            locationValue={current?.branch?.name ?? current?.pathInRepo ?? "-"}
            incoming={current?.incoming ?? 0}
            updating={ops.some((o) => o.handle.kind === "update" && o.state === "running")}
            activityCount={running}
            onUpdate={() => void actions.update(null)}
            onToggleActivity={() => setShowActivity((v) => !v)}
            wrapWorkingCopy={wcPicker}
            onOpenSettings={() => setSettingsOpen(true)}
            wrapLocation={
              current
                ? (item) => (
                    <Popover trigger={item}>
                      <BranchPopoverContent wc={current} onCreate={setCreateKind} />
                      <div className="mt-1 border-t border-line pt-1.5">
                        <Button small onClick={() => openDialog({ kind: "switch" })}>
                          {t("switchAction")}
                        </Button>
                      </div>
                    </Popover>
                  )
                : undefined
            }
          />
        }
        banner={<WcBanners />}
        sidebar={<WcSidebar />}
        main={
          loaded && workingCopies.length === 0 ? (
            <WelcomeScreen />
          ) : (
            <>
              <Tabs
                label={sh("views")}
                className="h-full"
                keepMounted
                value={view}
                onValueChange={(v) => onView(v as View)}
                tabs={[
                  { id: "changes", label: sh("changes") },
                  { id: "history", label: sh("history") },
                  { id: "browse", label: sh("browse") },
                ]}
                panels={{
                  changes: current ? <DiffView /> : noWc,
                  history: current ? <RemoteHistory wc={current} links={links} /> : noWc,
                  browse: current ? <RemoteBrowse wc={current} links={links} /> : noWc,
                }}
              />
              <SettingsSheet open={settingsOpen} onClose={() => setSettingsOpen(false)} />
              {createKind && current && (
                <CreateBranchTagDialog
                  kind={createKind}
                  sourceUrl={current.url}
                  sourceName={current.branch?.name ?? current.url}
                  repoRootUrl={current.repoRootUrl}
                  repoUuid={current.repoUuid}
                  currentRefUrl={current.branch?.url ?? null}
                  wcId={current.id}
                  onClose={() => setCreateKind(null)}
                />
              )}
            </>
          )
        }
        activity={
          showActivity ? (
            <ActivityPanel
              ops={ops}
              onCancel={(opId) => void actions.cancelOp(opId)}
              cancelLabel={t("activityCancel")}
            />
          ) : undefined
        }
        statusBar={
          <StatusBar>
            <StatusBarItem tone="ok">Connected</StatusBarItem>
            <StatusBarItem>{current ? `r${current.baseRev}` : "-"}</StatusBarItem>
            <StatusBarItem>{changed} changed</StatusBarItem>
            <span className="flex-1" />
            <SegmentedControl<ThemeChoice>
              label="Theme"
              value={theme}
              onChange={(next) => {
                setTheme(next);
                applyTheme(next);
              }}
              options={[
                { value: "system", label: "System" },
                { value: "light", label: "Light" },
                { value: "dark", label: "Dark" },
              ]}
            />
          </StatusBar>
        }
      />
      <WcDialogHost />
    </>
  );
}
