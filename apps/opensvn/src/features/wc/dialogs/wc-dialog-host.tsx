import { ConflictResolver } from "../conflicts/conflict-resolver";
import { useWcSession } from "../wc-session";
import { CheckoutDialog } from "./checkout-dialog";
import { DeleteDialog, DiscardDialog, IgnoreDialog, RenameDialog } from "./file-dialogs";
import { ChangelistDialog, WcLockDialog } from "./lock-dialogs";
import { CleanupDialog, SwitchDialog, UpdateToRevisionDialog } from "./update-dialogs";

/** Renders whichever dialog the session has open (one at a time). */
export function WcDialogHost() {
  const { dialog, closeDialog, wc } = useWcSession();
  if (!dialog) return null;
  // Everything except checkout needs a working copy; guard so a stale dialog never mounts without one.
  if (dialog.kind !== "checkout" && !wc) return null;
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
      return <WcLockDialog mode={dialog.mode} paths={dialog.paths} onClose={closeDialog} />;
    case "changelist":
      return <ChangelistDialog paths={dialog.paths} onClose={closeDialog} />;
    case "update-rev":
      return <UpdateToRevisionDialog onClose={closeDialog} />;
    case "switch":
      return <SwitchDialog onClose={closeDialog} />;
    case "cleanup":
      return <CleanupDialog onClose={closeDialog} />;
    case "resolver":
      return <ConflictResolver initialPath={dialog.path} onClose={closeDialog} />;
  }
}
