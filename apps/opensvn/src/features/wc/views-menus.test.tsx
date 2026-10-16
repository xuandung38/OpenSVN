import { act, configure, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, test, vi } from "vitest";
import { AppShell } from "../../app/app-shell";
import type { FileStatus, RepoEntry } from "../../ipc/bindings";
import { mockAdapter, mockCalls, mockEmit, setMockOpDuration } from "../../ipc/mock/adapter";
import { MOCK_WC_ID } from "../../ipc/mock/data";
import { resetWcMock } from "../../ipc/mock/wc-mock-commands";
import { setExternalOpener } from "../../lib/external-link";
import { expectNoSeriousA11yViolations } from "../../test-axe";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../test-utils";
import { compareListings, isCompareTarget } from "./compare/compare-model";
import { DOCS_URL } from "./help/help-links";
import { parseViewPrefs, reloadViewPrefs } from "./view-prefs";
import { buildRows, listedFiles, rowGlyph } from "./wc-model";

vi.mock("./diff/diff-merge-editor", () => ({
  DiffMergeEditor: ({ left, right }: { left: string; right: string }) => (
    <div data-testid="editor">{`${left.length}|${right.length}`}</div>
  ),
}));

vi.setConfig({ testTimeout: 120_000 });
const { user } = installFakeClock();
configure({ asyncUtilTimeout: 8_000 });

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
}, 20_000);

beforeEach(() => {
  localStorage.clear();
  reloadViewPrefs();
  resetWcMock();
  mockCalls.length = 0;
  setMockOpDuration(40);
});

afterEach(() => setExternalOpener(null));

const emit = (name: string) => act(async () => mockEmit(name));
const changes = () => screen.getByRole("listbox", { name: "Changed files" });
const option = (name: RegExp) => within(changes()).queryByRole("option", { name });

/**
 * axe schedules work on timers, which this file's fake clock only moves when told to: advance it
 * while the check runs.
 */
async function a11y(label: string, root: Element): Promise<void> {
  let done = false;
  const run = expectNoSeriousA11yViolations(label, root).finally(() => {
    done = true;
  });
  while (!done) await vi.advanceTimersByTimeAsync(20);
  await run;
}

/** The tick box of a list row (pointer only; the option carries the state). */
function tick(row: HTMLElement): HTMLElement {
  const el = row.querySelector<HTMLElement>("[data-row-check]");
  if (!el) throw new Error("row has no tick box");
  return el;
}

async function boot() {
  renderApp(<AppShell />);
  await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });
}

function fs(path: string, text: FileStatus["text"], extra: Partial<FileStatus> = {}): FileStatus {
  return {
    path,
    kind: "file",
    text,
    props: "none",
    treeConflict: false,
    movedFrom: null,
    movedTo: null,
    changelist: null,
    lock: null,
    needsLock: false,
    remote: null,
    binary: false,
    size: 1,
    ...extra,
  };
}

function entry(name: string, kind: "file" | "dir", createdRev: number, size = 10): RepoEntry {
  return {
    name,
    kind,
    size: kind === "file" ? size : null,
    createdRev,
    lastAuthor: "an",
    date: "2026-10-10T09:00:00Z",
    lock: null,
  };
}

describe("view model", () => {
  const files = [
    fs("src/a.ts", "modified"),
    fs("src/lib/b.ts", "added", { changelist: "refactor" }),
    fs("notes.txt", "unversioned"),
    fs("build/out.bin", "ignored"),
    fs("src/c.ts", "normal"),
  ];

  test("listed files follow Show unversioned / Show ignored", () => {
    const paths = (p: { showUnversioned: boolean; showIgnored: boolean }) =>
      listedFiles(files, p).map((f) => f.path);
    expect(paths({ showUnversioned: true, showIgnored: false })).toEqual([
      "src/a.ts",
      "src/lib/b.ts",
      "notes.txt",
    ]);
    expect(paths({ showUnversioned: false, showIgnored: true })).toEqual([
      "src/a.ts",
      "src/lib/b.ts",
      "build/out.bin",
    ]);
    expect(rowGlyph(fs("x", "ignored"))).toBe("I");
  });

  test("rows: groups only when asked, tree with folder rows before their files", () => {
    const listed = listedFiles(files, { showUnversioned: true, showIgnored: false });
    expect(buildRows(listed, { group: false, tree: false }).map((r) => r.kind)).toEqual([
      "file",
      "file",
      "file",
    ]);
    const tree = buildRows(listed, { group: false, tree: true });
    expect(
      tree.map((r) =>
        r.kind === "dir" ? `${r.depth}:${r.name}/` : r.kind === "file" ? r.file.path : r.name,
      ),
    ).toEqual(["0:src/", "1:lib/", "src/lib/b.ts", "src/a.ts", "notes.txt"]);
    const src = tree[0];
    expect(src?.kind === "dir" && src.paths).toEqual(["src/lib/b.ts", "src/a.ts"]);
    const grouped = buildRows(listed, { group: true, tree: true });
    expect(
      grouped.filter((r) => r.kind === "group").map((r) => r.kind === "group" && r.name),
    ).toEqual([null, "refactor"]);
  });

  test("a folder tree keeps every file of a folder together (a-b sorts apart from a)", () => {
    const rows = buildRows(
      [fs("a/x", "modified"), fs("a-b/y", "modified"), fs("a/z", "modified")],
      { group: false, tree: true },
    );
    expect(
      rows.map((r) => (r.kind === "dir" ? `${r.path}/` : r.kind === "file" ? r.file.path : "")),
    ).toEqual(["a/", "a/x", "a/z", "a-b/", "a-b/y"]);
  });

  test("stored prefs: defaults for missing, corrupt or wrongly typed values", () => {
    expect(parseViewPrefs(null)).toEqual({
      showUnversioned: true,
      showIgnored: false,
      groupByChangelist: true,
      tree: false,
    });
    expect(parseViewPrefs("{not json")).toEqual(parseViewPrefs(null));
    expect(parseViewPrefs('{"tree":true,"showIgnored":"yes"}')).toMatchObject({
      tree: true,
      showIgnored: false,
    });
  });

  test("compare: same revision and size is unchanged, folders first, one-sided entries", () => {
    const rows = compareListings(
      [
        entry("b.txt", "file", 5),
        entry("a.txt", "file", 5),
        entry("src", "dir", 7),
        entry("x", "file", 1),
      ],
      [
        entry("b.txt", "file", 5),
        entry("a.txt", "file", 9),
        entry("src", "dir", 7),
        entry("y", "dir", 3),
      ],
    );
    expect(rows.map((r) => `${r.name}:${r.state}`)).toEqual([
      "src:same",
      "y:onlyRight",
      "a.txt:changed",
      "b.txt:same",
      "x:onlyLeft",
    ]);
    expect(compareListings([entry("n", "file", 1)], [entry("n", "dir", 1)])[0]?.state).toBe(
      "kindChanged",
    );
    const root = "https://svn.example.com/repos/game";
    expect(isCompareTarget(`${root}/branches/x/`, root, `${root}/trunk`)).toBe(true);
    expect(isCompareTarget(`${root}/trunk`, root, `${root}/trunk`)).toBe(false);
    expect(isCompareTarget("https://other.example.com/x", root, `${root}/trunk`)).toBe(false);
    expect(isCompareTarget("javascript:alert(1)", root, `${root}/trunk`)).toBe(false);
  });
});

test("View menu: show ignored, hide unversioned (and untick them), tree view, changelist groups", async () => {
  await boot();
  expect(option(/build\/game\.exe/)).toBeNull();

  await emit("menu://view/show-ignored");
  expect(await screen.findByText("Show ignored files: on")).toBeTruthy();
  const ignored = await within(changes()).findByRole("option", { name: /build\/game\.exe/ });
  expect(within(ignored).getByRole("img", { name: "Ignored" })).toBeTruthy();
  // Listed for reference only: no commit tick.
  expect(ignored.hasAttribute("aria-checked")).toBe(false);
  await waitFor(() =>
    expect(
      mockCalls.some(
        (c) => c.name === "statusGet" && (c.args[0] as { includeIgnored: boolean }).includeIgnored,
      ),
    ).toBe(true),
  );

  // Tick the unversioned file, then hide unversioned files: it leaves the list and the commit.
  const unversioned = option(/tools\/readme-renamed\.md/) as HTMLElement;
  await user().click(tick(unversioned));
  await waitFor(() => expect(unversioned.getAttribute("aria-checked")).toBe("true"));
  await emit("menu://view/show-unversioned");
  await waitFor(() => expect(option(/tools\/readme-renamed\.md/)).toBeNull());
  expect(screen.getByText("Show unversioned files: off")).toBeTruthy();
  await emit("menu://view/show-unversioned");
  const back = await within(changes()).findByRole("option", { name: /tools\/readme-renamed\.md/ });
  expect(back.getAttribute("aria-checked")).toBe("false");

  // Tree view: folder rows with a tri-state tick; the file rows show the name only.
  await emit("menu://view/tree");
  const engine = await within(changes()).findByRole("option", { name: /^Folder src\/engine,/ });
  expect(engine.getAttribute("aria-checked")).toBe("mixed");
  expect(within(changes()).getByRole("option", { name: /renderer\.ts/ }).textContent).not.toContain(
    "src/engine/",
  );
  await a11y("tree view", changes());
  await emit("menu://view/tree");
  await waitFor(() => expect(option(/^Folder src\/engine,/)).toBeNull());

  // Changelist groups can be turned off.
  await act(async () => {
    await mockAdapter.commands.changelistSet(MOCK_WC_ID, ["src/engine/renderer.ts"], "refactor");
  });
  expect(
    await within(changes()).findByRole("option", { name: /Changelist: refactor/ }),
  ).toBeTruthy();
  await emit("menu://view/group-changelist");
  await waitFor(() => expect(option(/Changelist: refactor/)).toBeNull());

  // The choices are remembered.
  expect(JSON.parse(localStorage.getItem("opensvn.viewPrefs") ?? "{}")).toMatchObject({
    showIgnored: true,
    showUnversioned: true,
    groupByChangelist: false,
    tree: false,
  });
});

test("the View options menu in the Changes list toggles the same prefs", async () => {
  await boot();
  await user().click(screen.getByRole("button", { name: "View options" }));
  const menu = await screen.findByRole("menu");
  const tree = within(menu).getByRole("menuitemcheckbox", { name: "Show as tree" });
  expect(tree.getAttribute("aria-checked")).toBe("false");
  await a11y("view options menu", menu);
  await user().click(tree);
  expect(await within(changes()).findByRole("option", { name: /^Folder src,/ })).toBeTruthy();
});

test("Branch > Compare with branch: pick a branch, open changed folders, diff a file", async () => {
  await boot();
  await emit("menu://branch/compare");
  const dlg = await screen.findByRole("dialog", { name: "Compare with branch", hidden: true });
  const q = within(dlg);
  const compare = q.getByRole("button", { name: "Compare", hidden: true });
  expect(compare.hasAttribute("disabled")).toBe(true);
  await user().selectOptions(
    await q.findByRole("combobox", { name: "Branch or tag", hidden: true }),
    await q.findByRole("option", { name: "feature-x", hidden: true }),
  );
  await user().click(compare);

  const table = await q.findByRole("table", { name: "Entries of (top)", hidden: true });
  const row = (name: string) =>
    within(table)
      .getAllByRole("row", { hidden: true })
      .find((r) => r.textContent?.startsWith(name));
  expect(row("README.md")?.textContent).toContain("Changed");
  expect(row("CHANGELOG.md")?.textContent).toContain("Only in other");
  // Unchanged entries are hidden until asked for.
  expect(row("build.gradle")).toBeUndefined();
  await user().click(q.getByRole("checkbox", { name: "Show unchanged", hidden: true }));
  expect(row("build.gradle")?.textContent).toContain("Unchanged");
  await a11y("compare dialog", dlg);

  await user().click(q.getByRole("button", { name: "Open src", hidden: true }));
  await user().click(await q.findByRole("button", { name: "Open engine", hidden: true }));
  const engine = await q.findByRole("table", { name: "Entries of /src/engine", hidden: true });
  const states = within(engine)
    .getAllByRole("row", { hidden: true })
    .slice(1)
    .map((r) => r.textContent);
  expect(states).toEqual(
    expect.arrayContaining([
      "particles.tsOnly in other",
      "renderer.tsChanged",
      "scene-graph.tsOnly in current",
    ]),
  );
  await user().click(q.getByRole("button", { name: "Show diff of renderer.ts", hidden: true }));
  expect(
    await q.findByRole("region", { name: "Diff of src/engine/renderer.ts", hidden: true }),
  ).toBeTruthy();
  await waitFor(() => expect(q.getByTestId("editor")).toBeTruthy());
  const cats = mockCalls.filter((c) => c.name === "repoCat").map((c) => c.args[0]);
  expect(cats).toEqual(
    expect.arrayContaining([
      "https://svn.example.com/repos/game/trunk/src/engine/renderer.ts",
      "https://svn.example.com/repos/game/branches/feature-x/src/engine/renderer.ts",
    ]),
  );

  await user().click(q.getByRole("button", { name: "Up one folder", hidden: true }));
  expect(await q.findByRole("table", { name: "Entries of /src", hidden: true })).toBeTruthy();
});

test("Help: documentation opens in the browser, logs folder, report a problem with preview", async () => {
  const opened: string[] = [];
  setExternalOpener((url) => {
    opened.push(url);
  });
  await boot();

  await emit("menu://help/docs");
  await waitFor(() => expect(opened).toEqual([DOCS_URL]));

  await emit("menu://help/logs");
  await waitFor(() => expect(mockCalls.some((c) => c.name === "logsReveal")).toBe(true));

  await emit("menu://help/report");
  const dlg = await screen.findByRole("dialog", { name: "Report a problem", hidden: true });
  const q = within(dlg);
  const files = await q.findByRole("list", { name: "Files in the bundle", hidden: true });
  expect(within(files).getAllByRole("listitem", { hidden: true })).toHaveLength(3);
  await user().click(q.getByRole("button", { name: /logs\/opensvn\.log/, hidden: true }));
  const content = q.getByRole("region", { name: "Content of logs/opensvn.log", hidden: true });
  // Paths and commit messages are hidden by default.
  expect(content.textContent).toContain("<home>");
  expect(content.textContent).toContain("<message removed>");
  await a11y("report dialog", dlg);

  await user().click(q.getByRole("checkbox", { name: "Hide folder paths", hidden: true }));
  await waitFor(() =>
    expect(
      q.getByRole("region", { name: "Content of logs/opensvn.log", hidden: true }).textContent,
    ).toContain("/Users/dev"),
  );
  await user().click(q.getByRole("button", { name: "Save as zip…", hidden: true }));
  expect(
    await screen.findByText("Saved diagnostics to /Users/dev/Downloads/opensvn-diagnostics.zip"),
  ).toBeTruthy();
  const exported = mockCalls.filter((c) => c.name === "diagnosticsExport");
  const previews = mockCalls.filter((c) => c.name === "diagnosticsPreview");
  expect(exported).toHaveLength(1);
  expect(previews.at(-1)?.args[0]).toEqual({ hidePaths: false, hideCommitMessages: true });
});
