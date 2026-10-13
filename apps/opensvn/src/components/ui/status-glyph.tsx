import { cn } from "../../lib/cn";
import { STATUS_LABEL, type StatusCode } from "../../lib/status";

const COLOR: Record<StatusCode, string> = {
  M: "text-warn-text",
  A: "text-ok-text",
  D: "text-bad-text",
  R: "text-info-text",
  "?": "text-muted-text",
  "!": "text-bad-text",
  C: "bg-warn text-on-warn rounded-[4px]",
  P: "text-muted-text",
};

/** Mono status letter centred in a 20px cell. Always paired with a title/aria-label. */
export function StatusGlyph({ code, className }: { code: StatusCode; className?: string }) {
  return (
    <span
      role="img"
      aria-label={STATUS_LABEL[code]}
      title={STATUS_LABEL[code]}
      className={cn(
        "inline-flex h-[18px] w-5 items-center justify-center font-mono text-[12px] font-medium",
        COLOR[code],
        className,
      )}
    >
      {code}
    </span>
  );
}
