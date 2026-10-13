import { act, configure, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, test, vi } from "vitest";
import { AppShell } from "../../../app/app-shell";
import type { WcEntry } from "../../../ipc/bindings";
import { mockCalls, mockEmit, setMockOpDuration } from "../../../ipc/mock/adapter";
import { resetWcMock } from "../../../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import {
  filterEntries,
  isAdminFolderName,
  joinDisplay,
  pathTail,
  sortByLastOpened,
  suggestFolderName,
  urlHost,
  validateFolderName,
} from "./repo-model";

vi.mock("../diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

// Each test boots the whole window; the timeout is only a hang guard (waits use the fake clock).
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
  setMockOpDuration(40);
});

const lastCall = (name: string) => mockCalls.filter((c) => c.name === name).at(-1);
const calls = (name: string) => mockCalls.filter((c) => c.name === name).length;

async function closeCheckout() {
  const dlg = screen.getByRole("dialog", { name: "Checkout repository", hidden: true });
  await user().click(within(dlg).getByRole("button", { name: "Cancel" }));
  await waitFor(() =>
    expect(screen.queryByRole("dialog", { name: "Checkout repository", hidden: true })).toBeNull(),
  );
}

async function openPicker() {
  renderApp(<AppShell />);
  await screen.findByRole("option", { name: /renderer\.ts/ });
  await user().keyboard("{Control>}t{/Control}");
  const filter = await screen.findByRole("combobox", { name: "Filter repositories" });
  await waitFor(() => expect(document.activeElement).toBe(filter));
  const list = screen.getByRole("listbox", { name: "Repositories" });
  await within(list).findByRole("option", { name: /old-prototype/ });
  return { filter, list };
}

describe("repo model", () => {
  const entry = (p: Partial<WcEntry>): WcEntry => ({
    id: "x",
    name: "x",
    rootPath: "/a/b/c",
    repoRootUrl: "https://svn.example.com/r",
    repoUuid: "u",
    url: null,
    addedAt: "",
    lastOpenedAt: null,
    missing: false,
    problem: null,
    ...p,
  });

  test("folder names mirror the backend rules", () => {
    for (const ok of [
      "project",
      "my repo",
      "a.b",
      ".hidden",
      "COM10",
      "con-tools",
      "svn",
      "my_svn",
      ".svnignore",
      "a".repeat(255),
      "é".repeat(127),
    ]) {
      expect(validateFolderName(ok)).toBeNull();
    }
    expect(validateFolderName("")).toBe("empty");
    expect(validateFolderName("a/b")).toBe("separator");
    expect(validateFolderName("a\\b")).toBe("separator");
    expect(validateFolderName("..")).toBe("dots");
    expect(validateFolderName("a:b")).toBe("chars");
    expect(validateFolderName("tab\there")).toBe("chars");
    expect(validateFolderName("x.")).toBe("trailing");
    expect(validateFolderName(" x")).toBe("trailing");
    expect(validateFolderName("nul.txt")).toBe("reserved");
    expect(validateFolderName("Lpt3")).toBe("reserved");
    expect(validateFolderName("a".repeat(256))).toBe("tooLong");
    expect(validateFolderName("é".repeat(128))).toBe("tooLong"); // 256 UTF-8 bytes
    for (const reserved of ["COM0", "lpt0.log", "LPT²", "com³.txt", "COM¹"]) {
      expect(validateFolderName(reserved)).toBe("reserved");
    }
    for (const admin of [".svn", ".SVN", "_svn", "_SvN", "\uFF3Fsvn", "SVN~1", "_ſvn"]) {
      expect(validateFolderName(admin), admin).toBe("admin");
    }
    expect(validateFolderName("_svn.")).toBe("trailing");
    // ignorable code points are dropped before comparing (the name is refused as "format" first)
    expect(isAdminFolderName("_s\u200Dvn")).toBe(true);
    expect(isAdminFolderName("svn")).toBe(false);
    for (const hidden of [
      "a\u202Egnp.exe",
      "a\u2066b",
      "zero\u200Bwidth",
      "bom\uFEFF",
      "soft\u00ADhyphen",
      "tag\u{E0041}",
    ]) {
      expect(validateFolderName(hidden), hidden).toBe("format");
    }
    expect(validateFolderName("c1\u0085")).toBe("chars");
  });

  test("suggested folder names", () => {
    expect(suggestFolderName("https://svn.example.com/repos/game/trunk")).toBe("game");
    expect(suggestFolderName("https://svn.example.com/repos/game/trunk/")).toBe("game");
    expect(suggestFolderName("https://h/repos/game/branches/feature-x")).toBe("feature-x");
    expect(suggestFolderName("svn://h/r/My%20Project")).toBe("My Project");
    expect(suggestFolderName("https://h/r/a:b.")).toBe("a-b");
    expect(suggestFolderName("https://h/r/CON")).toBe("");
    expect(suggestFolderName("https://h/r/_svn")).toBe("");
    expect(suggestFolderName("https://h/r/a%E2%80%AEb")).toBe("ab");
    expect(suggestFolderName("not a url")).toBe("");
  });

  test("display helpers, filter and order", () => {
    expect(pathTail("/Users/dev/work/game")).toBe("…/work/game");
    expect(pathTail("C:\\work\\game")).toBe("…/work/game");
    expect(pathTail("/a/b")).toBe("/a/b");
    expect(urlHost("https://svn.example.com:8443/r")).toBe("svn.example.com:8443");
    expect(urlHost("file:///srv/repo")).toBe("file");
    expect(joinDisplay("/a", "b")).toBe("/a/b");
    expect(joinDisplay("C:\\work", "b")).toBe("C:\\work\\b");
    const list = [
      entry({ id: "1", name: "Game", lastOpenedAt: "2026-01-01T00:00:00Z" }),
      entry({ id: "2", name: "Docs", repoRootUrl: "https://docs.example.com/r" }),
      entry({ id: "3", name: "Tools", lastOpenedAt: "2026-05-01T00:00:00Z" }),
    ];
    expect(filterEntries(list, "DOCS.EX").map((e) => e.id)).toEqual(["2"]);
    expect(filterEntries(list, "  ").length).toBe(3);
    expect(sortByLastOpened(list).map((e) => e.id)).toEqual(["3", "1", "2"]);
  });
});

test("Mod+T opens the picker; typing filters, Enter switches through wc_open", async () => {
  const { filter, list } = await openPicker();
  // Most recently opened first; the missing working copy is flagged.
  const names = within(list)
    .getAllByRole("option")
    .map((o) => o.textContent ?? "");
  expect(names[0]).toContain("game-client");
  expect(names[0]).toContain("svn.example.com");
  expect(names[0]).toContain("…/work/game-client");
  expect(names.find((n) => n.includes("old-prototype"))).toContain("Missing");
  expect(
    within(list)
      .getByRole("option", { name: /game-client/ })
      .getAttribute("aria-current"),
  ).toBe("true");

  await user().type(filter, "design");
  expect(within(list).getAllByRole("option")).toHaveLength(1);
  await user().keyboard("{Enter}");
  await waitFor(() => expect(lastCall("wcOpen")?.args[0]).toBe("wc-docs"));
  expect(
    await screen.findByRole("button", { name: /Current repository\s*design-docs/ }),
  ).toBeTruthy();
  expect(screen.queryByRole("listbox", { name: "Repositories" })).toBeNull();

  // Arrow keys move the active option; no match shows a message.
  await user().keyboard("{Control>}t{/Control}");
  const again = await screen.findByRole("combobox", { name: "Filter repositories" });
  await user().keyboard("{ArrowDown}");
  const active = again.getAttribute("aria-activedescendant");
  expect(active && document.getElementById(active)?.getAttribute("aria-selected")).toBe("true");
  await user().type(again, "zzz");
  expect(screen.getByText("No repository matches “zzz”.")).toBeTruthy();
});

test("a missing working copy offers Locate…, which re-grants through the native dialog", async () => {
  const { filter } = await openPicker();
  await user().type(filter, "old");
  await user().keyboard("{Enter}");
  const menu = await screen.findByRole("menu", { name: "Actions for old-prototype" });
  expect(within(menu).queryByRole("menuitem", { name: "Reveal in file manager" })).toBeNull();
  await user().click(within(menu).getByRole("menuitem", { name: "Locate…" }));
  expect(await screen.findByText("old-prototype was found at its new location")).toBeTruthy();
  expect(String(lastCall("wcLocate")?.args[1])).toMatch(/^grant-/);
  expect(lastCall("wcLocate")?.args[0]).toBe("wc-missing");
  expect(calls("wcOpen")).toBe(0);
});

test("rename and remove from the context menu; remove asks first and keeps the files", async () => {
  const { list } = await openPicker();
  const docs = within(list).getByRole("option", { name: /design-docs/ });
  await act(async () => {
    docs.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
  });
  let menu = await screen.findByRole("menu", { name: "Actions for design-docs" });
  await user().click(within(menu).getByRole("menuitem", { name: "Rename…" }));
  const rename = await screen.findByRole("dialog", { name: "Rename repository", hidden: true });
  const box = within(rename).getByRole("textbox", { name: "Display name" });
  await user().clear(box);
  expect(within(rename).getByRole("alert").textContent).toBe("Enter 1 to 200 characters.");
  await user().type(box, "Docs");
  await user().click(within(rename).getByRole("button", { name: "Rename" }));
  expect(await screen.findByText("Renamed to Docs")).toBeTruthy();
  expect(lastCall("wcRename")?.args).toEqual(["wc-docs", "Docs"]);

  await user().keyboard("{Control>}t{/Control}");
  const list2 = await screen.findByRole("listbox", { name: "Repositories" });
  const renamed = await within(list2).findByRole("option", { name: /Docs/ });
  await act(async () => {
    renamed.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
  });
  menu = await screen.findByRole("menu", { name: "Actions for Docs" });
  await user().click(within(menu).getByRole("menuitem", { name: "Remove from list…" }));
  const confirm = await screen.findByRole("dialog", { name: "Remove from list", hidden: true });
  expect(confirm.textContent).toContain("The folder and its files stay on disk");
  expect(calls("wcRemove")).toBe(0);
  await user().click(within(confirm).getByRole("button", { name: "Remove" }));
  expect(await screen.findByText("Docs was removed from the list")).toBeTruthy();
  expect(lastCall("wcRemove")?.args).toEqual(["wc-docs"]);
});

test("Shift+F10 opens the actions of the active row: reveal and copy URL", async () => {
  await openPicker();
  await user().keyboard("{Shift>}{F10}{/Shift}");
  let menu = await screen.findByRole("menu", { name: "Actions for game-client" });
  await user().click(within(menu).getByRole("menuitem", { name: "Reveal in file manager" }));
  await waitFor(() => expect(lastCall("revealInOs")?.args).toEqual(["wc-main", ""]));

  await user().keyboard("{Shift>}{F10}{/Shift}");
  menu = await screen.findByRole("menu", { name: "Actions for game-client" });
  await user().click(within(menu).getByRole("menuitem", { name: "Copy URL" }));
  expect(await screen.findByText("URL copied")).toBeTruthy();
  expect(await navigator.clipboard.readText()).toBe("https://svn.example.com/repos/game/trunk");
});

test("footer and File menu events open checkout and add existing; a menu/key echo is ignored in either order", async () => {
  await openPicker();
  await user().click(screen.getByRole("button", { name: "Checkout repository…" }));
  expect(
    await screen.findByRole("dialog", { name: "Checkout repository", hidden: true }),
  ).toBeTruthy();
  await closeCheckout();

  await act(async () => mockEmit("menu://switch-repository"));
  expect(await screen.findByRole("combobox", { name: "Filter repositories" })).toBeTruthy();
  await user().keyboard("{Escape}");

  await act(async () => mockEmit("menu://checkout"));
  expect(
    await screen.findByRole("dialog", { name: "Checkout repository", hidden: true }),
  ).toBeTruthy();
  await closeCheckout();

  // Mod+O and the native accelerator's echo: the folder dialog is shown once.
  // Wall-clock time is frozen so a slow machine cannot stretch the key-to-echo gap.
  const now = vi.spyOn(Date, "now").mockReturnValue(1_000_000);
  await user().keyboard("{Control>}o{/Control}");
  await act(async () => mockEmit("menu://add-working-copy"));
  await waitFor(() => expect(calls("pathPick")).toBe(1));
  expect(await screen.findByText(/Added working copy/)).toBeTruthy();
  expect(calls("pathPick")).toBe(1);

  // The other order (Windows/Linux can deliver the menu event first): still shown once.
  now.mockReturnValue(2_000_000);
  await act(async () => mockEmit("menu://add-working-copy"));
  now.mockReturnValue(2_000_050);
  await user().keyboard("{Control>}o{/Control}");
  await waitFor(() => expect(calls("pathPick")).toBe(2));
  expect(calls("pathPick")).toBe(2);

  // A menu event more than 400 ms after the key is a new request, not an echo: it runs.
  now.mockReturnValue(3_000_000);
  await user().keyboard("{Control>}o{/Control}");
  await waitFor(() => expect(calls("pathPick")).toBe(3));
  now.mockReturnValue(3_000_401);
  await act(async () => mockEmit("menu://add-working-copy"));
  await waitFor(() => expect(calls("pathPick")).toBe(4));
  // ...and so is a key press more than 400 ms after a menu event.
  now.mockReturnValue(4_000_000);
  await act(async () => mockEmit("menu://add-working-copy"));
  await waitFor(() => expect(calls("pathPick")).toBe(5));
  now.mockReturnValue(4_000_401);
  await user().keyboard("{Control>}o{/Control}");
  await waitFor(() => expect(calls("pathPick")).toBe(6));
  now.mockRestore();
});
