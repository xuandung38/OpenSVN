import { cn } from "../../lib/cn";

export interface SegmentOption<V extends string> {
  value: V;
  label: string;
}

/** Bordered toggle group (Split/Unified, macOS/Windows). Active segment: accent-soft bg, accent text. */
export function SegmentedControl<V extends string>({
  options,
  value,
  onChange,
  label,
}: {
  options: SegmentOption<V>[];
  value: V;
  onChange: (v: V) => void;
  label: string;
}) {
  return (
    <fieldset
      aria-label={label}
      className="m-0 inline-flex min-w-0 overflow-hidden rounded-[7px] border border-line bg-surface p-0"
    >
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          aria-pressed={o.value === value}
          onClick={() => onChange(o.value)}
          className={cn(
            "cursor-pointer border-0 px-3 py-1.5",
            o.value === value
              ? "bg-accent-soft font-semibold text-accent"
              : "bg-transparent text-muted",
          )}
        >
          {o.label}
        </button>
      ))}
    </fieldset>
  );
}
