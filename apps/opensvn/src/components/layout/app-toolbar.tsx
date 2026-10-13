import { Activity, Boxes, GitBranch, RefreshCw } from "lucide-react";
import type { ReactNode } from "react";
import { ToolbarItem } from "../ui/toolbar-item";

export interface AppToolbarProps {
  workingCopyName: string;
  /** Branch name, or the in-repo location when the repo has no trunk/branches/tags layout. */
  locationLabel: "Branch" | "Location";
  locationValue: string;
  incoming: number;
  updating: boolean;
  activityCount: number;
  onUpdate: () => void;
  onToggleActivity: () => void;
  /** Popover/menu wrappers injected by the caller (e.g. Popover around the item). */
  wrapWorkingCopy?: (item: ReactNode) => ReactNode;
  wrapLocation?: (item: ReactNode) => ReactNode;
}

/** Toolbar: Working copy v / Branch|Location v / Update / (spacer) / Activity. */
export function AppToolbar(p: AppToolbarProps) {
  const wrap = (fn: ((i: ReactNode) => ReactNode) | undefined, item: ReactNode) =>
    fn ? fn(item) : item;
  return (
    <div className="flex items-stretch border-b border-line" role="toolbar" aria-label="Main">
      {wrap(
        p.wrapWorkingCopy,
        <ToolbarItem
          icon={<Boxes size={18} />}
          label="Working copy"
          value={`${p.workingCopyName} ▾`}
        />,
      )}
      {wrap(
        p.wrapLocation,
        <ToolbarItem
          icon={<GitBranch size={18} />}
          label={p.locationLabel}
          value={`${p.locationValue} ▾`}
        />,
      )}
      <ToolbarItem
        icon={<RefreshCw size={18} />}
        label="Update"
        value="Update"
        badge={p.incoming > 0 ? `↓${p.incoming}` : undefined}
        busy={p.updating}
        onClick={p.onUpdate}
        className="min-w-0"
      />
      <div className="flex-1" />
      <ToolbarItem
        icon={<Activity size={18} />}
        label="Activity"
        value={p.activityCount > 0 ? `${p.activityCount} running` : "Idle"}
        onClick={p.onToggleActivity}
        className="min-w-0 border-r-0 border-l border-line"
      />
    </div>
  );
}
