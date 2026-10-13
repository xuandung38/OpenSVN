import { expect, test } from "vitest";
import { formatBytes, relativeTime } from "./format";
import { statusCode } from "./status";
import { applyTheme } from "./theme";

test("relativeTime buckets", () => {
  const now = Date.parse("2026-10-13T12:00:00Z");
  expect(relativeTime("2026-10-13T11:59:30Z", now)).toBe("just now");
  expect(relativeTime("2026-10-13T11:00:00Z", now)).toBe("1h ago");
  expect(relativeTime("2026-10-11T12:00:00Z", now)).toBe("2d ago");
  expect(relativeTime("garbage", now)).toBe("");
});

test("formatBytes", () => {
  expect(formatBytes(512)).toBe("512 B");
  expect(formatBytes(1536)).toBe("1.5 KB");
});

test("statusCode maps node statuses and hides normal", () => {
  expect(statusCode("conflicted")).toBe("C");
  expect(statusCode("unversioned")).toBe("?");
  expect(statusCode("normal")).toBeNull();
});

test("applyTheme sets and clears data-theme", () => {
  applyTheme("dark");
  expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
  applyTheme("system");
  expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
});
