import { useMemo, useState } from "react";
import type { FileStatus, LogEntry } from "../../ipc/bindings";
import { relativeTime } from "../../lib/format";
import { statusCode } from "../../lib/status";
import { FileRow } from "../list/file-row";
import { RevisionRow } from "../list/revision-row";
import { VirtualList } from "../list/virtual-list";
import { Button } from "../ui/button";
import { Chip } from "../ui/chip";
import { EmptyState } from "../ui/empty-state";
import { Input, Textarea } from "../ui/fields";
import { Tabs } from "../ui/tabs";

const ROW_H = 28;
const REV_H = 44;

type Tab = "changes" | "history" | "incoming";

/** Sidebar content: tabs, filter + chips, virtual list, commit box (design-guidelines 5). */
export function ChangesSidebar({
  files,
  log,
  incomingCount,
  onCommit,
}: {
  files: FileStatus[];
  log: LogEntry[];
  incomingCount: number;
  onCommit?: (paths: string[], message: string) => void;
}) {
  const [tab, setTab] = useState<Tab>("changes");
  const [filter, setFilter] = useState("");
  const [onlyConflicts, setOnlyConflicts] = useState(false);
  const [checked, setChecked] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [message, setMessage] = useState("");

  const visible = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return files.filter(
      (f) =>
        statusCode(f.text) !== null &&
        (!q || f.path.toLowerCase().includes(q)) &&
        (!onlyConflicts || f.text === "conflicted"),
    );
  }, [files, filter, onlyConflicts]);

  const toggle = (path: string) =>
    setChecked((cur) => {
      const next = new Set(cur);
      if (!next.delete(path)) next.add(path);
      return next;
    });

  const canCommit = checked.size > 0 && message.trim().length > 0;

  return (
    <>
      <Tabs
        label="Sidebar sections"
        value={tab}
        onValueChange={(v) => setTab(v as Tab)}
        tabs={[
          { id: "changes", label: "Changes", count: files.length },
          { id: "history", label: "History" },
          { id: "incoming", label: "Incoming", count: incomingCount },
        ]}
      />
      <div className="flex flex-col gap-1.5 border-b border-line px-2.5 py-2">
        <Input
          aria-label="Filter"
          placeholder="Filter files"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <div className="flex gap-1.5">
          <Chip on={onlyConflicts} onClick={() => setOnlyConflicts((v) => !v)}>
            Conflicts
          </Chip>
        </div>
      </div>
      <div className="min-h-0 flex-1">
        {tab === "changes" &&
          (visible.length === 0 ? (
            <EmptyState
              title="No changes"
              description="Files you modify in the working copy appear here."
            />
          ) : (
            <VirtualList
              label="Changed files"
              className="h-full"
              count={visible.length}
              rowHeight={ROW_H}
              renderRow={(i) => {
                const f = visible[i];
                const code = f && statusCode(f.text);
                if (!f || !code) return null;
                return (
                  <FileRow
                    path={f.path}
                    status={code}
                    checked={checked.has(f.path)}
                    selected={selected === f.path}
                    lock={f.lock ? (f.lock.mine ? "me" : "other") : undefined}
                    onToggle={toggle}
                    onSelect={setSelected}
                  />
                );
              }}
            />
          ))}
        {tab === "history" && (
          <VirtualList
            label="History"
            className="h-full"
            count={log.length}
            rowHeight={REV_H}
            renderRow={(i) => {
              const e = log[i];
              return e ? (
                <RevisionRow
                  rev={e.rev}
                  message={e.message}
                  author={e.author ?? "unknown"}
                  when={relativeTime(e.date)}
                />
              ) : null;
            }}
          />
        )}
        {tab === "incoming" && (
          <EmptyState
            title="Up to date"
            description="New revisions on the server appear here before you update."
          />
        )}
      </div>
      <div className="flex flex-col gap-2 border-t border-line bg-surface-2 p-2.5">
        <Textarea
          aria-label="Commit message"
          rows={3}
          placeholder="Commit message"
          value={message}
          onChange={(e) => setMessage(e.target.value)}
        />
        <Button
          variant="primary"
          disabled={!canCommit}
          onClick={() => onCommit?.([...checked], message)}
        >
          Commit {checked.size > 0 ? `${checked.size} file${checked.size > 1 ? "s" : ""}` : ""}
        </Button>
      </div>
    </>
  );
}
