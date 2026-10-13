import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { Sheet } from "../../../components/ui/sheet";
import { useToast } from "../../../components/ui/toast";
import { ipc, unwrap } from "../../../ipc/client";
import { useSettings } from "../../../ipc/queries";
import { queryKeys } from "../../../ipc/query-keys";
import { cn } from "../../../lib/cn";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { applyTheme } from "../../../lib/theme";
import { type Draft, validateDraft, withoutUnchanged } from "./settings-model";
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
  });

  const s = settings.data;
  const errors = s ? validateDraft(s, draft) : {};
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
        set: (p) => setDraft((d) => withoutUnchanged(s, { ...d, ...p })),
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
              {t("saveFailed")}: {save.error.message}
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
