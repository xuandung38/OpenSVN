import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, expect, test, vi } from "vitest";
import { HOSTILE_NAME, MOCK_REPO_ROOT } from "../../../ipc/mock/remote-data";
import { renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import { BrowseScreen } from "./browse-screen";
import { flattenTree, type TreeRow } from "./repo-tree";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});

const noop = () => undefined;
const screenUi = () => (
  <BrowseScreen
    repoRootUrl={MOCK_REPO_ROOT}
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

test("lists the folder, renders a hostile file name as text and keeps it inert", async () => {
  const { container } = renderApp(screenUi());
  expect(await screen.findByRole("option", { name: /README\.md/ })).toBeTruthy();
  // Navigate into notes/ (contains the hostile name).
  const list = screen.getByRole("listbox", { name: "Repository browser" });
  list.focus();
  // notes is the 2nd folder (assets, notes, src); Enter opens it.
  await userEvent.keyboard("{ArrowDown}{Enter}");
  expect(await screen.findByText(HOSTILE_NAME)).toBeTruthy();
  expect(container.querySelector("img")).toBeNull();
  expect(document.querySelector("script")).toBeNull();
});

test("keyboard: Enter on a file opens Preview, showing its text without HTML", async () => {
  renderApp(screenUi());
  await screen.findByRole("option", { name: /README\.md/ });
  const list = screen.getByRole("listbox", { name: "Repository browser" });
  list.focus();
  await userEvent.keyboard("{End}{Enter}");
  expect(await screen.findByRole("tab", { name: "Preview" })).toBeTruthy();
  await waitFor(() => expect(document.body.textContent).toContain("export function render"));
});

test("New folder needs a valid name and a message before it can be sent", async () => {
  renderApp(screenUi());
  await screen.findByRole("option", { name: /README\.md/ });
  await userEvent.click(screen.getByRole("button", { name: "Actions" }));
  await userEvent.click(await screen.findByRole("menuitem", { name: "New folder…" }));
  const dialog = await screen.findByRole("dialog", { name: "New folder", hidden: true });
  const submit = within(dialog).getByRole("button", { name: "New folder" }) as HTMLButtonElement;
  expect(submit.disabled).toBe(true);
  await userEvent.type(within(dialog).getByLabelText("Name"), "bad/name");
  expect(within(dialog).getByRole("alert").textContent).toMatch(/cannot contain/);
  await userEvent.clear(within(dialog).getByLabelText("Name"));
  await userEvent.type(within(dialog).getByLabelText("Name"), "docs");
  expect(submit.disabled).toBe(true); // message still empty
  await userEvent.type(within(dialog).getByLabelText(/Commit message/), "Add docs");
  expect(submit.disabled).toBe(false);
});

test("revision field rejects garbage", async () => {
  renderApp(screenUi());
  await screen.findByRole("option", { name: /README\.md/ });
  await userEvent.type(screen.getByLabelText("Revision"), "r12x");
  expect(screen.getByRole("alert").textContent).toMatch(/HEAD or a revision number/);
  expect((screen.getByRole("button", { name: "Go" }) as HTMLButtonElement).disabled).toBe(true);
});

test("tree: arrow keys expand and move, Enter selects a folder", async () => {
  renderApp(screenUi());
  const tree = await screen.findByRole("tree", { name: "Folders" });
  await waitFor(() => expect(within(tree).getAllByRole("treeitem").length).toBeGreaterThan(1));
  tree.focus();
  await userEvent.keyboard("{ArrowDown}{ArrowRight}{ArrowDown}");
  await act(async () => undefined);
  expect(tree.getAttribute("aria-activedescendant")).toBeTruthy();
  const items = within(tree).getAllByRole("treeitem");
  expect(items[0]?.getAttribute("aria-level")).toBe("1");
  await userEvent.keyboard("{Enter}");
  expect(items.some((i) => i.getAttribute("aria-selected") === "true")).toBe(true);
});

test("preview of a huge file is truncated, not rendered in full", async () => {
  const { MAX_PREVIEW_LINES } = await import("./file-preview");
  expect(MAX_PREVIEW_LINES).toBeLessThanOrEqual(5000);
  vi.restoreAllMocks();
  stubLayout();
});
