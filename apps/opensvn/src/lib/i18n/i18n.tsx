import { createContext, type ReactNode, useCallback, useContext } from "react";

export type Lang = "en" | "vi";

/** A feature's strings: both languages must define exactly the same keys (compile-time check). */
export interface Strings<K extends string> {
  en: Record<K, string>;
  vi: Record<K, string>;
}

export function defineStrings<K extends string>(s: Strings<K>): Strings<K> {
  return s;
}

export type Vars = Record<string, string | number>;

/** `{name}` placeholders. Values are inserted as plain text; React escapes them at render. */
export function interpolate(template: string, vars?: Vars): string {
  if (!vars) return template;
  return template.replace(/\{(\w+)\}/g, (m, k: string) => (k in vars ? String(vars[k]) : m));
}

/** Default UI language: Vietnamese for vi-* locales (product default), English otherwise. */
export function detectLang(locale: string | undefined): Lang {
  return locale?.toLowerCase().startsWith("vi") ? "vi" : "en";
}

const I18nContext = createContext<Lang>("en");

export function I18nProvider({ lang, children }: { lang: Lang; children: ReactNode }) {
  return <I18nContext.Provider value={lang}>{children}</I18nContext.Provider>;
}

export function useLang(): Lang {
  return useContext(I18nContext);
}

/** Typed translator for one feature's catalog. */
export function useT<K extends string>(strings: Strings<K>) {
  const lang = useLang();
  return useCallback(
    (key: K, vars?: Vars): string => interpolate(strings[lang][key], vars),
    [strings, lang],
  );
}
