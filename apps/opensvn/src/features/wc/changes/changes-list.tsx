import { Folder, SlidersHorizontal } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { VirtualList } from "../../../components/list/virtual-list";
import { Button } from "../../../components/ui/button";
import { Chip } from "../../../components/ui/chip";
import { ContextMenu } from "../../../components/ui/context-menu";
import { DropdownMenu, type MenuEntries } from "../../../components/ui/dropdown-menu";
import { EmptyState } from "../../../components/ui/empty-state";
import { Input } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import type { FileStatus } from "../../../ipc/bindings";
import { cn } from "../../../lib/cn";
import { useT } from "../../../lib/i18n/i18n";
import { useUiCommand } from "../ui-commands";
import { useWcActions } from "../use-wc-actions";
import { setViewPref, useViewPrefs, type ViewPref } from "../view-prefs";
import {
  buildRows,
  CHIPS,
  type ChipId,
  changedFiles,
  filterFiles,
  isTickable,
  listedFiles,
  lockFiles,
  rowGlyph,
  suggestMoves,
  tickedByDefault,
  tristate,
} from "../wc-model";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";
import { fileMenuEntries } from "./file-menu";
import { LocksList } from "./locks-list";
import { WcFileRow } from "./wc-file-row";

const ROW_H = 28;
const NO_CHIPS: ReadonlySet<ChipId> = new Set();
/** Indent of one tree level, in pixels. */
const INDENT = 14;

const VIEW_OPTIONS = [
  ["showUnversioned", "viewShowUnversioned"],
  ["showIgnored", "viewShowIgnored"],
  ["groupByChangelist", "viewGroupChangelist"],
  ["tree", "viewTree"],
] as const satisfies readonly (readonly [ViewPref, keyof (typeof wcStrings)["en"]])[];

const CHIP_LABEL = {
  modified: "chipModified",
  added: "chipAdded",
  unversioned: "chipUnversioned",
  conflicted: "chipConflicted",
  locked: "chipLocked",
} as const satisfies Record<ChipId, keyof (typeof wcStrings)["en"]>;

/**
 * Changes tab list (doc 01 3.3): filter + quick chips, tri-state "select all", changelist groups,
 * staging-less commit selection (ticked rows are what the next commit contains) and move hints.
 * The View options (doc 01 8.4, also in the View menu) hide unversioned files, show ignored ones,
 * turn changelist groups off and show the list as a folder tree.
 */
export function ChangesList() {
  const session = useWcSession();
  const { files, checked, setChecked, selected, setSelected } = session;
  const t = useT(wcStrings);
  const actions = useWcActions();
  const [filter, setFilter] = useState("");
  const [chips, setChips] = useState<ReadonlySet<ChipId>>(new Set());
  const prefs = useViewPrefs();

  const changed = useMemo(() => changedFiles(files), [files]);
  const listed = useMemo(() => listedFiles(files, prefs), [files, prefs]);
  const tickable = useMemo(() => new Set(listed.filter(isTickable).map((f) => f.path)), [listed]);
  const visible = useMemo(() => filterFiles(listed, filter, chips), [listed, filter, chips]);
  const locks = useMemo(() => filterFiles(lockFiles(files), filter, NO_CHIPS), [files, filter]);
  const rows = useMemo(
    () => buildRows(visible, { group: prefs.groupByChangelist, tree: prefs.tree }),
    [visible, prefs.groupByChangelist, prefs.tree],
  );
  const moves = useMemo(() => suggestMoves(changed), [changed]);

  // Tick tracked changes the first time they appear; forget ticks of files that left the list
  // (also when a View option hides them).
  const seen = useRef(new Set<string>());
  useEffect(() => {
    const next = new Set([...checked].filter((p) => tickable.has(p)));
    for (const f of listed) {
      if (!seen.current.has(f.path) && tickedByDefault(f)) next.add(f.path);
    }
    for (const p of [...seen.current]) if (!tickable.has(p)) seen.current.delete(p);
    for (const p of tickable) seen.current.add(p);
    if (next.size !== checked.size || [...next].some((p) => !checked.has(p))) setChecked(next);
  }, [listed, tickable, checked, setChecked]);

  // A file that left the list (committed, reverted, ignored, hidden) cannot stay selected.
  useEffect(() => {
    if (selected !== null && !listed.some((f) => f.path === selected)) setSelected(null);
  }, [listed, selected, setSelected]);

  const toggleMany = (all: readonly string[]) => {
    const paths = all.filter((p) => tickable.has(p));
    if (paths.length === 0) return;
    const next = new Set(checked);
    const allOn = paths.every((p) => next.has(p));
    for (const p of paths) {
      if (allOn) next.delete(p);
      else next.add(p);
    }
    setChecked(next);
  };
  const visiblePaths = useMemo(() => visible.filter(isTickable).map((f) => f.path), [visible]);
  const overall = tristate(visiblePaths, checked);
  const headerRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (headerRef.current) headerRef.current.indeterminate = overall === "some";
  }, [overall]);

  // Mod+F / Edit > Find focuses the file filter (doc 01 8.3); routed by `useWcShortcuts`.
  useUiCommand("filter", () => {
    document.querySelector<HTMLInputElement>("[data-wc-filter]")?.focus();
  });

  const targetsFor = (f: FileStatus): FileStatus[] =>
    checked.has(f.path) && checked.size > 1 ? listed.filter((c) => checked.has(c.path)) : [f];

  const viewMenu: MenuEntries = VIEW_OPTIONS.map(([key, label]) => ({
    label: t(label),
    checked: prefs[key],
    onSelect: () => setViewPref(key, !prefs[key]),
  }));

  const labels = { needsLock: t("needsLockBadge"), outOfDate: t("outOfDateBadge") };
  const rootLabel = t("wcRootRow", { name: session.wc?.name ?? "" });

  return (
    <>
      <div className="flex flex-col gap-1.5 border-b border-line px-2.5 py-2">
        <div className="flex items-center gap-1.5">
          <Input
            data-wc-filter=""
            aria-label={t("filter")}
            placeholder={t("filterPlaceholder")}
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            className="min-w-0 flex-1"
          />
          <DropdownMenu
            trigger={
              <Button small aria-label={t("viewOptions")} title={t("viewOptions")}>
                <SlidersHorizontal size={14} aria-hidden="true" />
              </Button>
            }
            entries={viewMenu}
          />
        </div>
        <fieldset
          aria-label={t("chips")}
          className="m-0 flex min-w-0 flex-wrap gap-1.5 border-0 p-0"
        >
          {CHIPS.map((c) => (
            <Chip
              key={c}
              on={chips.has(c)}
              onClick={() =>
                setChips((cur) => {
                  const next = new Set(cur);
                  if (!next.delete(c)) next.add(c);
                  return next;
                })
              }
            >
              {t(CHIP_LABEL[c])}
            </Chip>
          ))}
        </fieldset>
        <label className="flex items-center gap-2 text-muted-text">
          <input
            ref={headerRef}
            type="checkbox"
            checked={overall === "all"}
            disabled={visible.length === 0}
            onChange={() => toggleMany(visiblePaths)}
            className="m-0 h-[14px] w-[14px] accent-[var(--accent)]"
          />
          {t("selectAll")}
        </label>
      </div>
      {moves.length > 0 && (
        <ul className="m-0 list-none border-b border-line bg-surface-2 p-0">
          {moves.map((m) => (
            <li key={m.from} className="flex items-center gap-2 px-2.5 py-1.5 text-[12px]">
              <SafeText className="min-w-0 flex-1">{t("suggestMove", m)}</SafeText>
              <Button small onClick={() => void actions.recordMove(m.from, m.to)}>
                {t("recordMove")}
              </Button>
            </li>
          ))}
        </ul>
      )}
      <div className="min-h-0 flex-1">
        {rows.length === 0 ? (
          <EmptyState
            title={listed.length === 0 ? t("noChanges") : t("noMatches")}
            description={listed.length === 0 ? t("noChangesHint") : ""}
          />
        ) : (
          <VirtualList
            label={t("changedFiles")}
            className="h-full"
            count={rows.length}
            rowHeight={ROW_H}
            onActivate={(i) => {
              const r = rows[i];
              if (r?.kind === "file") setSelected(r.file.path);
              else if (r) toggleMany(r.paths);
            }}
            onToggle={(i) => {
              const r = rows[i];
              if (r?.kind === "file") toggleMany([r.file.path]);
              else if (r) toggleMany(r.paths);
            }}
            renderRow={(i, ctx) => {
              const r = rows[i];
              if (!r) return null;
              if (r.kind === "group") {
                const text =
                  r.name === null
                    ? t("noChangelist", { count: r.paths.length })
                    : t("changelist", { name: r.name, count: r.paths.length });
                const state = tristate(
                  r.paths.filter((p) => tickable.has(p)),
                  checked,
                );
                return (
                  // biome-ignore lint/a11y/useFocusableInteractive: roving focus lives on the owning listbox
                  // biome-ignore lint/a11y/useKeyWithClickEvents: Enter/Space handled by the listbox
                  <div
                    id={ctx.id}
                    role="option"
                    aria-selected={false}
                    aria-checked={state === "all" ? true : state === "some" ? "mixed" : false}
                    onClick={() => {
                      ctx.activate();
                      toggleMany(r.paths);
                    }}
                    className={cn(
                      "flex h-full cursor-default items-center bg-surface-2 px-2.5 text-[11px] font-semibold text-muted-text",
                      ctx.active &&
                        "group-focus-visible:outline-2 group-focus-visible:-outline-offset-2 group-focus-visible:outline-accent",
                    )}
                  >
                    <SafeText>{text}</SafeText>
                  </div>
                );
              }
              if (r.kind === "dir") {
                const state = tristate(
                  r.paths.filter((p) => tickable.has(p)),
                  checked,
                );
                return (
                  // biome-ignore lint/a11y/useFocusableInteractive: roving focus lives on the owning listbox
                  // biome-ignore lint/a11y/useKeyWithClickEvents: Enter/Space handled by the listbox
                  <div
                    id={ctx.id}
                    role="option"
                    aria-selected={false}
                    aria-checked={state === "all" ? true : state === "some" ? "mixed" : false}
                    aria-label={t("folderRow", { path: r.path, count: r.paths.length })}
                    onClick={() => {
                      ctx.activate();
                      toggleMany(r.paths);
                    }}
                    style={{ paddingLeft: 10 + r.depth * INDENT }}
                    className={cn(
                      "flex h-full cursor-default items-center gap-1.5 pr-2.5 font-mono text-[12px] text-muted-text",
                      ctx.active &&
                        "group-focus-visible:outline-2 group-focus-visible:-outline-offset-2 group-focus-visible:outline-accent",
                    )}
                  >
                    <Folder size={13} aria-hidden="true" className="shrink-0" />
                    <SafeText className="min-w-0 truncate">{r.name}</SafeText>
                  </div>
                );
              }
              const f = r.file;
              const glyph = rowGlyph(f);
              if (!glyph) return null;
              return (
                <ContextMenu entries={fileMenuEntries(targetsFor(f), session, actions, t)}>
                  <div className="h-full">
                    <WcFileRow
                      id={ctx.id}
                      path={f.path}
                      rootLabel={rootLabel}
                      glyph={glyph}
                      checked={checked.has(f.path)}
                      selected={selected === f.path}
                      active={ctx.active}
                      lock={f.lock ? (f.lock.mine ? "me" : "other") : undefined}
                      needsLock={f.needsLock}
                      outOfDate={f.remote?.outOfDate === true}
                      depth={r.depth}
                      tickable={isTickable(f)}
                      labels={labels}
                      onToggle={(p) => toggleMany([p])}
                      onSelect={setSelected}
                      onPointerActivate={ctx.activate}
                    />
                  </div>
                </ContextMenu>
              );
            }}
          />
        )}
      </div>
      <LocksList files={locks} />
    </>
  );
}
