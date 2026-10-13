import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, expect, test, vi } from "vitest";
import { HOSTILE_NAME, MOCK_REPO_ROOT } from "../../../ipc/mock/remote-data";
import { renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import { HistoryScreen } from "./history-screen";
import { splitLinks } from "./linkified-text";
import { applyClientFilters, type HistoryFilters, NO_FILTERS } from "./use-history";
import { expandWebUrl } from "./web-url";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});

const screenUi = (nav = {}) => (
  <HistoryScreen
    target={{ kind: "wc", wcId: "wc-main", path: "" }}
    repoRootUrl={MOCK_REPO_ROOT}
    scopeLabel={null}
    nav={nav}
  />
);

test("splitLinks links only https URLs and keeps trailing punctuation outside", () => {
  expect(splitLinks("see https://a.test/x. and http://b.test and javascript:alert(1)")).toEqual([
    { text: "see ", link: false },
    { text: "https://a.test/x", link: true },
    { text: ". and http://b.test and javascript:alert(1)", link: false },
  ]);
});

test("client filters: author and inclusive UTC date range", () => {
  const e = (rev: number, author: string, date: string) =>
    ({ rev, author, date, message: "", changedPaths: null, hasChildren: null }) as const;
  const entries = [
    e(3, "an", "2026-10-13T23:00:00Z"),
    e(2, "binh", "2026-10-12T01:00:00Z"),
    e(1, "an", "2026-10-10T01:00:00Z"),
  ];
  const f = (p: Partial<HistoryFilters>) =>
    applyClientFilters([...entries], { ...NO_FILTERS, ...p }).map((x) => x.rev);
  expect(f({})).toEqual([3, 2, 1]);
  expect(f({ author: "an" })).toEqual([3, 1]);
  expect(f({ from: "2026-10-12", to: "2026-10-13" })).toEqual([3, 2]);
  expect(f({ to: "2026-10-10" })).toEqual([1]);
});

test("expandWebUrl fills placeholders and only yields https URLs", () => {
  const root = "https://svn.example.com:8443/svn/game";
  const tpl = "https://{server}/!/#{repo}/commit/r{rev}/";
  expect(expandWebUrl(tpl, { repoRootUrl: root, rev: 5 })).toBe(
    "https://svn.example.com:8443/!/#game/commit/r5/",
  );
  expect(
    expandWebUrl("https://{server}/!/#{repo}/view/{revSpec}/{path}", {
      repoRootUrl: root,
      path: "/trunk/a b.txt",
    }),
  ).toBe("https://svn.example.com:8443/!/#game/view/head/trunk/a%20b.txt");
  expect(expandWebUrl("http://{server}/x", { repoRootUrl: root })).toBeNull();
  expect(expandWebUrl("", { repoRootUrl: root })).toBeNull();
  expect(expandWebUrl("javascript:alert(1)", { repoRootUrl: root })).toBeNull();
});

test("shows revisions, hostile text stays inert, selecting shows message and changed paths", async () => {
  const { container } = renderApp(screenUi());
  const hostile = await screen.findByRole("option", { name: /Rename asset/ });
  expect(hostile.textContent).toContain(HOSTILE_NAME);
  expect(container.querySelector("img")).toBeNull();
  const branch = await screen.findByRole("option", { name: /Add feature-x branch/ });
  await userEvent.click(branch);
  const link = await screen.findByRole("link", { name: "https://tracker.example.com/issue/42" });
  expect(link.getAttribute("href")).toBe("https://tracker.example.com/issue/42");
  // The <script> in the message is shown literally.
  expect(document.body.textContent).toContain("<script>alert(1)</script>");
  expect(document.querySelector("script")).toBeNull();
  expect(screen.getByText("Changed paths")).toBeTruthy();
});

test("text filter is sent to the server and narrows the list", async () => {
  renderApp(screenUi());
  await screen.findByRole("option", { name: /Add feature-x branch/ });
  await userEvent.type(screen.getByLabelText("Filter revisions"), "feature-x");
  await waitFor(() => expect(screen.queryByRole("option", { name: /Update module 5/ })).toBeNull());
  expect(screen.getByRole("option", { name: /Add feature-x branch/ })).toBeTruthy();
});

test("author filter lists loaded authors", async () => {
  renderApp(screenUi());
  await screen.findByRole("option", { name: /Add feature-x branch/ });
  const select = screen.getByLabelText("Author") as HTMLSelectElement;
  await waitFor(() => expect(within(select).getAllByRole("option").length).toBeGreaterThan(2));
  await userEvent.selectOptions(select, "binh");
  const options = screen.getAllByRole("option").filter((o) => o.id);
  expect(options.length).toBeGreaterThan(0);
  for (const o of options) expect(o.textContent).toContain("binh");
});

test("keyboard: Down + Enter in the list selects a revision", async () => {
  renderApp(screenUi());
  const list = await screen.findByRole("listbox", { name: "Revisions" });
  await screen.findByRole("option", { name: /Add feature-x branch/ });
  list.focus();
  await userEvent.keyboard("{ArrowDown}{Enter}");
  await waitFor(() => expect(screen.getByText("r48212", { selector: "b" })).toBeTruthy());
});

test("changed path menu offers diff and navigation, and opens the diff sheet", async () => {
  const openInBrowse = vi.fn();
  renderApp(screenUi({ openInBrowse }));
  await userEvent.click(await screen.findByRole("option", { name: /Fix shadow cascade flicker/ }));
  await userEvent.click(
    await screen.findByRole("button", { name: /Actions: \/trunk\/src\/engine\/renderer\.ts/ }),
  );
  await userEvent.click(
    await screen.findByRole("menuitem", { name: "Open file at this revision" }),
  );
  expect(openInBrowse).toHaveBeenCalledWith(
    `${MOCK_REPO_ROOT}/trunk/src/engine/renderer.ts`,
    48213,
  );
  await userEvent.click(
    screen.getByRole("button", { name: /Actions: \/trunk\/src\/engine\/renderer\.ts/ }),
  );
  await userEvent.click(await screen.findByRole("menuitem", { name: "Show diff" }));
  expect(
    await screen.findByRole("dialog", { name: "Changes in r48213", hidden: true }),
  ).toBeTruthy();
  await waitFor(() => expect(document.body.textContent).toContain("const c = 4;"));
});
