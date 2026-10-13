import { type ReactNode, useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { Dialog } from "../../../components/ui/dialog";
import { SafeText } from "../../../components/ui/safe-text";
import { toAppError } from "../../../ipc/client";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";

/**
 * Form dialog shell: Cancel + submit footer, pending state and the backend error shown inline (a
 * failed command keeps the dialog open so the user can adjust and retry). `onSubmit` closes the
 * dialog itself on success.
 */
export function ActionDialog({
  title,
  submitLabel,
  danger = false,
  canSubmit = true,
  onSubmit,
  onClose,
  children,
}: {
  title: string;
  submitLabel: string;
  danger?: boolean;
  canSubmit?: boolean;
  onSubmit: () => Promise<void>;
  onClose: () => void;
  children: ReactNode;
}) {
  const c = useT(commonStrings);
  const formId = useId();
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    if (!canSubmit || pending) return;
    setPending(true);
    setError(null);
    try {
      await onSubmit();
    } catch (e) {
      setError(toAppError(e).message);
    } finally {
      setPending(false);
    }
  };

  return (
    <Dialog
      open
      onClose={onClose}
      title={title}
      footer={
        <>
          <Button onClick={onClose}>{c("cancel")}</Button>
          <Button
            type="submit"
            form={formId}
            variant={danger ? "danger" : "primary"}
            disabled={!canSubmit || pending}
          >
            {submitLabel}
          </Button>
        </>
      }
    >
      <form
        id={formId}
        className="flex flex-col gap-2.5"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        {children}
        {error && (
          <SafeText as="p" className="m-0 break-words text-bad-text">
            <span role="alert">{error}</span>
          </SafeText>
        )}
      </form>
    </Dialog>
  );
}

/** Scrollable list of affected paths, rendered as text. */
export function PathList({ paths, label }: { paths: readonly string[]; label: string }) {
  return (
    <ul
      aria-label={label}
      className="m-0 max-h-32 list-none overflow-auto rounded-md border border-line bg-surface-2 p-1.5 font-mono text-[12px]"
    >
      {paths.map((p) => (
        <li key={p} className="truncate" title={p}>
          <SafeText>{p}</SafeText>
        </li>
      ))}
    </ul>
  );
}
