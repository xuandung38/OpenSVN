import { defineStrings } from "../lib/i18n/i18n";

/** Strings of the main-window tab bar (the rest of the shell is localised in the i18n pass). */
export const shellStrings = defineStrings({
  en: {
    views: "Views",
    branch: "Branch",
    location: "Location",
    workingCopies: "Working copies",
    changes: "Changes",
    history: "History",
    browse: "Browse",
    selectFile: "Select a file",
    selectFileHint: "Pick a changed file in the sidebar to see its diff here.",
    noWorkingCopy: "No working copy",
    noWorkingCopyHint: "Add a working copy to see its history and browse the repository.",
    demoEngine: "Demo engine — not connected to real servers",
    demoEngineText:
      "This build runs the built-in demo repository. Nothing you do here reaches a real Subversion server.",
  },
  vi: {
    views: "Chế độ xem",
    branch: "Branch",
    location: "Vị trí",
    workingCopies: "Working copy",
    changes: "Thay đổi",
    history: "Lịch sử",
    browse: "Duyệt",
    selectFile: "Chọn một file",
    selectFileHint: "Chọn file đã thay đổi ở thanh bên để xem diff tại đây.",
    noWorkingCopy: "Chưa có working copy",
    noWorkingCopyHint: "Thêm working copy để xem lịch sử và duyệt repository.",
    demoEngine: "Engine demo — không kết nối tới server thật",
    demoEngineText:
      "Bản build này chạy repository demo có sẵn. Mọi thao tác ở đây không tới server Subversion thật.",
  },
});
