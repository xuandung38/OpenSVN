import { act, configure, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, expect, test, vi } from "vitest";
import { AppShell } from "../../app/app-shell";
import { mockAdapter, mockCalls, mockEmit, setMockOpDuration } from "../../ipc/mock/adapter";
import { resetWcMock } from "../../ipc/mock/wc-mock-commands";
import { renderApp, setupMockIpc, stubLayout } from "../../test-utils";
import { WcSessionProvider } from "./wc-session";
import { WelcomeScreen } from "./welcome/welcome-screen";

vi.mock("./diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

// Each test boots the whole window in jsdom: allow for slow CI machines.
vi.setConfig({ testTimeout: 40_000 });
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
const dialog = (name: string) => screen.findByRole("dialog", { name, hidden: true });
const lastCall = (name: string) => mockCalls.filter((c) => c.name === name).at(-1);

test("checkout: URL is validated, the folder comes from a native pick as a grant, never a path", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await userEvent.keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  const submit = within(dlg).getByRole("button", { name: "Checkout" }) as HTMLButtonElement;
  expect(submit.disabled).toBe(true);

  await userEvent.type(within(dlg).getByRole("textbox", { name: "Repository URL" }), "ftp://nope");
  expect(within(dlg).getByRole("alert").textContent).toContain("http(s), svn or file URL");
  await userEvent.clear(within(dlg).getByRole("textbox", { name: "Repository URL" }));
  await userEvent.type(
    within(dlg).getByRole("textbox", { name: "Repository URL" }),
    "https://svn.example.com/repos/game/branches/feature-x",
  );
  expect(submit.disabled).toBe(true); // still no destination

  await userEvent.click(within(dlg).getByRole("button", { name: "Choose folder…" }));
  await waitFor(() => expect(submit.disabled).toBe(false));
  await userEvent.selectOptions(within(dlg).getByRole("combobox", { name: "Scope" }), "immediates");
  await userEvent.click(submit);

  await waitFor(() => expect(lastCall("checkout")).toBeTruthy());
  const req = lastCall("checkout")?.args[0] as Record<string, unknown>;
  expect(String(req.dest)).toMatch(/^grant-/);
  expect(req.depth).toBe("immediates");
  expect(req.format).toBe(31);
  expect(req).not.toHaveProperty("path");
  expect(await screen.findByText("Checkout started")).toBeTruthy();
  // The finished checkout registers a new working copy.
  await waitFor(async () => {
    const list = await mockAdapter.commands.wcList();
    expect(list.status === "ok" && list.data.length).toBe(3);
  });
});

test("checkout: a consumed grant is dropped and the user must choose the folder again", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await userEvent.keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  await userEvent.type(
    within(dlg).getByRole("textbox", { name: "Repository URL" }),
    "https://svn.example.com/repos/game/trunk",
  );
  await userEvent.click(within(dlg).getByRole("button", { name: "Choose folder…" }));
  const submit = within(dlg).getByRole("button", { name: "Checkout" }) as HTMLButtonElement;
  await waitFor(() => expect(submit.disabled).toBe(false));
  // Someone else uses up the single-use grant (the first grant after a reset is grant-1).
  await mockAdapter.commands.wcAddExisting("grant-1");
  await userEvent.click(submit);
  expect(
    await within(dlg).findByText("The folder choice expired. Choose the folder again."),
  ).toBeTruthy();
  expect(within(dlg).getByText("No folder chosen")).toBeTruthy();
  expect(submit.disabled).toBe(true);
});

test("welcome: adding an existing working copy goes through path_pick and the grant", async () => {
  renderApp(
    <WcSessionProvider wc={null} files={[]}>
      <WelcomeScreen />
    </WcSessionProvider>,
  );
  await userEvent.click(await screen.findByRole("button", { name: "Add existing working copy…" }));
  expect(await screen.findByText("Added working copy game-assets")).toBeTruthy();
  const add = lastCall("wcAddExisting");
  expect(String(add?.args[0])).toMatch(/^grant-/);
  expect(mockCalls.some((c) => c.name === "pathPick")).toBe(true);
});

test("fs://dropped: a dropped folder is added by its grant, a dropped file is refused", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  const picked = await mockAdapter.commands.pathPick({ kind: "folder", title: null });
  if (picked.status !== "ok" || !picked.data) throw new Error("pick failed");
  await act(async () => {
    mockEmit("fs://dropped", {
      grants: [picked.data, { id: "g-file", displayName: "notes.txt", kind: "file" }],
      position: { x: 1, y: 1 },
    });
  });
  expect(await screen.findByText("Added working copy game-assets")).toBeTruthy();
  expect(await screen.findByText("Only folders can be added as a working copy.")).toBeTruthy();
  expect(lastCall("wcAddExisting")?.args[0]).toBe(picked.data.id);
});

test("conflicts: banner opens the resolver; text conflict resolves with theirs, tree conflict explains", async () => {
  renderApp(<AppShell />);
  await userEvent.click(await screen.findByRole("button", { name: "Resolve…" }));
  const dlg = await dialog("Conflicts (2)");
  expect(within(dlg).getByText("Text conflict")).toBeTruthy();
  expect(await within(dlg).findByText(/<<<<<<< \.mine/)).toBeTruthy();
  const resolve = within(dlg).getByRole("button", { name: "Resolve" }) as HTMLButtonElement;
  expect(resolve.disabled).toBe(true);
  for (const label of ["Mine", "Theirs", "Base (common ancestor)", "Merged"]) {
    expect(
      within(dlg).queryByRole("radio", { name: new RegExp(`^${label.split(" ")[0]}`) }),
    ).toBeTruthy();
  }
  await userEvent.click(within(dlg).getByRole("radio", { name: /^Theirs/ }));
  await userEvent.click(resolve);
  await waitFor(() => expect(lastCall("resolve")).toBeTruthy());
  expect((lastCall("resolve")?.args[0] as { choice: string } | undefined)?.choice).toBe("theirs");
  // The remaining conflict is the directory with a tree conflict.
  await within(dlg).findByText("Tree conflict");
  expect(within(dlg).getByText(/changed the surroundings of this item/)).toBeTruthy();
  expect(within(dlg).getByRole("radio", { name: /^Keep my local state/ })).toBeTruthy();
  expect(within(dlg).getByRole("radio", { name: /^Take the incoming state/ })).toBeTruthy();
  expect(within(dlg).queryByRole("radio", { name: /^Base/ })).toBeNull();
});

test("lock: a lock held by someone else can only be stolen after an explicit confirmation", async () => {
  renderApp(<AppShell />);
  await userEvent.click(await option(/crate\.fbx/));
  await userEvent.keyboard("{Control>}l{/Control}");
  const dlg = await dialog("Lock to edit");
  expect(within(dlg).getByText("Locked by an")).toBeTruthy();
  await userEvent.click(within(dlg).getByRole("checkbox", { name: /Steal the lock held by an/ }));
  const steal = within(dlg).getByRole("button", { name: "Steal lock" }) as HTMLButtonElement;
  expect(steal.disabled).toBe(true);
  await userEvent.click(within(dlg).getByRole("checkbox", { name: /I understand/ }));
  expect(steal.disabled).toBe(false);
  await userEvent.click(steal);
  await waitFor(() => expect(lastCall("lock")).toBeTruthy());
  const req = lastCall("lock")?.args[0] as {
    steal: boolean;
    targets: { kind: string; paths: string[] };
  };
  expect(req.steal).toBe(true);
  expect(req.targets).toMatchObject({ kind: "wc", paths: ["assets/models/crate.fbx"] });
  // The file now shows our lock.
  await waitFor(() =>
    expect(
      within(screen.getByRole("option", { name: /crate\.fbx/ })).getByText("Locked by you"),
    ).toBeTruthy(),
  );
});

test("lock: without the steal option the server refusal is shown and the dialog stays open", async () => {
  renderApp(<AppShell />);
  await userEvent.click(await option(/crate\.fbx/));
  await userEvent.keyboard("{Control>}l{/Control}");
  const dlg = await dialog("Lock to edit");
  await userEvent.click(within(dlg).getByRole("button", { name: "Lock" }));
  expect((await within(dlg).findByRole("alert")).textContent).toContain("Locked by an");
  expect(screen.getByRole("dialog", { name: "Lock to edit", hidden: true })).toBeTruthy();
});

test("discard: confirmation names the consequence and reverts only the selection", async () => {
  renderApp(<AppShell />);
  await userEvent.click(await option(/renderer\.ts/));
  await userEvent.keyboard("{Control>}{Shift>}{Backspace}{/Shift}{/Control}");
  const dlg = await dialog("Discard changes");
  expect(within(dlg).getByText(/discard the changes of 1 file/)).toBeTruthy();
  await userEvent.click(within(dlg).getByRole("button", { name: "Discard" }));
  await waitFor(() => expect(lastCall("revert")).toBeTruthy());
  const req = lastCall("revert")?.args[0] as { paths: string[]; removeUnversioned: boolean };
  expect(req.paths).toEqual(["src/engine/renderer.ts"]);
  expect(req.removeUnversioned).toBe(false);
  await waitFor(() => expect(screen.queryByRole("option", { name: /renderer\.ts/ })).toBeNull());
});

test("ignore: file actions menu adds a rule for the unversioned file and hides it", async () => {
  renderApp(<AppShell />);
  await userEvent.click(await option(/notes\/todo\.txt/));
  await userEvent.click(await screen.findByRole("button", { name: "File actions" }));
  await userEvent.click(await screen.findByRole("menuitem", { name: "Ignore…" }));
  const dlg = await dialog("Ignore");
  expect(within(dlg).getByRole("radio", { name: /All \*\.txt files/ })).toBeTruthy();
  await userEvent.click(within(dlg).getByRole("button", { name: "Ignore" }));
  await waitFor(() => expect(lastCall("ignoreAdd")).toBeTruthy());
  expect(lastCall("ignoreAdd")?.args[0]).toMatchObject({
    pattern: "todo.txt",
    dir: "notes",
    scope: "svn:ignore",
  });
  await waitFor(() =>
    expect(screen.queryByRole("option", { name: /notes\/todo\.txt/ })).toBeNull(),
  );
});

test("rename: paths that leave the working copy are refused before any command runs", async () => {
  renderApp(<AppShell />);
  await userEvent.click(await option(/renderer\.ts/));
  await userEvent.click(await screen.findByRole("button", { name: "File actions" }));
  await userEvent.click(await screen.findByRole("menuitem", { name: "Rename / Move…" }));
  const dlg = await dialog("Rename or move");
  const input = within(dlg).getByRole("textbox", { name: /New path/ });
  await userEvent.clear(input);
  await userEvent.type(input, "../outside.ts");
  expect(within(dlg).getByRole("alert").textContent).toContain("inside the working copy");
  expect((within(dlg).getByRole("button", { name: "Rename" }) as HTMLButtonElement).disabled).toBe(
    true,
  );
  await userEvent.clear(input);
  await userEvent.type(input, "src/engine/renderer-v2.ts");
  await userEvent.click(within(dlg).getByRole("button", { name: "Rename" }));
  await waitFor(() => expect(lastCall("movePath")).toBeTruthy());
  expect(lastCall("movePath")?.args.slice(1)).toEqual([
    "src/engine/renderer.ts",
    "src/engine/renderer-v2.ts",
  ]);
});

test("changelist: files can be grouped, and the group header appears", async () => {
  renderApp(<AppShell />);
  await userEvent.click(await option(/renderer\.ts/));
  await userEvent.click(await screen.findByRole("button", { name: "File actions" }));
  await userEvent.click(await screen.findByRole("menuitem", { name: "Changelist…" }));
  const dlg = await dialog("Changelist");
  await userEvent.type(within(dlg).getByRole("combobox", { name: "Changelist name" }), "refactor");
  await userEvent.click(within(dlg).getByRole("button", { name: "Apply" }));
  expect(await screen.findByRole("option", { name: "Changelist: refactor (1)" })).toBeTruthy();
});

test("activity: a running operation can be cancelled and is reported as cancelled, not as an error", async () => {
  setMockOpDuration(5000);
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await userEvent.keyboard("{Control>}{Shift>}u{/Shift}{/Control}");
  const activity = await screen.findByRole("region", { name: "Activity" });
  await userEvent.click(await within(activity).findByRole("button", { name: "Cancel" }));
  await waitFor(() => expect(lastCall("opCancel")).toBeTruthy());
  expect(await screen.findByText("Cancel requested")).toBeTruthy();
  await waitFor(() =>
    expect(within(activity).queryByRole("button", { name: "Cancel" })).toBeNull(),
  );
  expect(screen.queryByText(/failed: Cancelled/)).toBeNull();
});

test("activity: a failed operation is toasted with the backend message", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await act(async () => {
    mockEmit("op://started", { opId: "x1", kind: "update", wcId: null, title: "Update" });
    mockEmit("op://failed", {
      opId: "x1",
      error: {
        code: "network",
        message: "Connection reset",
        detail: null,
        path: null,
        retryable: true,
        actions: ["retry"],
      },
    });
  });
  expect(await screen.findAllByText("Update failed: Connection reset")).toBeTruthy();
});
