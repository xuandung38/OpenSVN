import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { Button } from "../../../components/ui/button";
import { Input, Select, Textarea } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { SegmentedControl } from "../../../components/ui/segmented-control";
import { useToast } from "../../../components/ui/toast";
import type { ProgramKind, Settings, TrustedCert } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { ExtDiffTools } from "./ext-diff-tools";
import {
  type Draft,
  type FieldError,
  MAX_GLOBAL_IGNORES,
  type RangedKey,
  rangeOf,
  settingValue,
} from "./settings-model";
import { settingsStrings } from "./settings-strings";

export interface PageProps {
  settings: Settings;
  draft: Draft;
  errors: Record<string, FieldError>;
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

/** Inline text for `key`: the local rule's message, or `rejected` when only the backend refused it. */
function fieldError(
  errors: Record<string, FieldError>,
  key: string,
  local: string,
  rejected: string,
): string | undefined {
  const e = errors[key];
  return e === "rejected" ? rejected : e ? local : undefined;
}

/**
 * A numeric setting: its label, `<input min/max>` and inline error all use the backend's range
 * (`SETTING_RANGES`), so the page can never accept a value that Save would refuse.
 */
function NumberField({
  name,
  label,
  settings,
  draft,
  errors,
  set,
}: PageProps & {
  name: RangedKey;
  label:
    | "checkInterval"
    | "maxConnections"
    | "timeoutSec"
    | "reconnectWait"
    | "cacheMax"
    | "fontSize"
    | "tabWidth";
}) {
  const t = useT(settingsStrings);
  const range = rangeOf(name);
  const value = settingValue(settings, draft, name);
  const error = fieldError(errors, name, t("outOfRange", range), t("rejected"));
  return (
    <Field label={t(label, range)} error={error}>
      <Input
        type="number"
        min={range.min}
        max={range.max}
        step={1}
        aria-invalid={error ? true : undefined}
        value={Number.isNaN(value) ? "" : value}
        onChange={(e) => set({ [name]: num(e.target.value) } as Draft)}
        className="w-32"
      />
    </Field>
  );
}

/**
 * Two-step action without a nested modal: the first click shows the question with Confirm and
 * Cancel next to it (focus moves to Confirm), the second runs the action.
 */
function ConfirmAction({
  label,
  ariaLabel,
  question,
  disabled,
  onConfirm,
}: {
  label: string;
  ariaLabel?: string;
  question: string;
  disabled?: boolean;
  onConfirm: () => void;
}) {
  const t = useT(settingsStrings);
  const c = useT(commonStrings);
  const [asking, setAsking] = useState(false);
  const confirmRef = useRef<HTMLButtonElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  // Focus follows the step: Confirm when asking, back on the button afterwards (if it is still
  // there: a removed row takes its button with it).
  const answered = useRef(false);
  useEffect(() => {
    if (asking) confirmRef.current?.focus();
    else if (answered.current) triggerRef.current?.focus();
  }, [asking]);
  const answer = (confirmed: boolean) => {
    answered.current = true;
    setAsking(false);
    if (confirmed) onConfirm();
  };
  if (!asking) {
    return (
      <Button
        ref={triggerRef}
        aria-label={ariaLabel}
        disabled={disabled}
        onClick={() => setAsking(true)}
      >
        {label}
      </Button>
    );
  }
  return (
    // biome-ignore lint/a11y/useSemanticElements: a fieldset would restyle the row; role="group" names it
    <div role="group" aria-label={question} className="flex flex-wrap items-center gap-2">
      <span className="text-[12px]">{question}</span>
      <Button ref={confirmRef} variant="primary" disabled={disabled} onClick={() => answer(true)}>
        {t("confirm")}
      </Button>
      <Button onClick={() => answer(false)}>{c("cancel")}</Button>
    </div>
  );
}

/**
 * An external program setting. Rust shows the native file picker and the confirmation; the WebView
 * never sends a path, only the arguments to keep.
 */
function ProgramField({
  kind,
  settings,
  label,
  hint,
  defaultArgs,
}: {
  kind: ProgramKind;
  settings: Settings;
  label: string;
  hint: string;
  defaultArgs: string[];
}) {
  const t = useT(settingsStrings);
  const toast = useToast();
  const qc = useQueryClient();
  const tool = settings[PROGRAM_SETTING[kind]];
  const program = useMutation({
    mutationFn: (choose: boolean) =>
      unwrap(
        choose
          ? ipc().commands.programChoose(kind, tool?.args ?? defaultArgs)
          : ipc().commands.programClear(kind),
      ),
    onSuccess: (next) => qc.setQueryData(queryKeys.settings, next),
    onError: (e) => toast.show(e.message),
  });
  return (
    <Field label={label} hint={hint}>
      <div className="flex gap-2">
        <Input
          readOnly
          value={tool ? `${tool.path} ${tool.args.join(" ")}`.trim() : t("noExternalTool")}
          className="flex-1 font-mono text-[12px]"
        />
        <Button
          aria-label={`${t("chooseProgram")} ${label}`}
          disabled={program.isPending}
          onClick={() => program.mutate(true)}
        >
          {t("chooseProgram")}
        </Button>
        {tool && (
          <Button
            aria-label={`${t("clearProgram")} ${label}`}
            disabled={program.isPending}
            onClick={() => program.mutate(false)}
          >
            {t("clearProgram")}
          </Button>
        )}
      </div>
    </Field>
  );
}

const PROGRAM_SETTING = {
  "diff-tool": "diff.externalTool",
  "merge-tool": "diff.mergeTool",
  editor: "tools.editor",
  terminal: "tools.terminal",
} as const satisfies Record<ProgramKind, keyof Settings>;

export function GeneralPage({ settings, draft, errors, set }: PageProps) {
  const t = useT(settingsStrings);
  const qc = useQueryClient();
  const toast = useToast();
  const v = <K extends keyof Settings>(k: K) => settingValue(settings, draft, k);
  // Rust shows the native folder dialog and stores the choice; the WebView never sends a path.
  const chooseRoot = useMutation({
    mutationFn: () => unwrap(ipc().commands.checkoutRootChoose()),
    onSuccess: (next) => qc.setQueryData(queryKeys.settings, next),
    onError: (e) => toast.show(e.message),
  });
  return (
    <div className="flex flex-col gap-3">
      <Field label={t("checkoutRoot")} hint={t("needsPicker")}>
        <div className="flex gap-2">
          <Input
            readOnly
            value={settings["general.checkoutRoot"]}
            className="flex-1 font-mono text-[12px]"
          />
          <Button disabled={chooseRoot.isPending} onClick={() => chooseRoot.mutate()}>
            {t("changeFolder")}
          </Button>
        </div>
      </Field>
      <NumberField
        name="general.checkIntervalMin"
        label="checkInterval"
        {...{ settings, draft, errors, set }}
      />
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
      <Check
        label={t("openLastWc")}
        checked={v("general.openLastWc")}
        onChange={(on) => set({ "general.openLastWc": on })}
      />
      <Field label={t("logLevel")} hint={t("logLevelHint")}>
        <Select
          value={v("general.logLevel")}
          onChange={(e) =>
            set({ "general.logLevel": e.target.value as Settings["general.logLevel"] })
          }
          className="w-72"
        >
          <option value="error">{t("logError")}</option>
          <option value="warn">{t("logWarn")}</option>
          <option value="info">{t("logInfo")}</option>
          <option value="debug">{t("logDebug")}</option>
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
      <TrustedCerts />
    </div>
  );
}

/** `ab12…` as `AB:12:…`, the form certificate viewers show. */
function fingerprintText(hex: string): string {
  return (hex.toUpperCase().match(/.{1,2}/g) ?? []).join(":");
}

function TrustedCerts() {
  const t = useT(settingsStrings);
  const qc = useQueryClient();
  const toast = useToast();
  const certs = useQuery({
    queryKey: queryKeys.trustedCerts,
    queryFn: () => unwrap(ipc().commands.certsList()),
  });
  const remove = useMutation({
    mutationFn: (c: TrustedCert) =>
      unwrap(ipc().commands.certsRemove({ host: c.host, port: c.port, sha256: c.sha256 })),
    onSuccess: () => toast.show(t("certRemoved")),
    onError: (e) => toast.show(e.message),
    onSettled: () => void qc.invalidateQueries({ queryKey: queryKeys.trustedCerts }),
  });
  return (
    <section aria-labelledby="trusted-certs-title" className="flex flex-col gap-2">
      <h3
        id="trusted-certs-title"
        className="m-0 mb-1 text-[10px] font-medium tracking-[.06em] text-muted-text uppercase"
      >
        {t("trustedCerts")}
      </h3>
      <p className="m-0 text-muted-text">{t("trustedCertsNote")}</p>
      {certs.error && (
        <p className="m-0 text-bad-text" role="alert">
          {certs.error.message}
        </p>
      )}
      {certs.data?.length === 0 && <p className="m-0">{t("noCerts")}</p>}
      <ul className="m-0 flex list-none flex-col gap-2 p-0">
        {certs.data?.map((c) => {
          const origin = `${c.host}:${c.port}`;
          return (
            <li
              key={`${origin}/${c.sha256}`}
              className="flex items-center gap-3 rounded-md border border-line px-3 py-2"
            >
              <div className="min-w-0 flex-1">
                <SafeText as="div" className="truncate font-semibold">
                  {origin}
                </SafeText>
                {c.subject && (
                  <SafeText as="div" className="truncate text-[12px] text-muted-text">
                    {c.subject}
                  </SafeText>
                )}
                <div className="text-[11px] text-muted-text">
                  {c.expiresAt
                    ? t("certExpires", { date: c.expiresAt.slice(0, 10) })
                    : t("certNoExpiry")}
                </div>
                <div className="font-mono text-[11px] break-all text-muted-text">
                  {t("certFingerprint")} {fingerprintText(c.sha256)}
                </div>
              </div>
              <ConfirmAction
                label={t("removeCert")}
                ariaLabel={`${t("removeCert")}: ${origin}`}
                question={t("removeCertConfirm", { host: origin })}
                disabled={remove.isPending}
                onConfirm={() => remove.mutate(c)}
              />
            </li>
          );
        })}
      </ul>
    </section>
  );
}

export function AppearancePage({ settings, draft, errors, set }: PageProps) {
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
      <NumberField
        name="appearance.fontSize"
        label="fontSize"
        {...{ settings, draft, errors, set }}
      />
      <span>{t("density")}</span>
      <SegmentedControl<Settings["appearance.density"]>
        label={t("density")}
        value={settingValue(settings, draft, "appearance.density")}
        onChange={(v) => set({ "appearance.density": v })}
        options={[
          { value: "comfortable", label: t("densityComfortable") },
          { value: "compact", label: t("densityCompact") },
        ]}
      />
    </div>
  );
}

/** Default arguments for a newly chosen diff tool: the two copies, left then right. */
const DEFAULT_DIFF_TOOL_ARGS = ["{left}", "{right}"];

export function DiffMergePage({ settings, draft, errors, set }: PageProps) {
  const t = useT(settingsStrings);
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
      <NumberField name="diff.tabWidth" label="tabWidth" {...{ settings, draft, errors, set }} />
      <ProgramField
        kind="diff-tool"
        settings={settings}
        label={t("externalTool")}
        hint={t("externalToolNote")}
        defaultArgs={DEFAULT_DIFF_TOOL_ARGS}
      />
      <ExtDiffTools settings={settings} />
      <ProgramField
        kind="merge-tool"
        settings={settings}
        label={t("mergeTool")}
        hint={t("programNote")}
        defaultArgs={[]}
      />
      <ProgramField
        kind="editor"
        settings={settings}
        label={t("editor")}
        hint={t("editorNote")}
        defaultArgs={[]}
      />
      <ProgramField
        kind="terminal"
        settings={settings}
        label={t("terminal")}
        hint={t("terminalNote")}
        defaultArgs={[]}
      />
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
        error={fieldError(
          errors,
          "integrations.webRevisionUrl",
          t("urlMustBeHttps"),
          t("rejected"),
        )}
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
        error={fieldError(errors, "integrations.webFileUrl", t("urlMustBeHttps"), t("rejected"))}
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
      <NumberField
        name="network.maxConnections"
        label="maxConnections"
        {...{ settings, draft, errors, set }}
      />
      <NumberField
        name="network.timeoutSec"
        label="timeoutSec"
        {...{ settings, draft, errors, set }}
      />
      <NumberField
        name="network.reconnectWaitMin"
        label="reconnectWait"
        {...{ settings, draft, errors, set }}
      />
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
        <Field
          label={t("proxyCustom")}
          error={fieldError(errors, "network.proxy", t("proxyUrlInvalid"), t("rejected"))}
        >
          <Input
            value={proxy.url}
            spellCheck={false}
            onChange={(e) => set({ "network.proxy": { mode: "custom", url: e.target.value } })}
            className="font-mono text-[12px]"
          />
        </Field>
      )}
      <Check
        label={t("negotiate")}
        checked={v("network.negotiate")}
        onChange={(on) => set({ "network.negotiate": on })}
      />
    </div>
  );
}

export function AdvancedPage({ settings, draft, errors, set }: PageProps) {
  const t = useT(settingsStrings);
  const toast = useToast();
  const qc = useQueryClient();
  const clear = useMutation({
    mutationFn: () => unwrap(ipc().commands.cacheClear()),
    onSuccess: () => {
      toast.show(t("cacheCleared"));
      // Status lists rescan instead of showing what was just removed.
      void qc.invalidateQueries({ queryKey: queryKeys.statusAll });
    },
    onError: (e) => toast.show(e.message),
  });
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
      <Field
        label={t("globalIgnores")}
        hint={t("globalIgnoresHint")}
        error={fieldError(
          errors,
          "wc.globalIgnores",
          t("globalIgnoresInvalid", { max: MAX_GLOBAL_IGNORES }),
          t("rejected"),
        )}
      >
        <Textarea
          rows={3}
          spellCheck={false}
          aria-invalid={errors["wc.globalIgnores"] ? true : undefined}
          value={settingValue(settings, draft, "wc.globalIgnores")}
          onChange={(e) => set({ "wc.globalIgnores": e.target.value })}
          className="font-mono text-[12px]"
        />
      </Field>
      <NumberField name="cache.maxSizeGb" label="cacheMax" {...{ settings, draft, errors, set }} />
      <div className="flex flex-col gap-1">
        <div>
          <ConfirmAction
            label={t("clearCache")}
            question={t("clearCacheConfirm")}
            disabled={clear.isPending}
            onConfirm={() => clear.mutate()}
          />
        </div>
        <span className="text-[11px] text-muted-text">{t("clearCacheHint")}</span>
      </div>
    </div>
  );
}
