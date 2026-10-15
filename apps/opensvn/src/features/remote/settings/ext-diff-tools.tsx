import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { Input } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { Settings } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { defineStrings, useT } from "../../../lib/i18n/i18n";

const strings = defineStrings({
  en: {
    title: "Diff tools by file type",
    note: "Used instead of the tool above for these files, including binary ones such as .docx or .xlsx. The program is chosen in the system file picker and confirmed before it is saved.",
    extension: "File extension",
    extensionPlaceholder: "docx",
    add: "Choose program…",
    remove: "Remove",
    none: "No file type has its own tool.",
  },
  vi: {
    title: "Công cụ diff theo loại file",
    note: "Dùng thay cho công cụ ở trên với các file này, kể cả file nhị phân như .docx hay .xlsx. Chương trình được chọn bằng hộp thoại chọn file của hệ điều hành và phải xác nhận trước khi lưu.",
    extension: "Đuôi file",
    extensionPlaceholder: "docx",
    add: "Chọn chương trình…",
    remove: "Xóa",
    none: "Chưa có loại file nào dùng công cụ riêng.",
  },
});

/** Arguments of a new tool: the two copies, older then newer. */
const DEFAULT_ARGS = ["{left}", "{right}"];

/**
 * Per-extension diff tools (`diff.toolsByExtension`). Like the global tool, only the extension
 * and the arguments cross the IPC boundary: Rust shows the native picker and the confirmation.
 */
export function ExtDiffTools({ settings }: { settings: Settings }) {
  const t = useT(strings);
  const toast = useToast();
  const qc = useQueryClient();
  const [ext, setExt] = useState("");
  const tools = settings["diff.toolsByExtension"] ?? [];
  const change = useMutation({
    mutationFn: (op: { add: boolean; ext: string }) =>
      unwrap(
        op.add
          ? ipc().commands.programChooseForExtension(op.ext, DEFAULT_ARGS)
          : ipc().commands.programClearForExtension(op.ext),
      ),
    onSuccess: (next) => {
      qc.setQueryData(queryKeys.settings, next);
      setExt("");
    },
    onError: (e) => toast.show(e.message),
  });
  const trimmed = ext.trim().replace(/^\./, "");
  return (
    <section aria-label={t("title")} className="flex flex-col gap-2">
      <span>{t("title")}</span>
      <p className="m-0 text-[12px] text-muted-text">{t("note")}</p>
      {tools.length === 0 ? (
        <p className="m-0 text-[12px] text-muted-text">{t("none")}</p>
      ) : (
        <ul className="m-0 flex list-none flex-col gap-1 p-0">
          {tools.map((tool) => (
            <li key={tool.ext} className="flex items-center gap-2">
              <b className="w-16 shrink-0 font-mono text-[12px]">.{tool.ext}</b>
              <SafeText className="min-w-0 flex-1 truncate font-mono text-[12px]">
                {`${tool.path} ${tool.args.join(" ")}`}
              </SafeText>
              <Button
                small
                disabled={change.isPending}
                onClick={() => change.mutate({ add: false, ext: tool.ext })}
              >
                {t("remove")}
              </Button>
            </li>
          ))}
        </ul>
      )}
      <div className="flex gap-2">
        <Input
          aria-label={t("extension")}
          placeholder={t("extensionPlaceholder")}
          value={ext}
          spellCheck={false}
          onChange={(e) => setExt(e.target.value)}
          className="w-28 font-mono text-[12px]"
        />
        <Button
          disabled={change.isPending || !/^[A-Za-z0-9]{1,16}$/.test(trimmed)}
          onClick={() => change.mutate({ add: true, ext: trimmed })}
        >
          {t("add")}
        </Button>
      </div>
    </section>
  );
}
