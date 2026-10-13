import { useQueries } from "@tanstack/react-query";
import { ChevronDown, ChevronRight, Folder } from "lucide-react";
import { type KeyboardEvent, useEffect, useId, useMemo, useState } from "react";
import { SafeText } from "../../../components/ui/safe-text";
import type { RepoEntry } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { cn } from "../../../lib/cn";
import { baseName, joinUrl } from "../repo-url";

export interface TreeRow {
  url: string;
  name: string;
  depth: number;
  expanded: boolean;
  loading: boolean;
  /** Parent row index in the flattened list (for ArrowLeft). */
  parent: number;
}

/**
 * Flattens the folder tree into visible rows. Only directories appear in the tree; files are shown
 * in the table pane. `lists` holds the loaded children per expanded URL.
 */
export function flattenTree(
  rootUrl: string,
  expanded: ReadonlySet<string>,
  lists: ReadonlyMap<string, RepoEntry[] | "loading">,
): TreeRow[] {
  const rows: TreeRow[] = [];
  const walk = (url: string, name: string, depth: number, parent: number) => {
    const isOpen = expanded.has(url);
    const children = lists.get(url);
    const idx = rows.length;
    rows.push({
      url,
      name,
      depth,
      expanded: isOpen,
      loading: isOpen && children === "loading",
      parent,
    });
    if (isOpen && Array.isArray(children)) {
      for (const c of children)
        if (c.kind === "dir") walk(joinUrl(url, c.name), c.name, depth + 1, idx);
    }
  };
  walk(rootUrl, baseName(rootUrl) || rootUrl, 0, -1);
  return rows;
}

/**
 * Lazy folder tree (ARIA tree, `aria-activedescendant` on one tab stop). Up/Down move, Right
 * expands or enters, Left collapses or goes to the parent, Home/End jump, Enter selects.
 */
export function RepoTree({
  rootUrl,
  rev,
  selectedUrl,
  onSelect,
  label,
  className,
}: {
  rootUrl: string;
  rev: number | null;
  selectedUrl: string;
  onSelect: (url: string) => void;
  label: string;
  className?: string;
}) {
  const baseId = useId();
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set([rootUrl]));
  // Active row is tracked by folder URL, not index: expanding/collapsing or refetching shifts indices.
  const [activeUrl, setActiveUrl] = useState(rootUrl);

  const expandedUrls = useMemo(() => [...expanded], [expanded]);
  const results = useQueries({
    queries: expandedUrls.map((url) => ({
      queryKey: queryKeys.repoList(url, rev),
      queryFn: () => unwrap(ipc().commands.repoList(url, rev)),
    })),
  });

  const rows = useMemo(() => {
    const lists = new Map<string, RepoEntry[] | "loading">();
    expandedUrls.forEach((url, i) => {
      const r = results[i];
      lists.set(url, r?.data ?? "loading");
    });
    return flattenTree(rootUrl, expanded, lists);
  }, [rootUrl, expanded, expandedUrls, results]);

  const found = rows.findIndex((r) => r.url === activeUrl);
  const activeIdx = found >= 0 ? found : 0;
  const setActive = (i: number) => {
    const r = rows[Math.max(0, Math.min(rows.length - 1, i))];
    if (r) setActiveUrl(r.url);
  };

  // Keep the active row visible while moving with the keyboard.
  // biome-ignore lint/correctness/useExhaustiveDependencies: scroll only when the active row changes
  useEffect(() => {
    document.getElementById(`${baseId}-${activeIdx}`)?.scrollIntoView?.({ block: "nearest" });
  }, [activeUrl]);
  const toggle = (url: string, open: boolean) =>
    setExpanded((cur) => {
      const next = new Set(cur);
      if (open) next.add(url);
      else next.delete(url);
      return next;
    });

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const row = rows[activeIdx];
    if (!row) return;
    switch (e.key) {
      case "ArrowDown":
        setActive(activeIdx + 1);
        break;
      case "ArrowUp":
        setActive(activeIdx - 1);
        break;
      case "Home":
        setActive(0);
        break;
      case "End":
        setActive(rows.length - 1);
        break;
      case "ArrowRight":
        if (!row.expanded) toggle(row.url, true);
        else setActive(activeIdx + 1);
        break;
      case "ArrowLeft":
        if (row.expanded) toggle(row.url, false);
        else if (row.parent >= 0) setActive(row.parent);
        break;
      case "Enter":
      case " ":
        onSelect(row.url);
        break;
      default:
        return;
    }
    e.preventDefault();
  };

  return (
    <div
      role="tree"
      tabIndex={0}
      aria-label={label}
      aria-activedescendant={rows[activeIdx] ? `${baseId}-${activeIdx}` : undefined}
      onKeyDown={onKeyDown}
      className={cn("group overflow-auto py-1", className)}
    >
      {rows.map((r, i) => (
        // biome-ignore lint/a11y/useFocusableInteractive: focus stays on the tree (aria-activedescendant)
        // biome-ignore lint/a11y/useKeyWithClickEvents: keyboard handled by the tree container
        <div
          key={r.url}
          id={`${baseId}-${i}`}
          role="treeitem"
          aria-level={r.depth + 1}
          aria-expanded={r.expanded}
          aria-selected={r.url === selectedUrl}
          onClick={() => {
            setActiveUrl(r.url);
            onSelect(r.url);
          }}
          className={cn(
            "flex cursor-default items-center gap-1 py-[3px] pr-2",
            r.url === selectedUrl ? "bg-sel" : "hover:bg-surface-2",
            i === activeIdx &&
              "group-focus-visible:outline-2 group-focus-visible:-outline-offset-2 group-focus-visible:outline-accent",
          )}
          // Indentation is data-driven; CSSOM style is allowed under style-src 'self'.
          style={{ paddingLeft: 6 + r.depth * 14 }}
        >
          <button
            type="button"
            tabIndex={-1}
            aria-hidden="true"
            onClick={(e) => {
              e.stopPropagation();
              toggle(r.url, !r.expanded);
            }}
            className="flex h-4 w-4 cursor-pointer items-center justify-center border-0 bg-transparent p-0 text-muted-text"
          >
            {r.expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
          </button>
          <Folder size={14} aria-hidden="true" className="text-muted-text" />
          <SafeText className="min-w-0 truncate">{r.name}</SafeText>
          {r.loading && <span className="text-[11px] text-muted-text">…</span>}
        </div>
      ))}
    </div>
  );
}
