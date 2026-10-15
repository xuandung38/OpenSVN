import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, expect, test } from "vitest";
import { mockCalls } from "../../../ipc/mock/adapter";
import { MOCK_REPO_ROOT, MOCK_SERVER_URL } from "../../../ipc/mock/remote-data";
import type { Lang } from "../../../lib/i18n/i18n";
import { renderWithQuery, setupMockIpc } from "../../../test-utils";
import { CheckoutDialog } from "./checkout-dialog";

beforeAll(async () => {
  await setupMockIpc("en");
});

beforeEach(() => {
  mockCalls.length = 0;
});

const VI_URL = "https://svn.example.com/repos/D%E1%BB%B1%20%C3%A1n%20Ti%E1%BA%BFng%20Vi%E1%BB%87t";

const text = {
  en: {
    url: "Repository URL",
    browse: "Browse…",
    tree: "Repositories on the server",
    note: "This URL is the server's list of repositories. Pick a repository, or a folder in one.",
    folder: "Folder name",
  },
  vi: {
    url: "URL repository",
    browse: "Duyệt…",
    tree: "Repository trên server",
    note: "URL này là danh sách repository trên server. Chọn một repository, hoặc một thư mục bên trong.",
    folder: "Tên thư mục",
  },
} as const;

async function browseServer(lang: Lang) {
  const t = text[lang];
  renderWithQuery(<CheckoutDialog onClose={() => {}} initialUrl={`${MOCK_SERVER_URL}/`} />, lang);
  const box = (await screen.findByRole("combobox", { name: t.url })) as HTMLInputElement;
  const user = userEvent.setup();
  await user.click(screen.getByRole("button", { name: t.browse }));
  const tree = await screen.findByRole("tree", { name: t.tree });
  return { box, tree, user, t };
}

for (const lang of ["en", "vi"] as const) {
  test(`a server URL lists its repositories as the top level (${lang})`, async () => {
    const { tree, t } = await browseServer(lang);
    expect(screen.getByText(t.note)).toBeTruthy();
    const items = within(tree).getAllByRole("treeitem");
    expect(items.map((i) => i.textContent)).toEqual(["game", "Dự án Tiếng Việt", "My Project"]);
    expect(items.every((i) => i.getAttribute("aria-level") === "1")).toBe(true);
    expect(items.every((i) => i.getAttribute("aria-expanded") === "false")).toBe(true);
    // The repositories come from the error itself: no listing was asked for yet.
    expect(mockCalls.filter((c) => c.name === "repoList")).toEqual([]);
  });

  test(`picking a repository fills in its encoded URL (${lang})`, async () => {
    const { box, tree, user, t } = await browseServer(lang);
    await user.click(within(tree).getByRole("treeitem", { name: "Dự án Tiếng Việt" }));
    expect(box.value).toBe(VI_URL);
    // The list stays for another pick, and the picked row is selected.
    const picked = within(tree).getByRole("treeitem", { name: "Dự án Tiếng Việt" });
    expect(picked.getAttribute("aria-selected")).toBe("true");
    const folder = screen.getByRole("textbox", { name: t.folder }) as HTMLInputElement;
    expect(folder.value).toBe("Dự án Tiếng Việt");
    await user.click(within(tree).getByRole("treeitem", { name: "My Project" }));
    expect(box.value).toBe("https://svn.example.com/repos/My%20Project");
  });
}

test("a repository opens into its folders and a folder can be picked by keyboard", async () => {
  const { box, tree, user } = await browseServer("en");
  tree.focus();
  // game is the first row: open it, move into trunk... and pick the first folder.
  await user.keyboard("{ArrowRight}");
  await waitFor(() => expect(within(tree).getAllByRole("treeitem").length).toBeGreaterThan(3));
  expect(mockCalls.filter((c) => c.name === "repoList").map((c) => c.args[0])).toEqual([
    MOCK_REPO_ROOT,
  ]);
  await user.keyboard("{ArrowDown}{Enter}");
  const first = within(tree).getAllByRole("treeitem")[1];
  expect(first?.getAttribute("aria-level")).toBe("2");
  expect(box.value).toBe(`${MOCK_REPO_ROOT}/${first?.textContent}`);
});

test("a repository URL still browses its own folders", async () => {
  renderWithQuery(<CheckoutDialog onClose={() => {}} initialUrl={`${MOCK_REPO_ROOT}/trunk`} />);
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "Browse…" }));
  const tree = await screen.findByRole("tree", { name: "Repository folders" });
  expect(within(tree).getAllByRole("treeitem")[0]?.textContent).toBe("game");
  expect(screen.queryByRole("tree", { name: "Repositories on the server" })).toBeNull();
});
