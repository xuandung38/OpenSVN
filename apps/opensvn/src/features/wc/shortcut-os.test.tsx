import { act, configure, screen, within } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";
import { AppShell } from "../../app/app-shell";
import { mockCalls, mockEmit } from "../../ipc/mock/adapter";
import { resetWcMock } from "../../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../test-utils";
import { isMac, setShortcutOs, shortcutLabel, shortcutOsFromQuery } from "./shortcuts";

vi.mock("./diff/diff-merge-editor", () => ({ DiffMergeEditor: () => <div /> }));

vi.setConfig({ testTimeout: 120_000 });
const { user } = installFakeClock();
configure({ asyncUtilTimeout: 8_000 });

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
}, 20_000);

beforeEach(() => {
  resetWcMock();
  mockCalls.length = 0;
});

afterEach(() => setShortcutOs(null));

test("?os=mac|win is read from a dev URL; anything else keeps the real platform", () => {
  expect(shortcutOsFromQuery("?os=mac")).toBe("mac");
  expect(shortcutOsFromQuery("?view=1&os=WIN")).toBe("win");
  expect(shortcutOsFromQuery("?os=linux")).toBeNull();
  expect(shortcutOsFromQuery("")).toBeNull();
  setShortcutOs("mac");
  expect(isMac()).toBe(true);
  expect(shortcutLabel("update")).toBe("⌘⇧U");
  setShortcutOs("win");
  expect(isMac()).toBe(false);
  expect(shortcutLabel("update")).toBe("Ctrl+Shift+U");
});

async function shortcutsShown(): Promise<HTMLElement> {
  await act(async () => mockEmit("menu://help/shortcuts"));
  return screen.findByRole("dialog", { name: "Keyboard shortcuts", hidden: true });
}

test.each([
  ["mac", "⌘⇧U", "Meta+T", "{Meta>}t{/Meta}", "{Control>}t{/Control}"],
  ["win", "Ctrl+Shift+U", "Control+T", "{Control>}t{/Control}", "{Meta>}t{/Meta}"],
] as const)(
  "with the %s override, labels, toolbar hint and key matching switch",
  async (os, update, hint, key, otherKey) => {
    setShortcutOs(os);
    renderApp(<AppShell />);
    await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });

    // The working copy picker trigger advertises the platform's accelerator.
    const trigger = document.querySelector<HTMLElement>("[aria-keyshortcuts]");
    expect(trigger?.getAttribute("aria-keyshortcuts")).toBe(hint);

    const help = await shortcutsShown();
    const row = within(help).getByText("Update");
    expect(row.nextElementSibling?.textContent).toBe(update);
    await user().click(within(help).getByRole("button", { name: "Close", hidden: true }));

    // The other platform's modifier does nothing; this one opens the picker.
    await user().keyboard(otherKey);
    expect(screen.queryByRole("combobox", { name: "Filter repositories" })).toBeNull();
    await user().keyboard(key);
    expect(await screen.findByRole("combobox", { name: "Filter repositories" })).toBeTruthy();
  },
);
