import { useCallback, useMemo, useState } from "react";
import type { LogTarget, WorkingCopy } from "../../ipc/bindings";
import { type BrowseLocation, BrowseScreen } from "./browse/browse-screen";
import { HistoryScreen } from "./history/history-screen";
import type { RevisionNav } from "./history/revision-details";
import { repoPath } from "./repo-url";

export type RemoteView = "history" | "browse";

export interface RemoteLinks {
  scope: LogTarget | null;
  clearScope: () => void;
  focusRev: number | null;
  location: BrowseLocation | null;
  nav: RevisionNav;
  showLogOf: (url: string) => void;
  openRevision: (rev: number) => void;
}

/**
 * State and cross-links shared by the History and Browse tabs (open file at revision, blame at
 * revision, show log of path, blame line to revision). Held above the tabs so it survives tab
 * switches; the caller provides `goTo` to switch the visible tab.
 */
export function useRemoteLinks(goTo: (v: RemoteView) => void): RemoteLinks {
  const [scope, setScope] = useState<LogTarget | null>(null);
  const [focusRev, setFocusRev] = useState<number | null>(null);
  const [location, setLocation] = useState<BrowseLocation | null>(null);

  const showLogOf = useCallback(
    (url: string) => {
      setScope({ kind: "url", url });
      goTo("history");
    },
    [goTo],
  );
  const nav: RevisionNav = useMemo(
    () => ({
      openInBrowse: (url, rev) => {
        setLocation({ url, rev, tab: "preview", nonce: Date.now() });
        goTo("browse");
      },
      blameAt: (url, rev) => {
        setLocation({ url, rev, tab: "blame", nonce: Date.now() });
        goTo("browse");
      },
      showLogOf,
    }),
    [goTo, showLogOf],
  );
  const openRevision = useCallback(
    (rev: number) => {
      setFocusRev(rev);
      goTo("history");
    },
    [goTo],
  );

  return {
    scope,
    clearScope: () => setScope(null),
    focusRev,
    location,
    nav,
    showLogOf,
    openRevision,
  };
}

/** History tab content for a working copy (whole WC, or the scope chosen from Browse). */
export function RemoteHistory({ wc, links }: { wc: WorkingCopy; links: RemoteLinks }) {
  const target: LogTarget = links.scope ?? { kind: "wc", wcId: wc.id, path: "" };
  const scopeLabel = links.scope?.kind === "url" ? repoPath(links.scope.url, wc.repoRootUrl) : null;
  return (
    <HistoryScreen
      // Reset selection/filters when the working copy or scope changes.
      key={`${wc.id}:${scopeLabel ?? ""}`}
      target={target}
      repoRootUrl={wc.repoRootUrl}
      scopeLabel={scopeLabel}
      onClearScope={links.scope ? links.clearScope : undefined}
      focusRev={links.focusRev}
      nav={links.nav}
    />
  );
}

/** Browse tab content for a working copy. */
export function RemoteBrowse({ wc, links }: { wc: WorkingCopy; links: RemoteLinks }) {
  return (
    <BrowseScreen
      key={wc.id}
      repoRootUrl={wc.repoRootUrl}
      repoUuid={wc.repoUuid}
      startUrl={wc.url}
      wcId={wc.id}
      currentRefUrl={wc.branch?.url ?? null}
      layoutRepo={wc.branch !== null}
      location={links.location}
      onShowLog={links.showLogOf}
      historyNav={links.nav}
      onOpenRevision={links.openRevision}
    />
  );
}
