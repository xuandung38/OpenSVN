import { render, screen } from "@testing-library/react";
import { expect, test } from "vitest";
import { mockSettings } from "../ipc/mock/data";
import {
  applyUiPrefs,
  DEFAULT_UI_PREFS,
  rowHeightFor,
  UiPrefsProvider,
  uiPrefsOf,
  useRowHeight,
} from "./ui-prefs";

test("row heights grow with the font size and shrink when compact, never below one text line", () => {
  expect(rowHeightFor(28, DEFAULT_UI_PREFS)).toBe(28);
  expect(rowHeightFor(28, { ...DEFAULT_UI_PREFS, fontSize: 20 })).toBe(43);
  expect(rowHeightFor(28, { ...DEFAULT_UI_PREFS, density: "compact" })).toBe(24);
  // 20 px at 13 px compact would be 17; a line of 13 px text needs ceil(13 * 1.4) + 2 = 21.
  expect(rowHeightFor(20, { ...DEFAULT_UI_PREFS, density: "compact" })).toBe(21);
});

test("preferences are written to the document as CSS variables and data-density", () => {
  const root = document.createElement("div");
  applyUiPrefs({ fontSize: 16, density: "compact", tabWidth: 8 }, root);
  expect(root.style.getPropertyValue("--ui-font-size")).toBe("16px");
  expect(root.style.getPropertyValue("--tab-width")).toBe("8");
  expect(root.getAttribute("data-density")).toBe("compact");
});

test("the provider applies the saved settings and scales list rows", () => {
  function Row() {
    return <span>{useRowHeight(28)}</span>;
  }
  const settings = {
    ...mockSettings,
    "appearance.fontSize": 15,
    "appearance.density": "compact" as const,
    "diff.tabWidth": 2,
  };
  expect(uiPrefsOf(settings)).toEqual({ fontSize: 15, density: "compact", tabWidth: 2 });
  render(
    <UiPrefsProvider settings={settings}>
      <Row />
    </UiPrefsProvider>,
  );
  expect(screen.getByText(String(rowHeightFor(28, uiPrefsOf(settings))))).toBeTruthy();
  const root = document.documentElement;
  expect(root.style.getPropertyValue("--ui-font-size")).toBe("15px");
  expect(root.style.getPropertyValue("--tab-width")).toBe("2");
  expect(root.dataset.density).toBe("compact");
});

test("outside the provider (and before settings load) the defaults apply", () => {
  expect(uiPrefsOf(undefined)).toEqual(DEFAULT_UI_PREFS);
});
