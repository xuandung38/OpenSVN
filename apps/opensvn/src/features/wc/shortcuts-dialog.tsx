import { Button } from "../../components/ui/button";
import { Dialog } from "../../components/ui/dialog";
import { commonStrings } from "../../lib/i18n/common-strings";
import { defineStrings, useT } from "../../lib/i18n/i18n";
import { SHORTCUTS, type ShortcutId, shortcutLabel } from "./shortcuts";
import { wcStrings } from "./wc-strings";

const names = defineStrings<ShortcutId>({
  en: {
    commit: "Commit",
    update: "Update",
    checkUpdates: "Check for updates",
    lock: "Lock",
    discard: "Discard changes",
    showLog: "Show log",
    blame: "Blame",
    checkout: "Checkout repository",
    addExisting: "Add existing working copy",
    switchRepo: "Switch repository",
    viewChanges: "Changes",
    viewHistory: "History",
    viewBrowse: "Browse",
    activity: "Activity panel",
    filter: "Find / Filter",
    toggleDiffMode: "Diff: split / unified",
    toggleWhitespace: "Hide whitespace changes",
    switch: "Switch branch",
    closeWc: "Close working copy",
    copyPath: "Copy path",
    toggleSidebar: "Toggle sidebar",
    reveal: "Show in file manager",
    terminal: "Open in terminal",
    editor: "Open in editor",
    newBranch: "New branch",
    newTag: "New tag",
    showShortcuts: "Keyboard shortcuts",
    settings: "Settings",
  },
  vi: {
    commit: "Commit",
    update: "Update",
    checkUpdates: "Kiểm tra bản mới",
    lock: "Lock",
    discard: "Hủy thay đổi",
    showLog: "Xem log",
    blame: "Blame",
    checkout: "Checkout repository",
    addExisting: "Thêm working copy có sẵn",
    switchRepo: "Chuyển repository",
    viewChanges: "Thay đổi",
    viewHistory: "Lịch sử",
    viewBrowse: "Duyệt",
    activity: "Bảng hoạt động",
    filter: "Tìm / Lọc",
    toggleDiffMode: "Diff: song song / gộp",
    toggleWhitespace: "Ẩn thay đổi khoảng trắng",
    switch: "Chuyển nhánh",
    closeWc: "Đóng working copy",
    copyPath: "Sao chép đường dẫn",
    toggleSidebar: "Ẩn/hiện thanh bên",
    reveal: "Hiện trong trình quản lý file",
    terminal: "Mở trong terminal",
    editor: "Mở trong editor",
    newBranch: "Nhánh mới",
    newTag: "Tag mới",
    showShortcuts: "Phím tắt",
    settings: "Cài đặt",
  },
});

/** Help > Keyboard shortcuts (`Mod+/`): every app shortcut of doc 01 section 8. */
export function ShortcutsDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT(wcStrings);
  const n = useT(names);
  const c = useT(commonStrings);
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("shortcutsTitle")}
      footer={<Button onClick={onClose}>{c("close")}</Button>}
    >
      <dl className="grid max-h-[60vh] grid-cols-[1fr_auto] gap-x-4 gap-y-1 overflow-auto text-[13px]">
        {(Object.keys(SHORTCUTS) as ShortcutId[]).map((id) => (
          <div key={id} className="contents">
            <dt>{n(id)}</dt>
            <dd className="font-mono text-muted-text">{shortcutLabel(id)}</dd>
          </div>
        ))}
      </dl>
    </Dialog>
  );
}
