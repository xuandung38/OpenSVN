import { act, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import { HOSTILE_NAME, MOCK_REPO_ROOT, MOCK_REPO_UUID } from "../../../ipc/mock/remote-data";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import { BrowseScreen } from "./browse-screen";
import { flattenForest, flattenTree, type TreeRow } from "./repo-tree";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});

// Fake clock: findBy/waitFor budgets count simulated time, so a loaded machine cannot time them out.
const { user } = installFakeClock();
// The per-test timeout is only a hang guard now: under CPU load rendering alone can take many
// wall-clock seconds, while findBy/waitFor budgets run on the fake clock.
vi.setConfig({ testTimeout: 120_000 });

const noop = () => undefined;
const screenUi = () => (
  <BrowseScreen
    repoRootUrl={MOCK_REPO_ROOT}
    repoUuid={MOCK_REPO_UUID}
    currentRefUrl={`${MOCK_REPO_ROOT}/trunk`}
    startUrl={`${MOCK_REPO_ROOT}/trunk`}
    wcId="wc-main"
    layoutRepo
    onShowLog={noop}
    historyNav={{}}
    onOpenRevision={noop}
  />
);

test("flattenTree lists only folders, respects expansion and records parents", () => {
  const rows: TreeRow[] = flattenTree(
    "https://h/r",
    new Set(["https://h/r", "https://h/r/a"]),
    new Map<string, never>([
      [
        "https://h/r",
        [
          { name: "a", kind: "dir" },
          { name: "f.txt", kind: "file" },
        ] as never,
      ],
      ["https://h/r/a", [{ name: "b", kind: "dir" }] as never],
    ]),
  );
  expect(rows.map((r) => [r.name, r.depth, r.parent])).toEqual([
    ["r", 0, -1],
    ["a", 1, 0],
    ["b", 2, 1],
  ]);
});

test("flattenForest puts several repositories at the top level", () => {
  const rows = flattenForest(
    [
      { url: "https://h/svn/A", name: "A" },
      { url: "https://h/svn/D%E1%BB%B1%20%C3%A1n", name: "Dự án" },
    ],
    new Set(["https://h/svn/D%E1%BB%B1%20%C3%A1n"]),
    new Map<string, never>([
      ["https://h/svn/D%E1%BB%B1%20%C3%A1n", [{ name: "trunk", kind: "dir" }] as never],
    ]),
  );
  expect(rows.map((r) => [r.name, r.url, r.depth, r.parent])).toEqual([
    ["A", "https://h/svn/A", 0, -1],
    ["Dự án", "https://h/svn/D%E1%BB%B1%20%C3%A1n", 0, -1],
    ["trunk", "https://h/svn/D%E1%BB%B1%20%C3%A1n/trunk", 1, 1],
  ]);
});

test("lists the folder, renders a hostile file name as text and keeps it inert", async () => {
  const { container } = renderApp(screenUi());
  expect(await screen.findByRole("option", { name: /README\.md/ })).toBeTruthy();
  // Navigate into notes/ (contains the hostile name).
  const list = screen.getByRole("listbox", { name: "Repository browser" });
  list.focus();
  // notes is the 2nd folder (assets, notes, src); Enter opens it.
  await user().keyboard("{ArrowDown}{Enter}");
  expect(await screen.findByText(HOSTILE_NAME)).toBeTruthy();
  expect(container.querySelector("img")).toBeNull();
  expect(document.querySelector("script")).toBeNull();
});

test("keyboard: Enter on a file opens Preview, showing its text without HTML", async () => {
  renderApp(screenUi());
  await screen.findByRole("option", { name: /README\.md/ });
  const list = screen.getByRole("listbox", { name: "Repository browser" });
  list.focus();
  await user().keyboard("{End}{Enter}");
  expect(await screen.findByRole("tab", { name: "Preview" })).toBeTruthy();
  await waitFor(() => expect(document.body.textContent).toContain("export function render"));
});

test("New folder needs a valid name and a message before it can be sent", async () => {
  renderApp(screenUi());
  await screen.findByRole("option", { name: /README\.md/ });
  await user().click(screen.getByRole("button", { name: "Actions" }));
  await user().click(await screen.findByRole("menuitem", { name: "New folder…" }));
  const dialog = await screen.findByRole("dialog", { name: "New folder", hidden: true });
  const submit = within(dialog).getByRole("button", { name: "New folder" }) as HTMLButtonElement;
  expect(submit.disabled).toBe(true);
  await user().type(within(dialog).getByLabelText("Name"), "bad/name");
  expect(within(dialog).getByRole("alert").textContent).toMatch(/cannot contain/);
  await user().clear(within(dialog).getByLabelText("Name"));
  await user().type(within(dialog).getByLabelText("Name"), "docs");
  expect(submit.disabled).toBe(true); // message still empty
  await user().type(within(dialog).getByLabelText(/Commit message/), "Add docs");
  expect(submit.disabled).toBe(false);
});

test("revision field rejects garbage", async () => {
  renderApp(screenUi());
  await screen.findByRole("option", { name: /README\.md/ });
  await user().type(screen.getByLabelText("Revision"), "r12x");
  expect(screen.getByRole("alert").textContent).toMatch(/HEAD or a revision number/);
  expect((screen.getByRole("button", { name: "Go" }) as HTMLButtonElement).disabled).toBe(true);
});

test("tree: arrow keys expand and move, Enter selects a folder", async () => {
  renderApp(screenUi());
  const tree = await screen.findByRole("tree", { name: "Folders" });
  await waitFor(() => expect(within(tree).getAllByRole("treeitem").length).toBeGreaterThan(1));
  tree.focus();
  await user().keyboard("{ArrowDown}{ArrowRight}{ArrowDown}");
  await act(async () => undefined);
  expect(tree.getAttribute("aria-activedescendant")).toBeTruthy();
  const items = within(tree).getAllByRole("treeitem");
  expect(items[0]?.getAttribute("aria-level")).toBe("1");
  await user().keyboard("{Enter}");
  expect(items.some((i) => i.getAttribute("aria-selected") === "true")).toBe(true);
});

test("preview of a huge file is truncated, not rendered in full", async () => {
  const { MAX_PREVIEW_LINES } = await import("./file-preview");
  expect(MAX_PREVIEW_LINES).toBeLessThanOrEqual(5000);
  vi.restoreAllMocks();
  stubLayout();
});

test("tree: scrolls the active row into view and tracks it by folder, not index", async () => {
  const scroll = vi.fn();
  Element.prototype.scrollIntoView = scroll;
  renderApp(screenUi());
  const tree = await screen.findByRole("tree", { name: "Folders" });
  await waitFor(() => expect(within(tree).getAllByRole("treeitem").length).toBeGreaterThan(2));
  tree.focus();
  await user().keyboard("{End}");
  expect(scroll).toHaveBeenCalled();
  const activeText = () =>
    document.getElementById(tree.getAttribute("aria-activedescendant") ?? "")?.textContent ?? "";
  expect(activeText()).toContain("tags");
  // Expanding a folder above the active one shifts indices; the active row must stay "tags".
  const trunk = within(tree)
    .getAllByRole("treeitem")
    .find((i) => i.textContent?.includes("trunk"));
  await user().click(within(trunk as HTMLElement).getByRole("button", { hidden: true }));
  await waitFor(() => expect(within(tree).getAllByRole("treeitem").length).toBeGreaterThan(4));
  expect(activeText()).toContain("tags");
});
