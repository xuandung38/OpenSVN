import type { ProxySetting, Settings, SettingsPatch_Deserialize } from "../../../ipc/bindings";

export type Draft = SettingsPatch_Deserialize;

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
    return new URL(sample).protocol === "https:";
  } catch {
    return false;
  }
}

export function isProxyUrl(v: string): boolean {
  try {
    const p = new URL(v).protocol;
    return p === "http:" || p === "https:";
  } catch {
    return false;
  }
}

export function inRange(n: number, min: number, max: number): boolean {
  return Number.isFinite(n) && Number.isInteger(n) && n >= min && n <= max;
}

/** Field-level validation of the draft against the stored settings; keys with errors map to an error id. */
export function validateDraft(s: Settings, d: Draft): Record<string, "https" | "range" | "proxy"> {
  const errors: Record<string, "https" | "range" | "proxy"> = {};
  const v = <K extends keyof Settings>(k: K) => settingValue(s, d, k);
  if (!isHttpsTemplate(v("integrations.webRevisionUrl")))
    errors["integrations.webRevisionUrl"] = "https";
  if (!isHttpsTemplate(v("integrations.webFileUrl"))) errors["integrations.webFileUrl"] = "https";
  if (!inRange(v("network.maxConnections"), 1, 16)) errors["network.maxConnections"] = "range";
  if (!inRange(v("network.timeoutSec"), 1, 3600)) errors["network.timeoutSec"] = "range";
  if (!inRange(v("general.checkIntervalMin"), 0, 1440))
    errors["general.checkIntervalMin"] = "range";
  if (!inRange(v("cache.maxSizeGb"), 1, 1024)) errors["cache.maxSizeGb"] = "range";
  const proxy: ProxySetting = v("network.proxy");
  if (proxy.mode === "custom" && !isProxyUrl(proxy.url)) errors["network.proxy"] = "proxy";
  return errors;
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
