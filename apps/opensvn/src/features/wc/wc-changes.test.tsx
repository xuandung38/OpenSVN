import { configure, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, expect, test, vi } from "vitest";
import { AppShell } from "../../app/app-shell";
import type { FileStatus } from "../../ipc/bindings";
import { mockCalls, setMockOpDuration } from "../../ipc/mock/adapter";
import { MOCK_WC_ID, mockWorkingCopies } from "../../ipc/mock/data";
import { resetWcMock } from "../../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../test-utils";
import { ChangesList } from "./changes/changes-list";
import { WcSessionProvider } from "./wc-session";

// CodeMirror needs real layout; the editor is replaced by a probe that exposes its input.
vi.mock("./diff/diff-merge-editor", () => ({
  DiffMergeEditor: ({ left, right, mode }: { left: string; right: string; mode: string }) => (
    <div data-testid="editor" data-mode={mode}>
      {`${left}|${right}`}
    </div>
  ),
}));

// Each test boots the whole window in jsdom (10-20 s of CPU). With the fake clock below, waits no
// longer depend on wall-clock time; this timeout is only a hang guard, sized for a starved machine.
vi.setConfig({ testTimeout: 120_000 });
// Fake clock: waits count simulated time, so a loaded machine cannot make them run out.
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

const option = (name: RegExp) => screen.findByRole("option", { name });

test("changes list ticks tracked changes, leaves unversioned files unticked, shows lock and needs-lock", async () => {
  renderApp(<AppShell />);
  expect((await option(/src\/engine\/renderer\.ts/)).getAttribute("aria-checked")).toBe("true");
  expect((await option(/notes\/todo\.txt/)).getAttribute("aria-checked")).toBe("false");
  expect((await option(/src\/engine\/physics\.ts/)).getAttribute("aria-checked")).toBe("false");
  expect(within(await option(/hero-diffuse\.png/)).getByText("Locked by you")).toBeTruthy();
  expect(within(await option(/audio\.ts/)).getByText("Newer version on the server")).toBeTruthy();
});

test("select all toggles every visible file; chips filter the list", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  const all = screen.getByRole("checkbox", { name: "Select all files" });
  await user().click(all);
  expect((await option(/notes\/todo\.txt/)).getAttribute("aria-checked")).toBe("true");
  await user().click(all);
  expect((await option(/renderer\.ts/)).getAttribute("aria-checked")).toBe("false");
  await user().click(screen.getByRole("button", { name: "Unversioned" }));
  expect(screen.queryByRole("option", { name: /renderer\.ts/ })).toBeNull();
  expect(await option(/notes\/todo\.txt/)).toBeTruthy();
});

test("a missing and an unversioned file with the same name offer 'Record as move'", async () => {
  renderApp(<AppShell />);
  await user().click(await screen.findByRole("button", { name: "Record as move" }));
  await waitFor(() => expect(mockCalls.some((c) => c.name === "markMove")).toBe(true));
  expect(mockCalls.find((c) => c.name === "markMove")?.args.slice(1)).toEqual([
    "docs/readme-renamed.md",
    "tools/readme-renamed.md",
  ]);
  await waitFor(() => expect(screen.queryByRole("button", { name: "Record as move" })).toBeNull());
});

test("commit needs a summary, reports out-of-date, recovers through Update and commits", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  const commit = await screen.findByRole("button", { name: /^Commit \d+ files to trunk/ });
  expect((commit as HTMLButtonElement).disabled).toBe(true);
  await user().type(screen.getByRole("textbox", { name: "Summary (required)" }), "Tune renderer");
  expect((commit as HTMLButtonElement).disabled).toBe(false);

  await user().click(commit);
  expect(
    await screen.findByText("The server has newer changes. Update, then commit again."),
  ).toBeTruthy();
  await user().click(screen.getByRole("button", { name: "Update now" }));
  await waitFor(() => expect(mockCalls.some((c) => c.name === "update")).toBe(true));
  await waitFor(() => expect(screen.queryByText(/Newer version on the server/)).toBeNull());

  await user().click(screen.getByRole("button", { name: /^Commit \d+ files to trunk/ }));
  expect(await screen.findByText("Committed r48214", {}, { timeout: 4000 })).toBeTruthy();
  const sent = mockCalls.filter((c) => c.name === "commit").at(-1)?.args[0] as {
    message: string;
    autoAddRemove: boolean;
  };
  expect(sent.message).toBe("Tune renderer");
  expect(sent.autoAddRemove).toBe(true);
  // The committed files leave the list and the box is cleared.
  await waitFor(() => expect(screen.queryByRole("option", { name: /renderer\.ts/ })).toBeNull());
  expect(
    (screen.getByRole("textbox", { name: "Summary (required)" }) as HTMLInputElement).value,
  ).toBe("");
});

test("commit box warns about foreign locks, long summaries and blocks on conflicts", async () => {
  renderApp(<AppShell />);
  await option(/crate\.fbx/);
  expect(await screen.findByText("1 file(s) are locked by another user.")).toBeTruthy();
  await user().click(await option(/physics\.ts/)); // select only
  await user().click(within(await option(/physics\.ts/)).getByRole("checkbox", { hidden: true }));
  expect(await screen.findByText(/Resolve conflicts in 1 selected file/)).toBeTruthy();
  const summary = screen.getByRole("textbox", { name: "Summary (required)" });
  await user().type(summary, "x".repeat(73));
  expect(screen.getByText("Summary is longer than 72 characters")).toBeTruthy();
  expect(
    (screen.getByRole("button", { name: /^Commit \d+ files to trunk/ }) as HTMLButtonElement)
      .disabled,
  ).toBe(true);
});

test("recent commit messages come back through the history menu", async () => {
  localStorage.setItem("opensvn.commitMessages", JSON.stringify(["Old subject\n\nold body"]));
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().click(screen.getByRole("button", { name: "Recent messages" }));
  await user().click(await screen.findByRole("menuitem", { name: /Old subject/ }));
  expect(
    (screen.getByRole("textbox", { name: "Summary (required)" }) as HTMLInputElement).value,
  ).toBe("Old subject");
  expect((screen.getByRole("textbox", { name: "Description" }) as HTMLTextAreaElement).value).toBe(
    "old body",
  );
  localStorage.clear();
});

test("diff: text opens the editor, unified toggles, binary / too complex / too large are explained", async () => {
  renderApp(<AppShell />);
  await user().click(await option(/src\/engine\/renderer\.ts/));
  const editor = await screen.findByTestId("editor");
  expect(editor.getAttribute("data-mode")).toBe("split");
  await user().click(screen.getByRole("button", { name: "Unified" }));
  expect(screen.getByTestId("editor").getAttribute("data-mode")).toBe("unified");

  await user().click(await option(/hero-diffuse\.png/));
  expect(await screen.findByText("Binary file")).toBeTruthy();
  expect(screen.queryByTestId("editor")).toBeNull();

  await user().click(await option(/big-table\.csv/));
  expect(await screen.findByText("Diff too complex")).toBeTruthy();
  expect(screen.queryByTestId("editor")).toBeNull();
  await user().click(screen.getByRole("button", { name: "Open in external tool" }));
  // The backend flag drives the hand-off; with no tool configured the backend's message is shown.
  await waitFor(() =>
    expect(mockCalls.filter((c) => c.name === "diffExternal").at(-1)?.args[0]).toEqual({
      kind: "wc",
      wcId: MOCK_WC_ID,
      path: "data/big-table.csv",
    }),
  );
  expect(await screen.findByText(/No external diff tool is configured/)).toBeTruthy();

  await user().click(await option(/huge-trace\.log/));
  expect(await screen.findByText("Large file")).toBeTruthy();
  await user().click(screen.getByRole("button", { name: "Load full diff" }));
  expect(await screen.findByTestId("editor")).toBeTruthy();
  const req = mockCalls.filter((c) => c.name === "diffGet").at(-1)?.args[0] as {
    forceLarge: boolean;
  };
  expect(req.forceLarge).toBe(true);
});

test("diff: property changes show under the content", async () => {
  renderApp(<AppShell />);
  await user().click(await option(/scene-graph\.ts/));
  const props = await screen.findByRole("region", { name: "Property changes" });
  expect(within(props).getByText("svn:eol-style")).toBeTruthy();
});

test("keyboard: Ctrl+2 opens History, Ctrl+Shift+U updates, Ctrl+Shift+K checks for updates", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}2{/Control}");
  await waitFor(() =>
    expect(
      within(screen.getByRole("tablist", { name: "Views" }))
        .getByRole("tab", { name: "History" })
        .getAttribute("aria-selected"),
    ).toBe("true"),
  );
  await user().keyboard("{Control>}{Shift>}u{/Shift}{/Control}");
  await waitFor(() => expect(mockCalls.some((c) => c.name === "update")).toBe(true));
  await user().keyboard("{Control>}{Shift>}k{/Shift}{/Control}");
  await waitFor(() => expect(mockCalls.some((c) => c.name === "statusRemote")).toBe(true));
});

test("incoming tab lists the revisions that are not in the working copy yet", async () => {
  renderApp(<AppShell />);
  await user().click(await screen.findByRole("tab", { name: /Incoming/ }));
  expect(await screen.findByText("3 new revisions on the server")).toBeTruthy();
  await user().click(screen.getByRole("button", { name: "Update to revision…" }));
  const dlg = await screen.findByRole("dialog", { name: "Update to revision", hidden: true });
  await user().type(within(dlg).getByRole("textbox", { name: "Revision" }), "48212");
  await user().click(within(dlg).getByRole("button", { name: "Update" }));
  await waitFor(() => expect(mockCalls.some((c) => c.name === "update")).toBe(true));
  const call = mockCalls.find((c) => c.name === "update");
  expect((call?.args[0] as { rev: number } | undefined)?.rev).toBe(48212);
});

test("hostile file names render as text, never as markup", async () => {
  const hostile = "<img src=x onerror=alert(1)>.txt";
  const file: FileStatus = {
    path: `dir/${hostile}`,
    kind: "file",
    text: "modified",
    props: "none",
    treeConflict: false,
    movedFrom: null,
    movedTo: null,
    changelist: "<script>alert(2)</script>",
    lock: null,
    needsLock: false,
    remote: null,
    binary: false,
    size: 1,
  };
  const { container } = renderApp(
    <WcSessionProvider wc={mockWorkingCopies[0] ?? null} files={[file]}>
      <ChangesList />
    </WcSessionProvider>,
  );
  expect(await screen.findByText(hostile)).toBeTruthy();
  expect(screen.getByText("Changelist: <script>alert(2)</script> (1)")).toBeTruthy();
  expect(container.querySelector("img, script")).toBeNull();
});
