import { expect, test } from "vitest";
import { authStrings } from "../../features/remote/auth/auth-strings";
import { blameStrings } from "../../features/remote/blame/blame-strings";
import { branchStrings } from "../../features/remote/branch/branch-strings";
import { browseStrings } from "../../features/remote/browse/browse-strings";
import { historyStrings } from "../../features/remote/history/history-strings";
import { lockStrings } from "../../features/remote/lock/lock-strings";
import { settingsStrings } from "../../features/remote/settings/settings-strings";
import { commonStrings } from "./common-strings";
import { detectLang, interpolate } from "./i18n";

const catalogs = {
  common: commonStrings,
  auth: authStrings,
  blame: blameStrings,
  branch: branchStrings,
  browse: browseStrings,
  history: historyStrings,
  lock: lockStrings,
  settings: settingsStrings,
};

const placeholders = (s: string) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort();

test("interpolate fills placeholders as plain text and keeps unknown ones", () => {
  expect(interpolate("Attempt {a} of {b}", { a: 2, b: 3 })).toBe("Attempt 2 of 3");
  expect(interpolate("{x} stays", {})).toBe("{x} stays");
  expect(interpolate("<b>{n}</b>", { n: "<i>" })).toBe("<b><i></b>");
});

test("detectLang prefers Vietnamese for vi locales", () => {
  expect(detectLang("vi-VN")).toBe("vi");
  expect(detectLang("en-US")).toBe("en");
  expect(detectLang(undefined)).toBe("en");
});

for (const [name, cat] of Object.entries(catalogs)) {
  test(`${name}: en and vi define the same keys, non-empty, with identical placeholders`, () => {
    const en = cat.en as Record<string, string>;
    const vi = cat.vi as Record<string, string>;
    expect(Object.keys(vi).sort()).toEqual(Object.keys(en).sort());
    for (const key of Object.keys(en)) {
      expect(en[key]?.trim().length, `${name}.${key} en`).toBeGreaterThan(0);
      expect(vi[key]?.trim().length, `${name}.${key} vi`).toBeGreaterThan(0);
      expect(placeholders(vi[key] ?? ""), `${name}.${key} placeholders`).toEqual(
        placeholders(en[key] ?? ""),
      );
    }
  });
}
