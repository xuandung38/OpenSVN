import type { ReactNode } from "react";
import { SafeText } from "../../../components/ui/safe-text";
import type { OfficeText } from "../../../ipc/bindings";
import { defineStrings, useT } from "../../../lib/i18n/i18n";

export const officeStrings = defineStrings({
  en: {
    extractedTitle: "Text comparison",
    extractedWord:
      "Comparing the text extracted from both Word documents, not their formatting, pictures or layout.",
    extractedExcel:
      "Comparing the cell values and formulas extracted from both workbooks, not their formatting or charts.",
    failedTitle: "No text comparison",
    failedHint:
      "Open each version in its default app (for example Word's Compare), or set an external diff tool for this file type in Settings.",
  },
  vi: {
    extractedTitle: "So sánh nội dung chữ",
    extractedWord:
      "Đang so sánh nội dung chữ đã trích từ hai tài liệu Word, không so sánh định dạng, hình ảnh hay bố cục.",
    extractedExcel:
      "Đang so sánh giá trị ô và công thức đã trích từ hai bảng tính, không so sánh định dạng hay biểu đồ.",
    failedTitle: "Không so sánh được nội dung chữ",
    failedHint:
      "Hãy mở từng phiên bản bằng ứng dụng mặc định (ví dụ tính năng Compare của Word), hoặc đặt công cụ diff ngoài cho loại file này trong Cài đặt.",
  },
});

/**
 * Note above the diff of a Word or Excel file: says the sides are extracted text (not the
 * formatting), or why no text could be extracted. `actions` holds the "open version" buttons.
 */
export function OfficeNote({ office, actions }: { office: OfficeText; actions?: ReactNode }) {
  const t = useT(officeStrings);
  const text = office.extracted
    ? t(office.kind === "word" ? "extractedWord" : "extractedExcel")
    : `${office.reason ?? ""}. ${t("failedHint")}`;
  return (
    <div
      role="note"
      className="flex flex-wrap items-center gap-2 border-b border-line bg-surface-2 px-3 py-2 text-[12px]"
    >
      <b className={office.extracted ? "text-info-text" : "text-warn-text"}>
        {t(office.extracted ? "extractedTitle" : "failedTitle")}
      </b>
      <SafeText className="min-w-0 flex-1">{text}</SafeText>
      {actions && <span className="flex gap-1.5">{actions}</span>}
    </div>
  );
}
