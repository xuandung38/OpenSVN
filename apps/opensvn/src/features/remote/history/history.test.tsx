import { screen, waitFor, within } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import { HOSTILE_NAME, MOCK_REPO_ROOT } from "../../../ipc/mock/remote-data";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import { filesBetween } from "./compare-dialog";
import { HistoryScreen } from "./history-screen";
import { splitLinks } from "./linkified-text";
import { applyClientFilters, type HistoryFilters, NO_FILTERS } from "./use-history";
import { expandWebUrl } from "./web-url";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});

// Fake clock: findBy/waitFor budgets count simulated time, not wall-clock time, so the suite stays
// green when the machine is loaded (these tests used to time out under parallel load).
const { user } = installFakeClock();
// The per-test timeout is only a hang guard now; role queries over the rendered list are CPU-bound
// and can take several wall-clock seconds when the machine is busy.
vi.setConfig({ testTimeout: 120_000 });

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
  await user().click(branch);
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
  await user().type(screen.getByLabelText("Filter revisions"), "feature-x");
  await waitFor(() => expect(screen.queryByRole("option", { name: /Update module 5/ })).toBeNull());
  expect(screen.getByRole("option", { name: /Add feature-x branch/ })).toBeTruthy();
});

test("author filter lists loaded authors", async () => {
  renderApp(screenUi());
  await screen.findByRole("option", { name: /Add feature-x branch/ });
  const select = screen.getByLabelText("Author") as HTMLSelectElement;
  await waitFor(() => expect(within(select).getAllByRole("option").length).toBeGreaterThan(2));
  await user().selectOptions(select, "binh");
  const options = screen.getAllByRole("option").filter((o) => o.id);
  expect(options.length).toBeGreaterThan(0);
  for (const o of options) expect(o.textContent).toContain("binh");
});

test("keyboard: Down + Enter in the list selects a revision", async () => {
  renderApp(screenUi());
  const list = await screen.findByRole("listbox", { name: "Revisions" });
  await screen.findByRole("option", { name: /Add feature-x branch/ });
  list.focus();
  await user().keyboard("{ArrowDown}{Enter}");
  await waitFor(() => expect(screen.getByText("r48212", { selector: "b" })).toBeTruthy());
});

test("changed path menu offers diff and navigation, and opens the diff sheet", async () => {
  const openInBrowse = vi.fn();
  renderApp(screenUi({ openInBrowse }));
  await user().click(await screen.findByRole("option", { name: /Fix shadow cascade flicker/ }));
  await user().click(
    await screen.findByRole("button", { name: /Actions: \/trunk\/src\/engine\/renderer\.ts/ }),
  );
  await user().click(await screen.findByRole("menuitem", { name: "Open file at this revision" }));
  expect(openInBrowse).toHaveBeenCalledWith(
    `${MOCK_REPO_ROOT}/trunk/src/engine/renderer.ts`,
    48213,
  );
  await user().click(
    screen.getByRole("button", { name: /Actions: \/trunk\/src\/engine\/renderer\.ts/ }),
  );
  await user().click(await screen.findByRole("menuitem", { name: "Show diff" }));
  expect(
    await screen.findByRole("dialog", { name: "Changes in r48213", hidden: true }),
  ).toBeTruthy();
  await waitFor(() => expect(document.body.textContent).toContain("const c = 4;"));
});

test("filesBetween lists files changed after `from` up to `to`, without folders or repeats", () => {
  const e = (rev: number, paths: [string, "A" | "M" | "D", "file" | "dir" | null][]) => ({
    rev,
    author: null,
    date: "2026-10-10T00:00:00Z",
    message: "",
    hasChildren: null,
    changedPaths: paths.map(([path, action, kind]) => ({ path, action, kind, copyFrom: null })),
  });
  const entries = [
    e(5, [["/a.txt", "M", "file"]]),
    e(4, [
      ["/dir", "A", "dir"],
      ["/b.txt", "A", null],
    ]),
    e(3, [["/a.txt", "A", "file"]]),
    e(2, [["/c.txt", "M", "file"]]),
  ];
  expect(filesBetween(entries as never, 2, 5)).toEqual([
    { path: "/a.txt", action: "M" },
    { path: "/b.txt", action: "A" },
  ]);
  expect(filesBetween(entries as never, 5, 5)).toEqual([]);
});

test("compare two revisions: Space marks one, select another, diff_rev(from, to) per file", async () => {
  const { mockCalls } = await import("../../../ipc/mock/adapter");
  renderApp(screenUi());
  const list = await screen.findByRole("listbox", { name: "Revisions" });
  await screen.findByRole("option", { name: /Fix shadow cascade flicker/ });
  list.focus();
  await user().keyboard(" ");
  expect(await screen.findByText(/r48213 is marked for comparison/)).toBeTruthy();
  expect(screen.getByRole("option", { name: /compare.*Fix shadow cascade flicker/ })).toBeTruthy();
  await user().keyboard("{ArrowDown}{ArrowDown}{Enter}");
  const compare = await screen.findByRole("button", { name: "Compare r48211 with r48213" });
  await user().click(compare);
  const picker = await screen.findByRole("dialog", {
    name: "Compare r48211 with r48213",
    hidden: true,
  });
  const files = within(picker)
    .getAllByRole("button")
    .filter((b) => b.textContent?.startsWith("/"));
  expect(files.length).toBeGreaterThan(0);
  const before = mockCalls.length;
  await user().click(files[0] as HTMLElement);
  expect(
    await screen.findByRole("dialog", { name: "Changes from r48211 to r48213", hidden: true }),
  ).toBeTruthy();
  await waitFor(() => expect(mockCalls.slice(before).some((c) => c.name === "diffGet")).toBe(true));
  const diff = mockCalls.slice(before).find((c) => c.name === "diffGet");
  expect(diff?.args[0]).toMatchObject({ target: { kind: "range", from: 48211, to: 48213 } });
});

test("the revision menu marks and unmarks a revision for comparison", async () => {
  renderApp(screenUi());
  await user().click(await screen.findByRole("option", { name: /Add feature-x branch/ }));
  await user().click(await screen.findByRole("button", { name: "Actions" }));
  await user().click(await screen.findByRole("menuitem", { name: "Mark r48212 for comparison" }));
  expect(await screen.findByText(/r48212 is marked for comparison/)).toBeTruthy();
  await user().click(screen.getByRole("button", { name: "Clear comparison" }));
  await waitFor(() => expect(screen.queryByText(/is marked for comparison/)).toBeNull());
});
