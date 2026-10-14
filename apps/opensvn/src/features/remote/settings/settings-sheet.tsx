import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { Sheet } from "../../../components/ui/sheet";
import { useToast } from "../../../components/ui/toast";
import { IpcError, ipc, unwrap } from "../../../ipc/client";
import { useSettings } from "../../../ipc/queries";
import { queryKeys } from "../../../ipc/query-keys";
import { cn } from "../../../lib/cn";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { applyTheme } from "../../../lib/theme";
import {
  type Draft,
  type FieldError,
  rejectedKey,
  validateDraft,
  withoutUnchanged,
} from "./settings-model";
import {
  AccountsPage,
  AdvancedPage,
  AppearancePage,
  DiffMergePage,
  GeneralPage,
  IntegrationsPage,
  NetworkPage,
  type PageProps,
} from "./settings-pages";
import { settingsStrings } from "./settings-strings";

type PageId =
  | "general"
  | "accounts"
  | "appearance"
  | "diffMerge"
  | "integrations"
  | "network"
  | "advanced";
/**
 * Footer text for a failed Save. A refused value is explained on its field; the backend's own
 * validation message (English, with the dotted key) is never shown.
 */
function saveErrorText(
  e: Error,
  onField: boolean,
  t: (key: "saveFailed" | "saveFailedField") => string,
): string {
  if (onField) return t("saveFailedField");
  if (e instanceof IpcError && e.appError.code === "invalid-argument") return t("saveFailed");
  return `${t("saveFailed")}: ${e.message}`;
}

/** The page that shows a setting, from its dotted key's section. */
function pageOf(key: string): PageId {
  const section = key.split(".")[0] ?? "";
  if (section === "diff" || section === "tools") return "diffMerge";
  if (section === "cache" || section === "wc") return "advanced";
  return (PAGES as string[]).includes(section) ? (section as PageId) : "general";
}

const PAGES: PageId[] = [
  "general",
  "accounts",
  "appearance",
  "diffMerge",
  "integrations",
  "network",
  "advanced",
];

/** Settings sheet (doc 01, section 9): nav on the left, page on the right, Save applies the draft once. */
export function SettingsSheet({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT(settingsStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const qc = useQueryClient();
  const settings = useSettings();
  const [page, setPage] = useState<PageId>("general");
  const [draft, setDraft] = useState<Draft>({});

  const save = useMutation({
    mutationFn: (patch: Draft) => unwrap(ipc().commands.settingsSet(patch)),
    onSuccess: (next, patch) => {
      qc.setQueryData(queryKeys.settings, next);
      if (patch["appearance.theme"]) applyTheme(patch["appearance.theme"]);
      toast.show(t("saved"));
      setDraft({});
      onClose();
    },
    // A value the backend refused is shown on its field, on its page.
    onError: (e) => {
      const key = rejectedKey(e, settings.data);
      if (key) setPage(pageOf(key));
    },
  });

  const s = settings.data;
  const rejected = rejectedKey(save.error, s);
  const errors: Record<string, FieldError> = {
    ...(rejected ? { [rejected]: "rejected" as const } : {}),
    ...(s ? validateDraft(s, draft) : {}),
  };
  const dirty = Object.keys(draft).length > 0;
  const canSave = !!s && dirty && Object.keys(errors).length === 0 && !save.isPending;

  const close = () => {
    setDraft({});
    save.reset();
    onClose();
  };

  const props: PageProps | null = s
    ? {
        settings: s,
        draft,
        errors,
        set: (p) => {
          // Editing again clears a refusal from the last Save.
          if (save.error) save.reset();
          setDraft((d) => withoutUnchanged(s, { ...d, ...p }));
        },
      }
    : null;

  return (
    <Sheet
      open={open}
      onClose={close}
      title={t("title")}
      footer={
        <>
          {save.error && (
            <span className="mr-auto text-bad-text" role="alert">
              {saveErrorText(save.error, rejected !== null, t)}
            </span>
          )}
          <Button onClick={close}>{c("cancel")}</Button>
          <Button variant="primary" disabled={!canSave} onClick={() => save.mutate(draft)}>
            {t("save")}
          </Button>
        </>
      }
    >
      <div className="grid h-full min-h-0 grid-cols-[230px_1fr]">
        <nav aria-label={t("nav")} className="flex flex-col gap-0.5 border-r border-line p-2">
          {PAGES.map((p) => (
            <button
              key={p}
              type="button"
              aria-current={p === page ? "page" : undefined}
              onClick={() => setPage(p)}
              className={cn(
                "cursor-pointer rounded-md border-0 bg-transparent px-2.5 py-1.5 text-left",
                p === page ? "bg-sel font-semibold" : "hover:bg-surface-2",
              )}
            >
              {t(p)}
            </button>
          ))}
        </nav>
        <div className="min-h-0 overflow-auto p-4">
          {!props ? (
            <p className="m-0 text-muted-text">{settings.error ? settings.error.message : "…"}</p>
          ) : page === "accounts" ? (
            <AccountsPage />
          ) : page === "general" ? (
            <GeneralPage {...props} />
          ) : page === "appearance" ? (
            <AppearancePage {...props} />
          ) : page === "diffMerge" ? (
            <DiffMergePage {...props} />
          ) : page === "integrations" ? (
            <IntegrationsPage {...props} />
          ) : page === "network" ? (
            <NetworkPage {...props} />
          ) : (
            <AdvancedPage {...props} />
          )}
        </div>
      </div>
    </Sheet>
  );
}
