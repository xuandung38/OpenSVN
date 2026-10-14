import { act, configure, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, expect, test, vi } from "vitest";
import { AppShell } from "../../app/app-shell";
import {
  mockAdapter,
  mockCalls,
  mockEmit,
  setMockEngineCapabilities,
  setMockOpDuration,
  setMockProgressTotal,
} from "../../ipc/mock/adapter";
import { mockWorkingCopies } from "../../ipc/mock/data";
import { resetWcMock } from "../../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../test-utils";
import { WcLockDialog } from "./dialogs/lock-dialogs";
import { WcSessionProvider } from "./wc-session";
import { WelcomeScreen } from "./welcome/welcome-screen";

vi.mock("./diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
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
  setMockProgressTotal(100);
  setMockEngineCapabilities({ externals: true, demo: false });
});

const option = (name: RegExp) => screen.findByRole("option", { name });
const dialog = (name: string) => screen.findByRole("dialog", { name, hidden: true });
const lastCall = (name: string) => mockCalls.filter((c) => c.name === name).at(-1);

test("checkout: name follows the URL, parent is the default folder granted by Rust, new WC opens", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  const submit = within(dlg).getByRole("button", { name: "Checkout" }) as HTMLButtonElement;
  expect(submit.disabled).toBe(true);
  // Recent URLs are offered as suggestions.
  await waitFor(() => expect(dlg.querySelectorAll("datalist option").length).toBe(2));

  const urlBox = within(dlg).getByRole("combobox", { name: "Repository URL" });
  await user().type(urlBox, "ftp://nope");
  expect(within(dlg).getByRole("alert").textContent).toContain("http(s), svn or file URL");
  await user().clear(urlBox);
  await user().type(urlBox, "https://svn.example.com/repos/game/branches/feature-x");
  const name = within(dlg).getByRole("textbox", { name: "Folder name" }) as HTMLInputElement;
  expect(name.value).toBe("feature-x");
  expect(within(dlg).getByText("Creates /Users/dev/work/feature-x")).toBeTruthy();
  expect(within(dlg).getByText("/Users/dev/work (default)")).toBeTruthy();
  await user().selectOptions(within(dlg).getByRole("combobox", { name: "Scope" }), "immediates");
  await waitFor(() => expect(submit.disabled).toBe(false));
  await user().click(submit);

  await waitFor(() => expect(lastCall("checkout")).toBeTruthy());
  const req = lastCall("checkout")?.args[0] as Record<string, unknown>;
  expect(String(req.dest)).toMatch(/^grant-root-/);
  expect(req.destName).toBe("feature-x");
  expect(req.depth).toBe("immediates");
  expect(req.format).toBe(31);
  expect(req.storePristine).toBe(true);
  expect(req.ignoreExternals).toBe(false);
  expect(req).not.toHaveProperty("path");
  expect(mockCalls.some((c) => c.name === "checkoutRootGrant")).toBe(true);
  // The finished checkout is registered, opened and becomes the current repository.
  expect(await screen.findByText("Checked out feature-x")).toBeTruthy();
  await waitFor(() => expect(lastCall("wcOpen")?.args[0]).toMatch(/^wc-checkout-/));
  expect(
    await screen.findByRole("button", { name: /Current repository\s*feature-x/ }),
  ).toBeTruthy();
  expect(screen.queryByRole("dialog", { name: "Checkout repository", hidden: true })).toBeNull();
});

test("checkout: a picked parent is single use; a refused target asks to choose it again", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  await user().type(
    within(dlg).getByRole("combobox", { name: "Repository URL" }),
    "https://svn.example.com/repos/game/trunk",
  );
  const name = within(dlg).getByRole("textbox", { name: "Folder name" }) as HTMLInputElement;
  expect(name.value).toBe("game");
  const submit = within(dlg).getByRole("button", { name: "Checkout" }) as HTMLButtonElement;

  // Names the backend would refuse are caught early.
  for (const [bad, message] of [
    ["CON", "This name is reserved by Windows."],
    ["../up", "The name must not contain / or \\."],
    ["dot.", "The name must not start with a space or end with a dot or space."],
  ]) {
    await user().clear(name);
    await user().type(name, bad as string);
    expect(within(dlg).getByRole("alert").textContent).toBe(message);
    expect(submit.disabled).toBe(true);
  }

  // A folder that already exists with content is refused by the backend.
  await user().clear(name);
  await user().type(name, "game-client");
  await user().click(within(dlg).getByRole("button", { name: "Change…" }));
  expect(await within(dlg).findByText("game-assets")).toBeTruthy();
  expect(within(dlg).getByText("Creates game-assets/game-client")).toBeTruthy();
  await waitFor(() => expect(submit.disabled).toBe(false));
  await user().click(submit);
  expect(await within(dlg).findByText(/already exists and is not empty/)).toBeTruthy();
  expect(within(dlg).getByText("Choose the parent folder again.")).toBeTruthy();
  expect(within(dlg).getByText("No folder chosen")).toBeTruthy();
  expect(submit.disabled).toBe(true);

  await user().click(within(dlg).getByRole("button", { name: "Change…" }));
  await user().clear(name);
  await user().type(name, "game-2");
  await waitFor(() => expect(submit.disabled).toBe(false));
});

test("checkout: a folder name the backend refuses keeps the picked parent", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  await user().type(
    within(dlg).getByRole("combobox", { name: "Repository URL" }),
    "https://svn.example.com/repos/game/trunk",
  );
  const name = within(dlg).getByRole("textbox", { name: "Folder name" }) as HTMLInputElement;
  const submit = within(dlg).getByRole("button", { name: "Checkout" }) as HTMLButtonElement;
  await user().click(within(dlg).getByRole("button", { name: "Change…" }));
  expect(await within(dlg).findByText("game-assets")).toBeTruthy();

  // Refused before the grant is redeemed: the parent stays chosen, no re-pick needed.
  await user().clear(name);
  await user().type(name, "refused-by-backend");
  await waitFor(() => expect(submit.disabled).toBe(false));
  await user().click(submit);
  expect(await within(dlg).findByText(/Invalid folder name/)).toBeTruthy();
  expect(within(dlg).queryByText("Choose the parent folder again.")).toBeNull();
  expect(within(dlg).getByText("game-assets")).toBeTruthy();
  const picks = mockCalls.filter((c) => c.name === "pathPick").length;

  await user().clear(name);
  await user().type(name, "game-3");
  await waitFor(() => expect(submit.disabled).toBe(false));
  await user().click(submit);
  await waitFor(() =>
    expect((lastCall("checkout")?.args[0] as Record<string, unknown> | undefined)?.destName).toBe(
      "game-3",
    ),
  );
  expect(mockCalls.filter((c) => c.name === "pathPick").length).toBe(picks);
  await waitFor(() =>
    expect(screen.queryByRole("dialog", { name: "Checkout repository", hidden: true })).toBeNull(),
  );
});

test("checkout: progress is shown with Cancel; cancelling returns to the form", async () => {
  setMockOpDuration(60_000);
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  await user().type(
    within(dlg).getByRole("combobox", { name: "Repository URL" }),
    "https://svn.example.com/repos/game/trunk",
  );
  await user().click(within(dlg).getByRole("button", { name: "Checkout" }));
  const cancel = await within(dlg).findByRole("button", { name: "Cancel checkout" });
  expect(within(dlg).getByRole("progressbar", { name: "Checking out…" })).toBeTruthy();
  expect(within(dlg).getByRole("button", { name: "Continue in background" })).toBeTruthy();
  await user().click(cancel);
  expect(await within(dlg).findByText(/^The checkout was cancelled\./)).toBeTruthy();
  expect(lastCall("opCancel")).toBeTruthy();
  expect(within(dlg).getByRole("combobox", { name: "Repository URL" })).toBeTruthy();
});

test("checkout: an engine that fetches externals offers to ignore them", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  const box = (await within(dlg).findByRole("checkbox", {
    name: "Ignore externals",
  })) as HTMLInputElement;
  expect(box.checked).toBe(false);
  expect(within(dlg).queryByText(/Externals are not fetched yet/)).toBeNull();
  await user().click(box);
  await user().type(
    within(dlg).getByRole("combobox", { name: "Repository URL" }),
    "https://svn.example.com/repos/game/trunk",
  );
  const submit = within(dlg).getByRole("button", { name: "Checkout" }) as HTMLButtonElement;
  await waitFor(() => expect(submit.disabled).toBe(false));
  await user().click(submit);
  await waitFor(() => expect(lastCall("checkout")).toBeTruthy());
  const req = lastCall("checkout")?.args[0] as Record<string, unknown>;
  expect(req.ignoreExternals).toBe(true);
});

test("checkout: an engine without externals says they are not fetched and links the issue", async () => {
  setMockEngineCapabilities({ externals: false, demo: false });
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  expect(await within(dlg).findByText(/Externals are not fetched yet/)).toBeTruthy();
  const link = within(dlg).getByRole("link", { name: "Tracking issue" });
  expect(link.getAttribute("href")).toBe("https://github.com/xuandung38/OpenSVN/issues/97");
  expect(within(dlg).queryByRole("checkbox")).toBeNull();
});

test("checkout: progress without a total is indeterminate and counts items", async () => {
  // First tick at 22.5% of the duration, within the find timeout.
  setMockOpDuration(20_000);
  setMockProgressTotal(null);
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  await user().type(
    within(dlg).getByRole("combobox", { name: "Repository URL" }),
    "https://svn.example.com/repos/game/trunk",
  );
  await user().click(within(dlg).getByRole("button", { name: "Checkout" }));
  expect(await within(dlg).findByText(/^1 items/)).toBeTruthy();
  const bar = within(dlg).getByRole("progressbar", { name: "Checking out…" });
  expect(bar.hasAttribute("value")).toBe(false);
  expect(bar.hasAttribute("max")).toBe(false);
  await user().click(within(dlg).getByRole("button", { name: "Cancel checkout" }));
});

test("checkout: Browse… shows the repository folders and picking one sets the URL", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const dlg = await dialog("Checkout repository");
  const urlBox = within(dlg).getByRole("combobox", { name: "Repository URL" }) as HTMLInputElement;
  await user().type(urlBox, "https://svn.example.com/repos/game/trunk");
  await user().click(within(dlg).getByRole("button", { name: "Browse…" }));
  const tree = await within(dlg).findByRole("tree", { name: "Repository folders" });
  await user().click(await within(tree).findByRole("treeitem", { name: /branches/ }));
  expect(urlBox.value).toBe("https://svn.example.com/repos/game/branches");
  expect(
    (within(dlg).getByRole("textbox", { name: "Folder name" }) as HTMLInputElement).value,
  ).toBe("branches");
});

test("welcome: adding an existing working copy goes through path_pick and the grant", async () => {
  renderApp(
    <WcSessionProvider wc={null} files={[]}>
      <WelcomeScreen />
    </WcSessionProvider>,
  );
  await user().click(await screen.findByRole("button", { name: "Add existing working copy…" }));
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
  await user().click(await screen.findByRole("button", { name: "Resolve…" }));
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
  await user().click(within(dlg).getByRole("radio", { name: /^Theirs/ }));
  await user().click(resolve);
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
  await user().click(await option(/crate\.fbx/));
  await user().keyboard("{Control>}l{/Control}");
  const dlg = await dialog("Lock to edit");
  expect(within(dlg).getByText(/assets\/models\/crate\.fbx: locked by an/)).toBeTruthy();
  expect(within(dlg).getByText("Their comment: rework")).toBeTruthy();
  await user().click(within(dlg).getByRole("checkbox", { name: /Steal the lock held by an/ }));
  const steal = within(dlg).getByRole("button", { name: "Steal lock" }) as HTMLButtonElement;
  expect(steal.disabled).toBe(true);
  await user().click(within(dlg).getByRole("checkbox", { name: /I understand/ }));
  expect(steal.disabled).toBe(false);
  await user().click(steal);
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
  await user().click(await option(/crate\.fbx/));
  await user().keyboard("{Control>}l{/Control}");
  const dlg = await dialog("Lock to edit");
  await user().click(within(dlg).getByRole("button", { name: "Lock" }));
  expect((await within(dlg).findByRole("alert")).textContent).toContain("locked by an");
  expect(screen.getByRole("dialog", { name: "Lock to edit", hidden: true })).toBeTruthy();
  expect(lastCall("lock")?.args[0]).toMatchObject({ steal: false });
});

test("lock held elsewhere (#127): the refusal names owner, comment and date; Steal needs two confirmations", async () => {
  renderApp(<AppShell />);
  // Unmodified needs-lock file: listed under Locks, its foreign lock unknown until the server says.
  const locks = await screen.findByRole("region", { name: /^Locks \(/ });
  const row = within(locks).getByRole("listitem", { name: "assets/audio/theme.ogg" });
  await user().click(within(row).getByRole("button", { name: "Lock to edit…" }));
  const dlg = await dialog("Lock to edit");
  expect(within(dlg).queryByRole("checkbox", { name: /Steal/ })).toBeNull();
  await user().click(within(dlg).getByRole("button", { name: "Lock" }));
  // The dialog stays open through the operation and shows who holds the lock.
  expect(await within(dlg).findByText(/assets\/audio\/theme\.ogg: locked by binh/)).toBeTruthy();
  expect(within(dlg).getByText("Their comment: mixing the final cut")).toBeTruthy();
  expect(within(dlg).getByText(/Ask binh to release the lock/)).toBeTruthy();
  expect(within(dlg).getByRole("alert").textContent).toContain("HTTP 423");
  expect(lastCall("lock")?.args[0]).toMatchObject({ steal: false });

  await user().click(within(dlg).getByRole("checkbox", { name: /Steal the lock held by binh/ }));
  const steal = within(dlg).getByRole("button", { name: "Steal lock" }) as HTMLButtonElement;
  expect(steal.disabled).toBe(true);
  expect(within(dlg).getByText(/takes the lock away from binh/)).toBeTruthy();
  await user().click(within(dlg).getByRole("checkbox", { name: /I understand/ }));
  expect(steal.disabled).toBe(false);
  await user().click(steal);
  await waitFor(() =>
    expect(screen.queryByRole("dialog", { name: "Lock to edit", hidden: true })).toBeNull(),
  );
  expect(lastCall("lock")?.args[0]).toMatchObject({
    steal: true,
    targets: { kind: "wc", paths: ["assets/audio/theme.ogg"] },
  });
  await waitFor(() =>
    expect(
      within(
        within(screen.getByRole("region", { name: /^Locks \(/ })).getByRole("listitem", {
          name: "assets/audio/theme.ogg",
        }),
      ).getByText("Locked by you"),
    ).toBeTruthy(),
  );
});

test("lock held elsewhere (#127): a refusal after the dialog was closed reopens it with the owner", async () => {
  renderApp(<AppShell />);
  const locks = await screen.findByRole("region", { name: /^Locks \(/ });
  const row = within(locks).getByRole("listitem", { name: "assets/audio/theme.ogg" });
  await user().click(within(row).getByRole("button", { name: "Lock to edit…" }));
  const dlg = await dialog("Lock to edit");
  await user().click(within(dlg).getByRole("button", { name: "Lock" }));
  await waitFor(() => expect(lastCall("lock")).toBeTruthy());
  await user().click(within(dlg).getByRole("button", { name: "Cancel" }));
  const again = await dialog("Lock to edit");
  expect(await within(again).findByText(/theme\.ogg: locked by binh/)).toBeTruthy();
  expect(within(again).getByRole("checkbox", { name: /Steal the lock held by binh/ })).toBeTruthy();
  // Nothing was stolen: only the one plain lock request went out.
  expect(mockCalls.filter((c) => c.name === "lock")).toHaveLength(1);
});

test("lock held elsewhere (#127): with several owners, the steal box and warning name them all", async () => {
  renderApp(
    <WcSessionProvider wc={mockWorkingCopies[0] ?? null} files={[]}>
      <WcLockDialog
        mode="lock"
        paths={["a.psd", "b.psd", "c.psd"]}
        conflicts={[
          { path: "a.psd", owner: "an", comment: null, createdAt: null },
          { path: "b.psd", owner: "binh", comment: null, createdAt: null },
          { path: "c.psd", owner: "an", comment: null, createdAt: null },
        ]}
        onClose={() => {}}
      />
    </WcSessionProvider>,
  );
  const dlg = await dialog("Lock to edit");
  expect(within(dlg).getByText("b.psd: Locked by binh")).toBeTruthy();
  await user().click(
    within(dlg).getByRole("checkbox", { name: "Steal the lock held by an, binh" }),
  );
  expect(within(dlg).getByText(/takes the lock away from an, binh\./)).toBeTruthy();
});

test("locks section (#128): unmodified needs-lock and locked files offer Lock / Unlock, never a commit tick", async () => {
  renderApp(<AppShell />);
  const locks = await screen.findByRole("region", { name: "Locks (2)" });
  const theme = within(locks).getByRole("listitem", { name: "assets/audio/theme.ogg" });
  expect(within(theme).getByText("Needs lock")).toBeTruthy();
  expect(within(theme).queryByRole("button", { name: "Unlock…" })).toBeNull();
  const spec = within(locks).getByRole("listitem", { name: "docs/spec.docx" });
  expect(within(spec).getByText("Locked by you")).toBeTruthy();
  expect(within(spec).queryByRole("button", { name: "Lock to edit…" })).toBeNull();
  // Not changes: not in the Changes list.
  expect(screen.queryByRole("option", { name: /theme\.ogg/ })).toBeNull();
  expect(screen.queryByRole("option", { name: /spec\.docx/ })).toBeNull();

  await user().click(within(spec).getByRole("button", { name: "Unlock…" }));
  const dlg = await dialog("Unlock");
  await user().click(within(dlg).getByRole("button", { name: "Unlock" }));
  await waitFor(() =>
    expect(screen.queryByRole("dialog", { name: "Unlock", hidden: true })).toBeNull(),
  );
  expect(lastCall("unlock")?.args[0]).toMatchObject({
    breakLock: false,
    targets: { kind: "wc", paths: ["docs/spec.docx"] },
  });
  // Still listed: it needs a lock again before the next edit.
  await waitFor(() =>
    expect(
      within(
        within(screen.getByRole("region", { name: "Locks (2)" })).getByRole("listitem", {
          name: "docs/spec.docx",
        }),
      ).getByText("Needs lock"),
    ).toBeTruthy(),
  );
});

test("discard: confirmation names the consequence and reverts only the selection", async () => {
  renderApp(<AppShell />);
  await user().click(await option(/renderer\.ts/));
  await user().keyboard("{Control>}{Shift>}{Backspace}{/Shift}{/Control}");
  const dlg = await dialog("Discard changes");
  expect(within(dlg).getByText(/discard the changes of 1 file/)).toBeTruthy();
  await user().click(within(dlg).getByRole("button", { name: "Discard" }));
  await waitFor(() => expect(lastCall("revert")).toBeTruthy());
  const req = lastCall("revert")?.args[0] as { paths: string[]; removeUnversioned: boolean };
  expect(req.paths).toEqual(["src/engine/renderer.ts"]);
  expect(req.removeUnversioned).toBe(false);
  await waitFor(() => expect(screen.queryByRole("option", { name: /renderer\.ts/ })).toBeNull());
});

test("ignore: file actions menu adds a rule for the unversioned file and hides it", async () => {
  renderApp(<AppShell />);
  await user().click(await option(/notes\/todo\.txt/));
  await user().click(await screen.findByRole("button", { name: "File actions" }));
  await user().click(await screen.findByRole("menuitem", { name: "Ignore…" }));
  const dlg = await dialog("Ignore");
  expect(within(dlg).getByRole("radio", { name: /All \*\.txt files/ })).toBeTruthy();
  await user().click(within(dlg).getByRole("button", { name: "Ignore" }));
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
  await user().click(await option(/renderer\.ts/));
  await user().click(await screen.findByRole("button", { name: "File actions" }));
  await user().click(await screen.findByRole("menuitem", { name: "Rename / Move…" }));
  const dlg = await dialog("Rename or move");
  const input = within(dlg).getByRole("textbox", { name: /New path/ });
  await user().clear(input);
  await user().type(input, "../outside.ts");
  expect(within(dlg).getByRole("alert").textContent).toContain("inside the working copy");
  expect((within(dlg).getByRole("button", { name: "Rename" }) as HTMLButtonElement).disabled).toBe(
    true,
  );
  await user().clear(input);
  await user().type(input, "src/engine/renderer-v2.ts");
  await user().click(within(dlg).getByRole("button", { name: "Rename" }));
  await waitFor(() => expect(lastCall("movePath")).toBeTruthy());
  expect(lastCall("movePath")?.args.slice(1)).toEqual([
    "src/engine/renderer.ts",
    "src/engine/renderer-v2.ts",
  ]);
});

test("changelist: files can be grouped, and the group header appears", async () => {
  renderApp(<AppShell />);
  await user().click(await option(/renderer\.ts/));
  await user().click(await screen.findByRole("button", { name: "File actions" }));
  await user().click(await screen.findByRole("menuitem", { name: "Changelist…" }));
  const dlg = await dialog("Changelist");
  await user().type(within(dlg).getByRole("combobox", { name: "Changelist name" }), "refactor");
  await user().click(within(dlg).getByRole("button", { name: "Apply" }));
  expect(await screen.findByRole("option", { name: "Changelist: refactor (1)" })).toBeTruthy();
});

test("activity: a running operation can be cancelled and is reported as cancelled, not as an error", async () => {
  setMockOpDuration(5000);
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await user().keyboard("{Control>}{Shift>}u{/Shift}{/Control}");
  const activity = await screen.findByRole("region", { name: "Activity" });
  await user().click(await within(activity).findByRole("button", { name: "Cancel" }));
  await waitFor(() => expect(lastCall("opCancel")).toBeTruthy());
  expect(await screen.findByText("Cancel requested")).toBeTruthy();
  await waitFor(() =>
    expect(within(activity).queryByRole("button", { name: "Cancel" })).toBeNull(),
  );
  expect(screen.queryByText(/failed: Cancelled/)).toBeNull();
});

test("activity: a failed operation is toasted with a short text for transport errors", async () => {
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
  expect(
    await screen.findAllByText("Update failed: Connection was closed by the server"),
  ).toBeTruthy();
});

test("demo engine: a release build running the demo engine says so, permanently", async () => {
  setMockEngineCapabilities({ externals: true, demo: true });
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  expect(await screen.findByText("Demo engine — not connected to real servers")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Dismiss" })).toBeNull();
});

test("demo engine: no banner for a real engine", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await waitFor(() => expect(lastCall("engineCapabilities")).toBeTruthy());
  expect(screen.queryByText("Demo engine — not connected to real servers")).toBeNull();
});

test("commit: a needs-cleanup notice shows the working copy's cleanup banner", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await act(async () => {
    mockEmit("op://started", { opId: "c1", kind: "commit", wcId: "wc-main", title: "Commit" });
    mockEmit("op://progress", {
      opId: "c1",
      phase: "needs-cleanup",
      current: null,
      total: null,
      bytesDone: null,
      bytesTotal: null,
      currentPath: "src/app.ts is in use",
    });
  });
  // While the commit itself still runs, its lock is ours: no cleanup offered yet.
  expect(await screen.findByText("Commit in progress…")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Run cleanup" })).toBeNull();
  await act(async () => {
    mockEmit("op://finished", { opId: "c1", result: { kind: "newRev", newRev: 9 } });
  });
  expect(await screen.findByRole("button", { name: "Run cleanup" })).toBeTruthy();
});

test("add: a skipped nested working copy is shown to the user, not only logged", async () => {
  renderApp(<AppShell />);
  await option(/renderer\.ts/);
  await act(async () => {
    mockEmit("op://started", { opId: "a1", kind: "add", wcId: "wc-main", title: "Add" });
    mockEmit("op://progress", {
      opId: "a1",
      phase: "nested-wc-skipped",
      current: null,
      total: null,
      bytesDone: null,
      bytesTotal: null,
      currentPath: "'vendor/lib' is a nested working copy",
    });
  });
  expect(
    await screen.findByText(
      "Skipped a nested working copy, which stays unversioned: 'vendor/lib' is a nested working copy",
    ),
  ).toBeTruthy();
});
