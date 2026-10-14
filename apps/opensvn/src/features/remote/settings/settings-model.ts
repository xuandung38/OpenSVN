import {
  type AppError,
  type ProxySetting,
  SETTING_RANGES,
  type Settings,
  type SettingsPatch_Deserialize,
} from "../../../ipc/bindings";
import { IpcError } from "../../../ipc/client";

export type Draft = SettingsPatch_Deserialize;

/** A numeric setting with a range. The ranges come from the Rust validator (generated bindings). */
export type RangedKey = keyof typeof SETTING_RANGES & keyof Settings;
export const RANGED_KEYS = Object.keys(SETTING_RANGES) as RangedKey[];

/** Inclusive bounds of `key`, exactly as the backend validates them. */
export function rangeOf(key: RangedKey): { min: number; max: number } {
  return SETTING_RANGES[key];
}

/**
 * Why a field is invalid: checked here before Save, or `rejected` when the backend refused the
 * value on Save (its message is never shown as is; the field shows a localized text).
 */
export type FieldError = "https" | "range" | "proxy" | "rejected";

/** Effective value of a setting: the unsaved draft wins over the stored value. */
export function settingValue<K extends keyof Settings>(s: Settings, d: Draft, key: K): Settings[K] {
  return (key in d ? (d as Settings)[key] : s[key]) as Settings[K];
}

/** `""` is allowed (feature off); anything else must be an absolute https URL. */
export function isHttpsTemplate(v: string): boolean {
  if (v.trim() === "") return true;
  // Placeholders are not valid URL characters, so test with sample values substituted.
  const sample = v
    .replaceAll("{server}", "host")
    .replaceAll("{repo}", "repo")
    .replaceAll("{rev}", "1")
    .replaceAll("{revSpec}", "head")
    .replaceAll("{path}", "a/b");
  try {
    const u = new URL(sample);
    // Like the backend: no credentials in a link that is opened in the browser.
    return u.protocol === "https:" && u.username === "" && u.password === "";
  } catch {
    return false;
  }
}

/** http(s) proxy URL without credentials (they belong in the credential store, as the backend says). */
export function isProxyUrl(v: string): boolean {
  try {
    const u = new URL(v);
    return (
      (u.protocol === "http:" || u.protocol === "https:") &&
      u.hostname !== "" &&
      u.username === "" &&
      u.password === ""
    );
  } catch {
    return false;
  }
}

export function inRange(n: number, min: number, max: number): boolean {
  return Number.isFinite(n) && Number.isInteger(n) && n >= min && n <= max;
}

/** Field-level validation of the draft against the stored settings; keys with errors map to an error id. */
export function validateDraft(s: Settings, d: Draft): Record<string, FieldError> {
  const errors: Record<string, FieldError> = {};
  const v = <K extends keyof Settings>(k: K) => settingValue(s, d, k);
  if (!isHttpsTemplate(v("integrations.webRevisionUrl")))
    errors["integrations.webRevisionUrl"] = "https";
  if (!isHttpsTemplate(v("integrations.webFileUrl"))) errors["integrations.webFileUrl"] = "https";
  for (const key of RANGED_KEYS) {
    const { min, max } = rangeOf(key);
    if (!inRange(v(key), min, max)) errors[key] = "range";
  }
  const proxy: ProxySetting = v("network.proxy");
  if (proxy.mode === "custom" && !isProxyUrl(proxy.url)) errors["network.proxy"] = "proxy";
  return errors;
}

/**
 * The setting a failed Save points at: the backend refuses a value with `invalid-argument` and the
 * dotted key in `detail`. `null` for any other failure (or a key the sheet does not show).
 */
export function rejectedKey(err: unknown, s: Settings | undefined): keyof Settings | null {
  const e: AppError | null = err instanceof IpcError ? err.appError : null;
  if (e?.code !== "invalid-argument" || !e.detail || !s || !(e.detail in s)) return null;
  return e.detail as keyof Settings;
}

/**
 * Drops draft keys whose value is back to the stored one (deep compare, e.g. `network.proxy`), so
 * Save only enables and sends keys that really changed.
 */
export function withoutUnchanged(s: Settings, d: Draft): Draft {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(d)) {
    if (JSON.stringify(v) !== JSON.stringify((s as unknown as Record<string, unknown>)[k]))
      out[k] = v;
  }
  return out as Draft;
}
