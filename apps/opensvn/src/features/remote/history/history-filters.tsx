import { X } from "lucide-react";
import { Chip } from "../../../components/ui/chip";
import { Input, Select } from "../../../components/ui/fields";
import { useT } from "../../../lib/i18n/i18n";
import { historyStrings } from "./history-strings";
import type { HistoryFilters } from "./use-history";

/** Filter bar: text (server-side), author and date range (client-side on loaded pages), stop-on-copy. */
export function HistoryFilterBar({
  filters,
  authors,
  onChange,
  scopeLabel,
  onClearScope,
}: {
  filters: HistoryFilters;
  authors: string[];
  onChange: (f: HistoryFilters) => void;
  scopeLabel: string | null;
  onClearScope?: () => void;
}) {
  const t = useT(historyStrings);
  const set = (patch: Partial<HistoryFilters>) => onChange({ ...filters, ...patch });
  return (
    <div className="flex flex-col gap-1.5 border-b border-line px-2.5 py-2">
      {scopeLabel && (
        <div className="flex items-center gap-1.5 text-[11px]">
          <span className="text-muted-text">{t("scope")}:</span>
          <span className="flex min-w-0 items-center gap-1 rounded-[10px] bg-accent-soft px-2 py-0.5 text-accent-text">
            <span className="truncate font-mono">{scopeLabel}</span>
            {onClearScope && (
              <button
                type="button"
                onClick={onClearScope}
                aria-label={t("clearScope")}
                className="flex cursor-pointer border-0 bg-transparent p-0 text-accent-text"
              >
                <X size={12} />
              </button>
            )}
          </span>
        </div>
      )}
      <Input
        aria-label={t("filterLabel")}
        placeholder={t("filterPlaceholder")}
        value={filters.text}
        onChange={(e) => set({ text: e.target.value })}
      />
      <div className="grid grid-cols-2 gap-1.5">
        <Select
          aria-label={t("author")}
          value={filters.author}
          onChange={(e) => set({ author: e.target.value })}
        >
          <option value="">{t("allAuthors")}</option>
          {authors.map((a) => (
            <option key={a} value={a}>
              {a}
            </option>
          ))}
        </Select>
        <Chip on={filters.stopOnCopy} onClick={() => set({ stopOnCopy: !filters.stopOnCopy })}>
          {t("stopOnCopy")}
        </Chip>
        <label className="flex items-center gap-1.5 text-[11px] text-muted-text">
          {t("from")}
          <Input type="date" value={filters.from} onChange={(e) => set({ from: e.target.value })} />
        </label>
        <label className="flex items-center gap-1.5 text-[11px] text-muted-text">
          {t("to")}
          <Input type="date" value={filters.to} onChange={(e) => set({ to: e.target.value })} />
        </label>
      </div>
      <span className="text-[11px] text-muted-text">{t("dateHint")}</span>
    </div>
  );
}
