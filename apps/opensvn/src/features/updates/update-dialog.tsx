import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useEventBus } from "../../app/providers";
import { Button } from "../../components/ui/button";
import { BadBox, Dialog } from "../../components/ui/dialog";
import { SafeText } from "../../components/ui/safe-text";
import type { AppError, UpdateProgress } from "../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../ipc/client";
import { queryKeys } from "../../ipc/query-keys";
import { formatBytes } from "../../lib/format";
import { commonStrings } from "../../lib/i18n/common-strings";
import { useT } from "../../lib/i18n/i18n";
import { percentOf, updateErrorKey } from "./update-model";
import { updateStrings } from "./update-strings";

/** Version of the running app (fixed for its lifetime). */
export function useAppVersion() {
  return useQuery({
    queryKey: queryKeys.appVersion,
    staleTime: Number.POSITIVE_INFINITY,
    queryFn: () => unwrap(ipc().commands.appVersion()),
  });
}

/** Localised text of a failed check or install. */
function useErrorText() {
  const t = useT(updateStrings);
  return (e: AppError) => {
    const key = updateErrorKey(e);
    return key ? t(key) : e.message;
  };
}

/**
 * Help › Check for app updates… and Settings › Updates › Check for updates: checks when opened,
 * then shows exactly one outcome: up to date, a new version (notes, Install with progress, then
 * the restart prompt), not configured (no request was sent), or an error with Retry.
 */
export function UpdateDialog({
  open,
  onClose,
  onOpenSettings,
}: {
  open: boolean;
  onClose: () => void;
  /** Shown with "not configured" when the dialog was not opened from Settings. */
  onOpenSettings?: () => void;
}) {
  const t = useT(updateStrings);
  const c = useT(commonStrings);
  const errorText = useErrorText();
  const qc = useQueryClient();
  const bus = useEventBus();
  const version = useAppVersion();
  const [progress, setProgress] = useState<UpdateProgress | null>(null);

  const check = useMutation({
    mutationFn: () => unwrap(ipc().commands.updateCheck()),
    // A refusal marks the token invalid (and a success clears that): Settings shows it.
    onSettled: () => void qc.invalidateQueries({ queryKey: queryKeys.updaterToken }),
  });
  const install = useMutation({
    mutationFn: () => unwrap(ipc().commands.updateInstall()),
    // A second attempt starts from zero, not from where the failed one stopped.
    onMutate: () => setProgress(null),
  });
  const restart = useMutation({
    mutationFn: () => unwrap(ipc().commands.updateRestart()),
  });

  // Each opening checks again; closing forgets the outcome.
  const { mutate: runCheck, reset: resetCheck } = check;
  const { reset: resetInstall } = install;
  const { reset: resetRestart } = restart;
  useEffect(() => {
    if (open) {
      runCheck();
    } else {
      resetCheck();
      resetInstall();
      resetRestart();
      setProgress(null);
    }
  }, [open, runCheck, resetCheck, resetInstall, resetRestart]);

  useEffect(() => bus.on("app://update-progress", setProgress), [bus]);

  // The primary action of each step gets the focus (the dialog itself focuses the first button);
  // while downloading, the progress text has it, so focus never falls out of the dialog.
  const primary = useRef<HTMLButtonElement>(null);
  const downloading = useRef<HTMLDivElement>(null);
  const step = install.isSuccess
    ? "installed"
    : install.isPending
      ? "installing"
      : (check.data?.kind ?? (check.isError ? "error" : "checking"));
  useEffect(() => {
    if (!open || step === "checking") return;
    (step === "installing" ? downloading.current : primary.current)?.focus();
  }, [open, step]);

  const busy = install.isPending || restart.isPending;
  const close = () => {
    // The download cannot be stopped halfway; the dialog stays until it ends.
    if (!busy) onClose();
  };
  const current = version.data ?? "…";
  const result = check.data;
  const percent = progress ? percentOf(progress.downloaded, progress.total) : null;

  let body: React.ReactNode;
  let footer: React.ReactNode;
  if (install.isSuccess) {
    body = <p className="m-0">{t("installed")}</p>;
    footer = (
      <>
        <Button onClick={close} disabled={restart.isPending}>
          {t("later")}
        </Button>
        <Button
          ref={primary}
          variant="primary"
          disabled={restart.isPending}
          onClick={() => restart.mutate()}
        >
          {t("restartNow")}
        </Button>
      </>
    );
    if (restart.error) {
      body = (
        <>
          {body}
          <BadBox>
            <p className="m-0" role="alert">
              <SafeText>{toAppError(restart.error).message}</SafeText>
            </p>
          </BadBox>
        </>
      );
    }
  } else if (install.isPending) {
    body = (
      <div
        ref={downloading}
        tabIndex={-1}
        className="flex flex-col gap-2 outline-none"
        aria-busy="true"
      >
        <p className="m-0">{t("installing")}</p>
        <progress
          aria-label={t("progressLabel")}
          className="h-2 w-full accent-[var(--accent)]"
          max={100}
          {...(percent === null ? {} : { value: percent })}
        />
        <p className="m-0 text-[12px] text-muted-text">
          {progress
            ? progress.total
              ? t("installProgress", {
                  done: formatBytes(progress.downloaded),
                  total: formatBytes(progress.total),
                })
              : t("installProgressUnknown", { done: formatBytes(progress.downloaded) })
            : " "}
        </p>
      </div>
    );
    footer = (
      <Button variant="primary" disabled>
        {t("install")}
      </Button>
    );
  } else if (check.isPending || (!result && !check.isError)) {
    body = (
      <p className="m-0" role="status" aria-busy="true">
        {t("checking")}
      </p>
    );
    footer = <Button onClick={close}>{c("close")}</Button>;
  } else if (check.isError) {
    body = (
      <BadBox>
        <p className="m-0 font-semibold">{t("checkFailed")}</p>
        <p className="m-0" role="alert">
          <SafeText>{errorText(toAppError(check.error))}</SafeText>
        </p>
      </BadBox>
    );
    footer = (
      <>
        <Button onClick={close}>{c("close")}</Button>
        <Button ref={primary} variant="primary" onClick={() => check.mutate()}>
          {c("retry")}
        </Button>
      </>
    );
  } else if (result?.kind === "available") {
    body = (
      <>
        <p className="m-0 font-semibold">{t("available", { version: result.version })}</p>
        <p className="m-0 text-muted-text">{t("youHave", { version: current })}</p>
        <section aria-label={t("notes")} className="flex flex-col gap-1">
          <h3 className="m-0 text-[13px] font-semibold">{t("notes")}</h3>
          {result.notes?.trim() ? (
            <div
              // biome-ignore lint/a11y/noNoninteractiveTabindex: a scrollable region must be reachable by keyboard (axe scrollable-region-focusable)
              tabIndex={0}
              className="max-h-60 overflow-auto rounded-md border border-line bg-surface-2 px-2.5 py-2 font-mono text-[12px] whitespace-pre-wrap"
            >
              <SafeText>{result.notes}</SafeText>
            </div>
          ) : (
            <p className="m-0 text-muted-text">{t("noNotes")}</p>
          )}
        </section>
        {install.error && (
          <BadBox>
            <p className="m-0" role="alert">
              <SafeText>
                {t("installFailed", { message: errorText(toAppError(install.error)) })}
              </SafeText>
            </p>
          </BadBox>
        )}
      </>
    );
    footer = (
      <>
        <Button onClick={close}>{t("later")}</Button>
        <Button ref={primary} variant="primary" onClick={() => install.mutate()}>
          {install.error ? c("retry") : t("install")}
        </Button>
      </>
    );
  } else if (result?.kind === "notConfigured") {
    body = (
      <p className="m-0">
        {result.reason === "token" ? t("notConfiguredToken") : t("notConfiguredBuild")}
      </p>
    );
    footer = (
      <>
        <Button
          ref={onOpenSettings && result.reason === "token" ? undefined : primary}
          onClick={close}
        >
          {c("close")}
        </Button>
        {onOpenSettings && result.reason === "token" && (
          <Button
            ref={primary}
            variant="primary"
            onClick={() => {
              onClose();
              onOpenSettings();
            }}
          >
            {t("openSettings")}
          </Button>
        )}
      </>
    );
  } else {
    body = <p className="m-0">{t("upToDate", { version: current })}</p>;
    footer = (
      <Button ref={primary} variant="primary" onClick={close}>
        {c("close")}
      </Button>
    );
  }

  return (
    <Dialog open={open} onClose={close} title={t("dialogTitle")} footer={footer}>
      {body}
    </Dialog>
  );
}
