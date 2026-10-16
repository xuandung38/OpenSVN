import { CompareDialog } from "../compare/compare-dialog";
import { ConflictResolver } from "../conflicts/conflict-resolver";
import { ReportProblemDialog } from "../help/report-problem-dialog";
import { LayoutDialog } from "../props/layout-dialog";
import { PropertiesDialog } from "../props/properties-dialog";
import { RemoveRepoDialog, RenameRepoDialog } from "../repos/repo-dialogs";
import { isAppDialog, useWcSession } from "../wc-session";
import { CheckoutDialog } from "./checkout-dialog";
import { DeleteDialog, DiscardDialog, IgnoreDialog, RenameDialog } from "./file-dialogs";
import { ChangelistDialog, WcLockDialog } from "./lock-dialogs";
import { CleanupDialog, SwitchDialog, UpdateToRevisionDialog } from "./update-dialogs";

/** Renders whichever dialog the session has open (one at a time). */
export function WcDialogHost() {
  const { dialog, closeDialog, wc } = useWcSession();
  if (!dialog) return null;
  // Everything except the app-level dialogs needs a working copy; guard so a stale dialog never
  // mounts without one.
  if (!isAppDialog(dialog) && !wc) return null;
  switch (dialog.kind) {
    case "checkout":
      return <CheckoutDialog onClose={closeDialog} />;
    case "discard":
      return <DiscardDialog paths={dialog.paths} all={dialog.all} onClose={closeDialog} />;
    case "ignore":
      return <IgnoreDialog path={dialog.path} onClose={closeDialog} />;
    case "rename":
      return <RenameDialog path={dialog.path} onClose={closeDialog} />;
    case "delete":
      return <DeleteDialog paths={dialog.paths} onClose={closeDialog} />;
    case "lock":
      return (
        <WcLockDialog
          mode={dialog.mode}
          paths={dialog.paths}
          conflicts={dialog.conflicts}
          onClose={closeDialog}
        />
      );
    case "changelist":
      return <ChangelistDialog paths={dialog.paths} onClose={closeDialog} />;
    case "update-rev":
      return <UpdateToRevisionDialog onClose={closeDialog} />;
    case "switch":
      return <SwitchDialog onClose={closeDialog} />;
    case "cleanup":
      return <CleanupDialog foreignLock={dialog.foreignLock ?? false} onClose={closeDialog} />;
    case "properties":
      return <PropertiesDialog paths={dialog.paths} onClose={closeDialog} />;
    case "layout":
      return <LayoutDialog onClose={closeDialog} />;
    case "repo-rename":
      return <RenameRepoDialog wcId={dialog.wcId} name={dialog.name} onClose={closeDialog} />;
    case "repo-remove":
      return <RemoveRepoDialog wcId={dialog.wcId} name={dialog.name} onClose={closeDialog} />;
    case "resolver":
      return <ConflictResolver initialPath={dialog.path} onClose={closeDialog} />;
    case "compare":
      return <CompareDialog onClose={closeDialog} />;
    case "report":
      return <ReportProblemDialog onClose={closeDialog} />;
  }
}
