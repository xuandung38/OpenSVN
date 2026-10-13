import { MoreHorizontal, TriangleAlert } from "lucide-react";
import { DropdownMenu as M } from "radix-ui";
import {
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from "react";
import { Button } from "../../../components/ui/button";
import { Input } from "../../../components/ui/fields";
import { type MenuEntries, renderEntries } from "../../../components/ui/menu-entries";
import { MENU_CONTENT } from "../../../components/ui/menu-styles";
import { Popover } from "../../../components/ui/popover";
import { SafeText } from "../../../components/ui/safe-text";
import type { WcEntry } from "../../../ipc/bindings";
import { useWcEntries } from "../../../ipc/queries";
import { cn } from "../../../lib/cn";
import { relativeTime } from "../../../lib/format";
import { useT } from "../../../lib/i18n/i18n";
import { shortcutLabel } from "../shortcuts";
import { useWcSession } from "../wc-session";
import { useAddExistingWorkingCopy } from "../welcome/welcome-screen";
import { filterEntries, pathTail, sortByLastOpened, urlHost } from "./repo-model";
import { repoStrings } from "./repo-strings";
import { useRepoActions } from "./use-repo-actions";

/**
 * "Current repository" picker (GitHub Desktop style, doc 01 §3.2 / §6.5): a filterable list of every
 * registered working copy. Typing filters; Up/Down move; Enter switches; Shift+F10 or the context
 * menu key (or right-click / the ⋯ button) opens the row's actions. Missing working copies are
 * flagged and offer "Locate…" instead of switching. The footer starts a checkout or adds an existing
 * working copy.
 */
export function RepoPicker({
  trigger,
  open,
  onOpenChange,
  currentId,
  onSelect,
}: {
  trigger: ReactNode;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  currentId: string | null;
  onSelect: (wcId: string) => void;
}) {
  const r = useT(repoStrings);
  const { openDialog } = useWcSession();
  const addExisting = useAddExistingWorkingCopy();
  const actions = useRepoActions(onSelect);
  const entriesQ = useWcEntries(open);
  const listId = useId();
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const [menuFor, setMenuFor] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const all = useMemo(() => sortByLastOpened(entriesQ.data ?? []), [entriesQ.data]);
  const shown = useMemo(() => filterEntries(all, query), [all, query]);
  const activeIdx = Math.min(active, Math.max(0, shown.length - 1));
  const activeEntry = shown[activeIdx];

  // Every opening starts with an empty filter on the current repository.
  const [wasOpen, setWasOpen] = useState(open);
  if (wasOpen !== open) {
    setWasOpen(open);
    if (open) {
      setQuery("");
      setMenuFor(null);
      setActive(
        Math.max(
          0,
          all.findIndex((e) => e.id === currentId),
        ),
      );
    }
  }

  // Keep the active row visible while moving with the keyboard.
  useEffect(() => {
    if (!open) return;
    document.getElementById(`${listId}-${activeIdx}`)?.scrollIntoView?.({ block: "nearest" });
  }, [open, listId, activeIdx]);

  const close = () => onOpenChange(false);
  const choose = (e: WcEntry) => {
    if (e.missing) {
      setMenuFor(e.id);
      return;
    }
    close();
    if (e.id !== currentId) void actions.switchTo(e);
  };

  const onKeyDown = (ev: KeyboardEvent<HTMLInputElement>) => {
    switch (ev.key) {
      case "ArrowDown":
        setActive(Math.min(shown.length - 1, activeIdx + 1));
        break;
      case "ArrowUp":
        setActive(Math.max(0, activeIdx - 1));
        break;
      case "PageDown":
        setActive(Math.min(shown.length - 1, activeIdx + 8));
        break;
      case "PageUp":
        setActive(Math.max(0, activeIdx - 8));
        break;
      case "Enter":
        if (activeEntry) choose(activeEntry);
        break;
      case "ContextMenu":
        if (activeEntry) setMenuFor(activeEntry.id);
        break;
      case "F10":
        if (!ev.shiftKey || !activeEntry) return;
        setMenuFor(activeEntry.id);
        break;
      default:
        return;
    }
    ev.preventDefault();
  };

  const entriesFor = (e: WcEntry): MenuEntries => {
    const later = (fn: () => void) => () => {
      close();
      fn();
    };
    const rename = { label: r("rename"), onSelect: later(() => openRepoDialog("repo-rename", e)) };
    const remove = { label: r("remove"), onSelect: later(() => openRepoDialog("repo-remove", e)) };
    return e.missing
      ? [
          { label: r("locate"), onSelect: later(() => void actions.locate(e)) },
          rename,
          "separator",
          remove,
        ]
      : [
          { label: r("reveal"), onSelect: () => void actions.reveal(e) },
          { label: r("copyUrl"), onSelect: () => void actions.copyUrl(e) },
          "separator",
          rename,
          remove,
        ];
  };
  const openRepoDialog = (kind: "repo-rename" | "repo-remove", e: WcEntry) =>
    openDialog({ kind, wcId: e.id, name: e.name });

  return (
    <Popover
      trigger={trigger}
      open={open}
      onOpenChange={onOpenChange}
      label={r("currentRepo")}
      className="w-[380px]"
    >
      <Input
        ref={inputRef}
        autoFocus
        role="combobox"
        aria-label={r("filter")}
        aria-expanded="true"
        aria-controls={listId}
        aria-autocomplete="list"
        aria-activedescendant={activeEntry ? `${listId}-${activeIdx}` : undefined}
        aria-keyshortcuts="Shift+F10"
        placeholder={r("filter")}
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          setActive(0);
        }}
        onKeyDown={onKeyDown}
        spellCheck={false}
      />
      <div
        role="listbox"
        id={listId}
        aria-label={r("listLabel")}
        className="flex max-h-[min(60vh,420px)] flex-col overflow-auto"
      >
        {shown.map((e, i) => (
          <div key={e.id} role="presentation" className="flex items-center">
            {/* biome-ignore lint/a11y/useFocusableInteractive: focus stays in the filter (aria-activedescendant) */}
            {/* biome-ignore lint/a11y/useKeyWithClickEvents: keyboard handled by the filter input */}
            <div
              id={`${listId}-${i}`}
              role="option"
              aria-selected={i === activeIdx}
              aria-current={e.id === currentId ? "true" : undefined}
              title={e.missing ? (e.problem ?? r("missingHint")) : e.rootPath}
              onMouseMove={() => setActive(i)}
              onClick={() => choose(e)}
              onContextMenu={(ev) => {
                ev.preventDefault();
                setActive(i);
                setMenuFor(e.id);
              }}
              className={cn(
                "flex min-w-0 flex-1 cursor-default flex-col rounded-[5px] px-2 py-1.5",
                i === activeIdx && "bg-sel",
              )}
            >
              <span className="flex min-w-0 items-center gap-1.5">
                <SafeText className={cn("truncate", e.id === currentId && "font-semibold")}>
                  {e.name}
                </SafeText>
                {e.missing && (
                  <span className="flex shrink-0 items-center gap-0.5 rounded bg-warn-soft px-1 text-[11px]">
                    <TriangleAlert size={12} aria-hidden="true" />
                    {r("missing")}
                  </span>
                )}
              </span>
              <SafeText className="truncate font-mono text-[11px] text-muted-text">
                {pathTail(e.rootPath)}
              </SafeText>
              <SafeText className="truncate text-[11px] text-muted-text">
                {`${urlHost(e.repoRootUrl)} · ${
                  e.lastOpenedAt
                    ? r("lastOpened", { when: relativeTime(e.lastOpenedAt) })
                    : r("neverOpened")
                }`}
              </SafeText>
            </div>
            <M.Root
              modal={false}
              open={menuFor === e.id}
              onOpenChange={(o) => setMenuFor(o ? e.id : null)}
            >
              <M.Trigger asChild>
                <button
                  type="button"
                  tabIndex={-1}
                  aria-hidden="true"
                  className="ml-1 flex h-6 w-6 shrink-0 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-muted-text hover:bg-surface-2"
                >
                  <MoreHorizontal size={14} />
                </button>
              </M.Trigger>
              <M.Portal>
                <M.Content
                  align="end"
                  sideOffset={2}
                  aria-label={r("actionsFor", { name: e.name })}
                  className={MENU_CONTENT}
                  onCloseAutoFocus={(ev) => {
                    // Back to the filter, which owns the keyboard of the picker.
                    ev.preventDefault();
                    inputRef.current?.focus();
                  }}
                >
                  {renderEntries(M, entriesFor(e))}
                </M.Content>
              </M.Portal>
            </M.Root>
          </div>
        ))}
        {entriesQ.isSuccess && shown.length === 0 && (
          <SafeText as="p" className="m-0 px-2 py-3 text-muted-text">
            {all.length === 0 ? r("empty") : r("noMatch", { query })}
          </SafeText>
        )}
      </div>
      <div className="flex flex-col gap-1 border-t border-line pt-1.5">
        <Button
          small
          title={shortcutLabel("checkout")}
          onClick={() => {
            close();
            openDialog({ kind: "checkout" });
          }}
        >
          {r("checkoutAction")}
        </Button>
        <Button
          small
          title={shortcutLabel("addExisting")}
          onClick={() => {
            close();
            void addExisting();
          }}
        >
          {r("addExistingAction")}
        </Button>
      </div>
    </Popover>
  );
}
