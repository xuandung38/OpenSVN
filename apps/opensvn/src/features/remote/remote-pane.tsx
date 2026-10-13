import { useCallback, useMemo, useState } from "react";
import type { LogTarget, WorkingCopy } from "../../ipc/bindings";
import { type BrowseLocation, BrowseScreen } from "./browse/browse-screen";
import { HistoryScreen } from "./history/history-screen";
import type { RevisionNav } from "./history/revision-details";
import { repoPath } from "./repo-url";

export type RemoteView = "history" | "browse";

/**
 * Hosts the History and Browse tabs for one working copy and wires the cross-links between them
 * (open file at revision, blame at revision, show log of path, blame line to revision). Both views
 * stay mounted so their state survives tab switches; only the active one is visible.
 */
export function RemotePane({
  view,
  onViewChange,
  wc,
}: {
  /** `null` hides both (another tab is active) while keeping their state. */
  view: RemoteView | null;
  onViewChange: (v: RemoteView) => void;
  wc: WorkingCopy;
}) {
  const [scope, setScope] = useState<LogTarget | null>(null);
  const [focusRev, setFocusRev] = useState<number | null>(null);
  const [location, setLocation] = useState<BrowseLocation | null>(null);

  const showLogOf = useCallback(
    (url: string) => {
      setScope({ kind: "url", url });
      onViewChange("history");
    },
    [onViewChange],
  );
  const nav: RevisionNav = useMemo(
    () => ({
      openInBrowse: (url, rev) => {
        setLocation({ url, rev, tab: "preview", nonce: Date.now() });
        onViewChange("browse");
      },
      blameAt: (url, rev) => {
        setLocation({ url, rev, tab: "blame", nonce: Date.now() });
        onViewChange("browse");
      },
      showLogOf,
    }),
    [onViewChange, showLogOf],
  );

  const target: LogTarget = scope ?? { kind: "wc", wcId: wc.id, path: "" };
  const scopeLabel = scope?.kind === "url" ? repoPath(scope.url, wc.repoRootUrl) : null;

  return (
    <>
      <div className={view === "history" ? "h-full" : "hidden"}>
        <HistoryScreen
          // Reset selection/filters when the working copy or scope changes.
          key={`${wc.id}:${scopeLabel ?? ""}`}
          target={target}
          repoRootUrl={wc.repoRootUrl}
          scopeLabel={scopeLabel}
          onClearScope={scope ? () => setScope(null) : undefined}
          focusRev={focusRev}
          nav={nav}
        />
      </div>
      <div className={view === "browse" ? "h-full" : "hidden"}>
        <BrowseScreen
          key={wc.id}
          repoRootUrl={wc.repoRootUrl}
          startUrl={wc.url}
          wcId={wc.id}
          layoutRepo={wc.branch !== null}
          location={location}
          onShowLog={showLogOf}
          historyNav={nav}
          onOpenRevision={(rev) => {
            setFocusRev(rev);
            onViewChange("history");
          }}
        />
      </div>
    </>
  );
}
