import { useState } from "react";
import { ActivityPanel } from "../components/layout/activity-panel";
import { AppToolbar } from "../components/layout/app-toolbar";
import { ChangesSidebar } from "../components/layout/changes-sidebar";
import { MainWindow } from "../components/layout/main-window";
import { Banner } from "../components/ui/banner";
import { Button } from "../components/ui/button";
import { EmptyState } from "../components/ui/empty-state";
import { Popover, PopoverGroup } from "../components/ui/popover";
import { SegmentedControl } from "../components/ui/segmented-control";
import { StatusBar, StatusBarItem } from "../components/ui/status-bar";
import { useToast } from "../components/ui/toast";
import { useLog, useOperations, useStatus, useUpdate, useWorkingCopies } from "../ipc/queries";
import { applyTheme, readTheme, type ThemeChoice } from "../lib/theme";

/** Main window wired to the IPC client (real bindings in Tauri, mock adapter in a browser). */
export function AppShell() {
  const wcs = useWorkingCopies();
  const [wcId, setWcId] = useState<string | null>(null);
  const current = wcs.data?.find((w) => w.id === wcId) ?? wcs.data?.[0] ?? null;
  const status = useStatus(current?.id ?? null);
  const log = useLog(current?.id ?? null);
  const ops = useOperations();
  const update = useUpdate();
  const toast = useToast();
  const [showActivity, setShowActivity] = useState(true);
  const [theme, setTheme] = useState<ThemeChoice>(readTheme);

  const files = status.data ?? [];
  const conflicts = files.filter((f) => f.text === "conflicted").length;
  const running = ops.filter((o) => o.state === "running").length;

  if (wcs.isError) {
    return (
      <EmptyState
        title="Cannot load working copies"
        description={wcs.error instanceof Error ? wcs.error.message : "Unknown error"}
        actions={<Button onClick={() => wcs.refetch()}>Retry</Button>}
      />
    );
  }

  const wcPicker = (item: React.ReactNode) => (
    <Popover trigger={<span className="contents">{item}</span>}>
      <PopoverGroup>Working copies</PopoverGroup>
      {(wcs.data ?? []).map((w) => (
        <button
          key={w.id}
          type="button"
          onClick={() => setWcId(w.id)}
          className={`flex cursor-pointer justify-between rounded-[5px] border-0 bg-transparent p-1.5 text-left hover:bg-sel ${
            w.id === current?.id ? "font-semibold" : ""
          }`}
        >
          <span>{w.name}</span>
          <span className="font-mono text-[11px] text-muted">r{w.baseRev}</span>
        </button>
      ))}
    </Popover>
  );

  return (
    <MainWindow
      toolbar={
        <AppToolbar
          workingCopyName={current?.name ?? "No working copy"}
          locationLabel={current?.branch ? "Branch" : "Location"}
          locationValue={current?.branch?.name ?? current?.pathInRepo ?? "-"}
          incoming={current?.incoming ?? 0}
          updating={
            update.isPending || ops.some((o) => o.handle.kind === "update" && o.state === "running")
          }
          activityCount={running}
          onUpdate={() =>
            current &&
            update.mutate(
              { wcId: current.id, paths: null, rev: null, depth: null },
              { onError: (e) => toast.show(e.message) },
            )
          }
          onToggleActivity={() => setShowActivity((v) => !v)}
          wrapWorkingCopy={wcPicker}
        />
      }
      banner={
        conflicts > 0 ? (
          <Banner
            title={`${conflicts} conflict${conflicts > 1 ? "s" : ""}`}
            actions={<Button small>Resolve</Button>}
          >
            Files with conflicts cannot be committed until resolved.
          </Banner>
        ) : undefined
      }
      sidebar={
        <ChangesSidebar
          files={files}
          log={log.data?.entries ?? []}
          incomingCount={current?.incoming ?? 0}
          onCommit={(paths) => toast.show(`Commit of ${paths.length} file(s) requested`)}
        />
      }
      main={
        <EmptyState
          title="Select a file"
          description="Pick a changed file in the sidebar to see its diff here."
        />
      }
      activity={showActivity ? <ActivityPanel ops={ops} /> : undefined}
      statusBar={
        <StatusBar>
          <StatusBarItem tone="ok">Connected</StatusBarItem>
          <StatusBarItem>{current ? `r${current.baseRev}` : "-"}</StatusBarItem>
          <StatusBarItem>{files.length} changed</StatusBarItem>
          <span className="flex-1" />
          <SegmentedControl<ThemeChoice>
            label="Theme"
            value={theme}
            onChange={(t) => {
              setTheme(t);
              applyTheme(t);
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
  );
}
