import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Button } from "../../components/ui/button";
import { Input } from "../../components/ui/fields";
import { SafeText } from "../../components/ui/safe-text";
import { useToast } from "../../components/ui/toast";
import type { TokenStatus } from "../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../ipc/client";
import { queryKeys } from "../../ipc/query-keys";
import { useT } from "../../lib/i18n/i18n";
import { settingValue } from "../remote/settings/settings-model";
import { Check, ConfirmAction, type PageProps } from "../remote/settings/settings-pages";
import { UpdateDialog, useAppVersion } from "./update-dialog";
import { type UpdateKey, updateStrings } from "./update-strings";

/** Stored update token state; the token itself never comes back from the backend. */
export function useUpdaterToken() {
  return useQuery({
    queryKey: queryKeys.updaterToken,
    queryFn: () => unwrap(ipc().commands.updaterTokenStatus()),
  });
}

const STATUS_TEXT: Record<TokenStatus, UpdateKey> = {
  missing: "tokenMissing",
  stored: "tokenStored",
  rejected: "tokenRejected",
};

/**
 * Settings › Updates: current version, the release access token (write-only: pasted, saved to the
 * OS credential store, then only its status is shown), the daily check setting and "Check for
 * updates". Token changes apply at once; the daily check setting is saved with Save.
 */
export function UpdatesPage({ settings, draft, set }: PageProps) {
  const t = useT(updateStrings);
  const toast = useToast();
  const qc = useQueryClient();
  const version = useAppVersion();
  const status = useUpdaterToken();
  const tokenId = useId();
  const statusId = useId();
  const [token, setToken] = useState("");
  const [checking, setChecking] = useState(false);

  const saveToken = useMutation({
    mutationFn: (value: string) => unwrap(ipc().commands.updaterTokenSet(value)),
    onSuccess: () => {
      // The field is emptied at once: the token is never kept or shown in the page.
      setToken("");
      toast.show(t("tokenSaved"));
    },
    onSettled: () => void qc.invalidateQueries({ queryKey: queryKeys.updaterToken }),
  });
  const clearToken = useMutation({
    mutationFn: () => unwrap(ipc().commands.updaterTokenClear()),
    onSuccess: () => toast.show(t("tokenCleared")),
    onError: (e) => toast.show(toAppError(e).message),
    onSettled: () => void qc.invalidateQueries({ queryKey: queryKeys.updaterToken }),
  });

  const state = status.data;
  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-baseline gap-2">
        <span className="text-muted-text">{t("currentVersion")}</span>
        <SafeText className="font-mono font-semibold">{version.data ?? "…"}</SafeText>
      </div>

      <section aria-labelledby={`${tokenId}-title`} className="flex flex-col gap-2">
        <h3 id={`${tokenId}-title`} className="m-0 text-[13px] font-semibold">
          {t("tokenTitle")}
        </h3>
        <p className="m-0 text-muted-text">{t("tokenIntro")}</p>
        <p className="m-0" id={statusId}>
          <span className="text-muted-text">{t("tokenStatus")}: </span>
          <span
            className={state === "rejected" ? "font-semibold text-bad-text" : "font-semibold"}
            data-token-status={state}
          >
            {state ? t(STATUS_TEXT[state]) : "…"}
          </span>
        </p>
        <form
          className="flex flex-wrap items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (token.trim()) saveToken.mutate(token);
          }}
        >
          <label className="flex min-w-0 flex-1 flex-col gap-1" htmlFor={tokenId}>
            <span>{t("tokenLabel")}</span>
            <Input
              id={tokenId}
              type="password"
              autoComplete="off"
              spellCheck={false}
              aria-describedby={statusId}
              value={token}
              onChange={(e) => {
                if (saveToken.error) saveToken.reset();
                setToken(e.target.value);
              }}
              className="font-mono"
            />
          </label>
          <Button type="submit" disabled={!token.trim() || saveToken.isPending}>
            {t("tokenSave")}
          </Button>
          {state && state !== "missing" && (
            <ConfirmAction
              label={t("tokenClear")}
              question={t("tokenClearConfirm")}
              disabled={clearToken.isPending}
              onConfirm={() => clearToken.mutate()}
            />
          )}
        </form>
        {saveToken.error && (
          <p className="m-0 text-bad-text" role="alert">
            <SafeText>
              {t("tokenSaveFailed", { message: toAppError(saveToken.error).message })}
            </SafeText>
          </p>
        )}
      </section>

      <div className="flex flex-col gap-1">
        <Check
          label={t("autoCheck")}
          checked={settingValue(settings, draft, "updates.autoCheck")}
          onChange={(on) => set({ "updates.autoCheck": on })}
        />
        <span className="text-[11px] text-muted-text">{t("autoCheckHint")}</span>
      </div>

      <div>
        <Button onClick={() => setChecking(true)}>{t("checkNow")}</Button>
      </div>
      <UpdateDialog open={checking} onClose={() => setChecking(false)} />
    </div>
  );
}
