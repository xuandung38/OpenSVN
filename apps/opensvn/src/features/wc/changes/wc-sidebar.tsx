import { MoreHorizontal } from "lucide-react";
import { useState } from "react";
import { RevisionRow } from "../../../components/list/revision-row";
import { VirtualList } from "../../../components/list/virtual-list";
import { Button } from "../../../components/ui/button";
import { DropdownMenu, type MenuEntries } from "../../../components/ui/dropdown-menu";
import { Tabs } from "../../../components/ui/tabs";
import { useLog } from "../../../ipc/queries";
import { relativeTime } from "../../../lib/format";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { shortcutLabel } from "../shortcuts";
import { useWcActions } from "../use-wc-actions";
import { changedFiles } from "../wc-model";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";
import { ChangesList } from "./changes-list";
import { CommitBox } from "./commit-box";
import { IncomingList } from "./incoming-list";

type Tab = "changes" | "history" | "incoming";

/** Sidebar of the main window: Changes / History / Incoming tabs, working-copy menu, commit box. */
export function WcSidebar() {
  const { wc, files, openDialog } = useWcSession();
  const t = useT(wcStrings);
  const c = useT(commonStrings);
  const actions = useWcActions();
  const log = useLog(wc?.id ?? null);
  const [tab, setTab] = useState<Tab>("changes");
  const changed = changedFiles(files);
  const entries = log.data?.entries ?? [];

  const menu: MenuEntries = [
    {
      label: t("updateAll"),
      shortcut: shortcutLabel("update"),
      onSelect: () => void actions.update(null),
    },
    { label: t("updateToRevision"), onSelect: () => openDialog({ kind: "update-rev" }) },
    {
      label: t("checkForUpdates"),
      shortcut: shortcutLabel("checkUpdates"),
      onSelect: () => void actions.checkForUpdates(),
    },
    "separator",
    {
      label: t("switchAction"),
      shortcut: shortcutLabel("switch"),
      onSelect: () => openDialog({ kind: "switch" }),
    },
    {
      label: t("discardAll"),
      disabled: changed.length === 0,
      onSelect: () => openDialog({ kind: "discard", paths: changed.map((f) => f.path), all: true }),
    },
    { label: t("cleanupAction"), onSelect: () => openDialog({ kind: "cleanup" }) },
    "separator",
    {
      label: t("checkoutAction"),
      shortcut: shortcutLabel("checkout"),
      onSelect: () => openDialog({ kind: "checkout" }),
    },
  ];

  return (
    <>
      <div className="flex items-center justify-end border-b border-line px-2 py-1">
        <DropdownMenu
          trigger={
            <Button small aria-label={t("wcMenu")} title={t("wcMenu")} disabled={!wc}>
              <MoreHorizontal size={14} aria-hidden="true" />
            </Button>
          }
          entries={menu}
        />
      </div>
      <Tabs
        label={t("sections")}
        className="flex-1"
        value={tab}
        onValueChange={(v) => setTab(v as Tab)}
        tabs={[
          { id: "changes", label: t("tabChanges"), count: changed.length },
          { id: "history", label: t("tabHistory") },
          { id: "incoming", label: t("tabIncoming"), count: wc?.incoming ?? 0 },
        ]}
        panels={{
          changes: <ChangesList />,
          history: (
            <VirtualList
              label={t("tabHistory")}
              className="h-full"
              count={entries.length}
              rowHeight={44}
              renderRow={(i, ctx) => {
                const e = entries[i];
                return e ? (
                  <RevisionRow
                    id={ctx.id}
                    active={ctx.active}
                    onPointerActivate={ctx.activate}
                    rev={e.rev}
                    message={e.message}
                    author={e.author ?? c("unknownAuthor")}
                    when={relativeTime(e.date)}
                  />
                ) : null;
              }}
            />
          ),
          incoming: <IncomingList />,
        }}
      />
      <CommitBox />
    </>
  );
}
