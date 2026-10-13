import { defineStrings } from "../../../lib/i18n/i18n";

export const blameStrings = defineStrings({
  en: {
    title: "Blame",
    running: "Computing blame…",
    failed: "Blame failed",
    colRev: "Rev",
    colAuthor: "Author",
    colDate: "Date",
    colLine: "Line",
    empty: "This file has no lines",
    openRevision: "Open r{rev} in History",
  },
  vi: {
    title: "Blame",
    running: "Đang tính blame…",
    failed: "Blame thất bại",
    colRev: "Rev",
    colAuthor: "Tác giả",
    colDate: "Ngày",
    colLine: "Dòng",
    empty: "File không có dòng nào",
    openRevision: "Mở r{rev} trong History",
  },
});
