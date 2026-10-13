import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { Button } from "../../../components/ui/button";
import { Input, Select } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { SegmentedControl } from "../../../components/ui/segmented-control";
import { useToast } from "../../../components/ui/toast";
import type { Settings } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { useT } from "../../../lib/i18n/i18n";
import { type Draft, settingValue } from "./settings-model";
import { settingsStrings } from "./settings-strings";

export interface PageProps {
  settings: Settings;
  draft: Draft;
  errors: Record<string, "https" | "range" | "proxy">;
  set: (patch: Draft) => void;
}

function Field({
  label,
  error,
  hint,
  children,
}: {
  label: string;
  error?: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <div className="flex flex-col gap-1">
      {/* biome-ignore lint/a11y/noLabelWithoutControl: the control is passed as children */}
      <label className="flex flex-col gap-1">
        <span>{label}</span>
        {children}
      </label>
      {hint && <span className="text-[11px] text-muted-text">{hint}</span>}
      {error && (
        <span className="text-bad-text" role="alert">
          {error}
        </span>
      )}
    </div>
  );
}

function Check({
  label,
  checked,
  onChange,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label className="flex items-center gap-2">
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        className="accent-[var(--accent)]"
      />
      {label}
    </label>
  );
}

const num = (s: string) => (s.trim() === "" ? Number.NaN : Number(s));

export function GeneralPage({ settings, draft, errors, set }: PageProps) {
  const t = useT(settingsStrings);
  const v = <K extends keyof Settings>(k: K) => settingValue(settings, draft, k);
  return (
    <div className="flex flex-col gap-3">
      <Field label={t("checkoutRoot")} hint={t("needsPicker")}>
        <div className="flex gap-2">
          <Input
            readOnly
            value={v("general.checkoutRoot")}
            className="flex-1 font-mono text-[12px]"
          />
          <Button disabled>{t("changeFolder")}</Button>
        </div>
      </Field>
      <Field
        label={t("checkInterval")}
        error={errors["general.checkIntervalMin"] && t("outOfRange")}
      >
        <Input
          type="number"
          min={0}
          max={1440}
          value={Number.isNaN(v("general.checkIntervalMin")) ? "" : v("general.checkIntervalMin")}
          onChange={(e) => set({ "general.checkIntervalMin": num(e.target.value) })}
          className="w-32"
        />
      </Field>
      <Field label={t("language")}>
        <Select
          value={v("general.language")}
          onChange={(e) => set({ "general.language": e.target.value as "vi" | "en" })}
          className="w-48"
        >
          <option value="vi">{t("langVi")}</option>
          <option value="en">{t("langEn")}</option>
        </Select>
      </Field>
    </div>
  );
}

export function AccountsPage() {
  const t = useT(settingsStrings);
  const qc = useQueryClient();
  const toast = useToast();
  const accounts = useQuery({
    queryKey: queryKeys.accounts,
    queryFn: () => unwrap(ipc().commands.accountsList()),
  });
  const signOut = useMutation({
    mutationFn: (realm: string) => unwrap(ipc().commands.accountsSignOut(realm)),
    onSuccess: () => {
      toast.show(t("signedOut"));
      void qc.invalidateQueries({ queryKey: queryKeys.accounts });
    },
    onError: (e) => toast.show(e.message),
  });
  return (
    <div className="flex flex-col gap-3">
      <p className="m-0 text-muted-text">{t("accountsIntro")}</p>
      {accounts.error && (
        <p className="m-0 text-bad-text" role="alert">
          {accounts.error.message}
        </p>
      )}
      {accounts.data?.length === 0 && <p className="m-0">{t("noAccounts")}</p>}
      <ul className="m-0 flex list-none flex-col gap-2 p-0">
        {accounts.data?.map((a) => (
          <li
            key={a.realm}
            className="flex items-center gap-3 rounded-md border border-line px-3 py-2"
          >
            <div className="min-w-0 flex-1">
              <SafeText as="div" className="truncate font-semibold">
                {a.username ?? "-"}
              </SafeText>
              <SafeText as="div" className="truncate text-[12px] text-muted-text">
                {a.realm}
              </SafeText>
              <div className="text-[11px] text-muted-text">
                {a.method === "negotiate" ? t("methodNegotiate") : t("methodBasic")}
              </div>
            </div>
            <Button
              aria-label={`${t("signOut")}: ${a.realm}`}
              disabled={signOut.isPending}
              onClick={() => signOut.mutate(a.realm)}
            >
              {t("signOut")}
            </Button>
          </li>
        ))}
      </ul>
      <section aria-label={t("trustedCerts")}>
        <h3 className="m-0 mb-1 text-[10px] font-medium tracking-[.06em] text-muted-text uppercase">
          {t("trustedCerts")}
        </h3>
        <p className="m-0 text-muted-text">{t("trustedCertsNote")}</p>
      </section>
    </div>
  );
}

export function AppearancePage({ settings, draft, set }: PageProps) {
  const t = useT(settingsStrings);
  return (
    <div className="flex flex-col gap-3">
      <span>{t("theme")}</span>
      <SegmentedControl<Settings["appearance.theme"]>
        label={t("theme")}
        value={settingValue(settings, draft, "appearance.theme")}
        onChange={(v) => set({ "appearance.theme": v })}
        options={[
          { value: "system", label: t("themeSystem") },
          { value: "light", label: t("themeLight") },
          { value: "dark", label: t("themeDark") },
        ]}
      />
    </div>
  );
}

/** Default arguments for a newly chosen diff tool: the two copies, left then right. */
const DEFAULT_DIFF_TOOL_ARGS = ["{left}", "{right}"];

export function DiffMergePage({ settings, draft, set }: PageProps) {
  const t = useT(settingsStrings);
  const toast = useToast();
  const qc = useQueryClient();
  const tool = settings["diff.externalTool"];
  // Rust shows the native file picker and the confirmation; the WebView never sends a path.
  const program = useMutation({
    mutationFn: (choose: boolean) =>
      unwrap(
        choose
          ? ipc().commands.programChoose("diff-tool", tool?.args ?? DEFAULT_DIFF_TOOL_ARGS)
          : ipc().commands.programClear("diff-tool"),
      ),
    onSuccess: (next) => qc.setQueryData(queryKeys.settings, next),
    onError: (e) => toast.show(e.message),
  });
  return (
    <div className="flex flex-col gap-3">
      <span>{t("diffMode")}</span>
      <SegmentedControl<Settings["diff.mode"]>
        label={t("diffMode")}
        value={settingValue(settings, draft, "diff.mode")}
        onChange={(v) => set({ "diff.mode": v })}
        options={[
          { value: "split", label: t("diffSplit") },
          { value: "unified", label: t("diffUnified") },
        ]}
      />
      <Check
        label={t("ignoreWhitespace")}
        checked={settingValue(settings, draft, "diff.ignoreWhitespace")}
        onChange={(v) => set({ "diff.ignoreWhitespace": v })}
      />
      <Field label={t("externalTool")} hint={t("externalToolNote")}>
        <div className="flex gap-2">
          <Input
            readOnly
            value={tool ? `${tool.path} ${tool.args.join(" ")}` : t("noExternalTool")}
            className="flex-1 font-mono text-[12px]"
          />
          <Button disabled={program.isPending} onClick={() => program.mutate(true)}>
            {t("chooseProgram")}
          </Button>
          {tool && (
            <Button disabled={program.isPending} onClick={() => program.mutate(false)}>
              {t("clearProgram")}
            </Button>
          )}
        </div>
      </Field>
    </div>
  );
}

export function IntegrationsPage({ settings, draft, errors, set }: PageProps) {
  const t = useT(settingsStrings);
  const rev = settingValue(settings, draft, "integrations.webRevisionUrl");
  const file = settingValue(settings, draft, "integrations.webFileUrl");
  return (
    <div className="flex flex-col gap-3">
      <Field
        label={t("webRevisionUrl")}
        hint={t("templateHelp")}
        error={errors["integrations.webRevisionUrl"] && t("urlMustBeHttps")}
      >
        <Input
          value={rev}
          spellCheck={false}
          onChange={(e) => set({ "integrations.webRevisionUrl": e.target.value })}
          className="font-mono text-[12px]"
        />
      </Field>
      <Field
        label={t("webFileUrl")}
        error={errors["integrations.webFileUrl"] && t("urlMustBeHttps")}
      >
        <Input
          value={file}
          spellCheck={false}
          onChange={(e) => set({ "integrations.webFileUrl": e.target.value })}
          className="font-mono text-[12px]"
        />
      </Field>
    </div>
  );
}

export function NetworkPage({ settings, draft, errors, set }: PageProps) {
  const t = useT(settingsStrings);
  const proxy = settingValue(settings, draft, "network.proxy");
  const v = <K extends keyof Settings>(k: K) => settingValue(settings, draft, k);
  return (
    <div className="flex flex-col gap-3">
      <Field
        label={t("maxConnections")}
        error={errors["network.maxConnections"] && t("outOfRange")}
      >
        <Input
          type="number"
          min={1}
          max={16}
          value={Number.isNaN(v("network.maxConnections")) ? "" : v("network.maxConnections")}
          onChange={(e) => set({ "network.maxConnections": num(e.target.value) })}
          className="w-32"
        />
      </Field>
      <Field label={t("timeoutSec")} error={errors["network.timeoutSec"] && t("outOfRange")}>
        <Input
          type="number"
          min={1}
          value={Number.isNaN(v("network.timeoutSec")) ? "" : v("network.timeoutSec")}
          onChange={(e) => set({ "network.timeoutSec": num(e.target.value) })}
          className="w-32"
        />
      </Field>
      <Field label={t("proxy")}>
        <Select
          value={proxy.mode}
          onChange={(e) =>
            set({
              "network.proxy":
                e.target.value === "custom" ? { mode: "custom", url: "" } : { mode: "system" },
            })
          }
          className="w-64"
        >
          <option value="system">{t("proxySystem")}</option>
          <option value="custom">{t("proxyCustom")}</option>
        </Select>
      </Field>
      {proxy.mode === "custom" && (
        <Field label={t("proxyCustom")} error={errors["network.proxy"] && t("proxyUrlInvalid")}>
          <Input
            value={proxy.url}
            spellCheck={false}
            onChange={(e) => set({ "network.proxy": { mode: "custom", url: e.target.value } })}
            className="font-mono text-[12px]"
          />
        </Field>
      )}
    </div>
  );
}

export function AdvancedPage({ settings, draft, errors, set }: PageProps) {
  const t = useT(settingsStrings);
  const cache = settingValue(settings, draft, "cache.maxSizeGb");
  return (
    <div className="flex flex-col gap-3">
      <Field label={t("wcFormat")}>
        <Input readOnly value={t("wcFormatValue")} className="w-72" />
      </Field>
      <Check
        label={t("readSvnConfig")}
        checked={settingValue(settings, draft, "wc.readSubversionConfig")}
        onChange={(v) => set({ "wc.readSubversionConfig": v })}
      />
      <Field label={t("cacheMax")} error={errors["cache.maxSizeGb"] && t("outOfRange")}>
        <Input
          type="number"
          min={1}
          value={Number.isNaN(cache) ? "" : cache}
          onChange={(e) => set({ "cache.maxSizeGb": num(e.target.value) })}
          className="w-32"
        />
      </Field>
    </div>
  );
}
