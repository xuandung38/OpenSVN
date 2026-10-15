import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { DiffSideKind, DiffTarget, FileDiff, OfficeText } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
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
    legacyHint: "Set an external diff tool for this file type in Settings.",
    openBase: "Open base version",
    openWorking: "Open working version",
    openRev: "Open r{rev}",
    opening: "Opening…",
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
    legacyHint: "Hãy đặt công cụ diff ngoài cho loại file này trong Cài đặt.",
    openBase: "Mở bản gốc",
    openWorking: "Mở bản đang sửa",
    openRev: "Mở bản r{rev}",
    opening: "Đang mở…",
  },
});

type OfficeKey = keyof (typeof officeStrings)["en"];
type Translate = (key: OfficeKey, vars?: Record<string, number>) => string;

/**
 * Office files the default application may open (the backend's allow-list: no macro-enabled and
 * no legacy binary formats).
 */
const OPENABLE = /\.(docx|xlsx|pptx)$/i;

/** Labels of the older and the newer version of a diff target. */
function versionLabels(target: DiffTarget, t: Translate): [string, string] {
  switch (target.kind) {
    case "wc":
      return [t("openBase"), t("openWorking")];
    case "wcVsRev":
      return [t("openRev", { rev: target.rev }), t("openWorking")];
    case "rev":
      return [
        t("openRev", { rev: Math.max(target.rev - 1, 0) }),
        t("openRev", { rev: target.rev }),
      ];
    case "range":
      return [t("openRev", { rev: target.from }), t("openRev", { rev: target.to })];
  }
}

/**
 * Note above the diff of a Word or Excel file: the sides are extracted text (not the formatting),
 * or why no text could be extracted. With a `target`, it also offers "Open rX" / "Open rY": each
 * version as a read-only copy in its default app, so Word or Excel can compare them itself.
 */
export function OfficeNote({
  office,
  diff,
  target,
}: {
  office: OfficeText;
  diff: FileDiff;
  target?: DiffTarget;
}) {
  const t = useT(officeStrings);
  const openable = target !== undefined && OPENABLE.test(diff.path);
  const text = office.extracted
    ? t(office.kind === "word" ? "extractedWord" : "extractedExcel")
    : `${office.reason ?? ""}. ${t(openable ? "failedHint" : "legacyHint")}`;
  return (
    <div
      role="note"
      className="flex flex-wrap items-center gap-2 border-b border-line bg-surface-2 px-3 py-2 text-[12px]"
    >
      <b className={office.extracted ? "text-info-text" : "text-warn-text"}>
        {t(office.extracted ? "extractedTitle" : "failedTitle")}
      </b>
      <SafeText className="min-w-0 flex-1">{text}</SafeText>
      {openable && <VersionButtons target={target} diff={diff} known={office.extracted} />}
    </div>
  );
}

/** One button per version; a side the extracted diff shows as missing (added/deleted) has none. */
function VersionButtons({
  target,
  diff,
  known,
}: {
  target: DiffTarget;
  diff: FileDiff;
  known: boolean;
}) {
  const t = useT(officeStrings);
  const toast = useToast();
  const [busy, setBusy] = useState<DiffSideKind | null>(null);
  const labels = versionLabels(target, t);
  const sides = (["left", "right"] as const).filter(
    (s) => !known || (s === "left" ? diff.left : diff.right) !== null,
  );
  const open = async (side: DiffSideKind) => {
    setBusy(side);
    try {
      await unwrap(ipc().commands.diffOpenVersion(target, side));
    } catch (e) {
      toast.show(toAppError(e).message, 6000);
    } finally {
      setBusy(null);
    }
  };
  return (
    <span className="flex gap-1.5">
      {sides.map((side) => (
        <Button key={side} small disabled={busy !== null} onClick={() => void open(side)}>
          {busy === side ? t("opening") : labels[side === "left" ? 0 : 1]}
        </Button>
      ))}
    </span>
  );
}
