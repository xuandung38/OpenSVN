import { act, configure, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, expect, test, vi } from "vitest";
import { AppShell } from "../../../app/app-shell";
import {
  mockCalls,
  mockEmit,
  resetMockLayouts,
  setMockOpDuration,
} from "../../../ipc/mock/adapter";
import { MOCK_WC_ID } from "../../../ipc/mock/data";
import { resetWcMock, setMockWcBranch } from "../../../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../../test-utils";

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
  resetMockLayouts();
  mockCalls.length = 0;
  setMockOpDuration(40);
});

const emit = (name: string, payload?: unknown) => act(async () => mockEmit(name, payload));
const dialog = (name: string | RegExp) => screen.findByRole("dialog", { name, hidden: true });
const calls = (name: string) => mockCalls.filter((c) => c.name === name);
const ready = () => screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });

test("properties (menu): the root's properties can be added, edited and deleted, values checked as svn does", async () => {
  renderApp(<AppShell />);
  await ready();
  await emit("menu://wc/properties");
  const dlg = await dialog("Properties of the working copy root");
  const table = await within(dlg).findByRole("table", { name: "Properties", hidden: true });
  expect(within(table).getByRole("rowheader", { name: "svn:ignore", hidden: true })).toBeTruthy();
  expect(calls("propsGet").at(-1)?.args).toEqual([MOCK_WC_ID, [""]]);

  // A file-only property is refused on a folder before anything is sent.
  await user().click(within(dlg).getByRole("button", { name: "Add property", hidden: true }));
  const form = within(dlg).getByRole("form", { name: "New property", hidden: true });
  const name = within(form).getByRole("combobox", { name: "Name", hidden: true });
  await user().type(name, "svn:eol-style");
  expect(within(form).getByRole("alert").textContent).toBe("svn:eol-style applies to files only.");
  const save = within(form).getByRole("button", {
    name: "Save",
    hidden: true,
  }) as HTMLButtonElement;
  expect(save.disabled).toBe(true);
  await user().clear(name);
  await user().type(name, "svn:externals");
  const value = within(form).getByRole("textbox", { name: "Value", hidden: true });
  await user().type(value, "a b c d e");
  expect(within(form).getByRole("alert").textContent).toContain("svn:externals line is not valid");
  expect(save.disabled).toBe(true);
  await user().clear(value);
  await user().type(value, "^/libs/engine engine");
  await waitFor(() => expect(save.disabled).toBe(false));
  await user().click(save);
  await waitFor(() =>
    expect(calls("propsSet").at(-1)?.args).toEqual([
      MOCK_WC_ID,
      "",
      "svn:externals",
      "^/libs/engine engine",
    ]),
  );
  expect(
    await within(dlg).findByRole("rowheader", { name: "svn:externals", hidden: true }),
  ).toBeTruthy();

  await user().click(within(dlg).getByRole("button", { name: "Edit svn:ignore", hidden: true }));
  const edit = within(dlg).getByRole("form", { name: "Edit property", hidden: true });
  const editValue = within(edit).getByRole("textbox", { name: "Value", hidden: true });
  expect((editValue as HTMLTextAreaElement).value).toBe("*.log\nbuild\n");
  await user().type(editValue, "*.tmp");
  await user().click(within(edit).getByRole("button", { name: "Save", hidden: true }));
  await waitFor(() =>
    expect(calls("propsSet").at(-1)?.args).toEqual([
      MOCK_WC_ID,
      "",
      "svn:ignore",
      "*.log\nbuild\n*.tmp",
    ]),
  );

  await user().click(
    await within(dlg).findByRole("button", { name: "Delete svn:externals", hidden: true }),
  );
  await waitFor(() =>
    expect(calls("propsSet").at(-1)?.args).toEqual([MOCK_WC_ID, "", "svn:externals", null]),
  );
  await waitFor(() =>
    expect(
      within(dlg).queryByRole("rowheader", { name: "svn:externals", hidden: true }),
    ).toBeNull(),
  );
});

test("properties (selection menu): boolean properties, and no line endings on a binary file", async () => {
  const { mockAdapter } = await import("../../../ipc/mock/adapter");
  await mockAdapter.commands.propsSet(
    MOCK_WC_ID,
    "src/engine/renderer.ts",
    "svn:mime-type",
    "application/octet-stream",
  );
  renderApp(<AppShell />);
  await user().click(await ready());
  await emit("menu://sel/properties");
  const dlg = await dialog("Properties of src/engine/renderer.ts");
  expect(
    await within(dlg).findByRole("rowheader", { name: "svn:mime-type", hidden: true }),
  ).toBeTruthy();
  // svn:executable is boolean: no value field, stored as *.
  await user().click(within(dlg).getByRole("button", { name: "Add property", hidden: true }));
  const form = within(dlg).getByRole("form", { name: "New property", hidden: true });
  await user().type(
    within(form).getByRole("combobox", { name: "Name", hidden: true }),
    "svn:executable",
  );
  expect(within(form).queryByRole("textbox", { name: "Value", hidden: true })).toBeNull();
  expect(within(form).getByText(/Stored as \*/)).toBeTruthy();
  await user().click(within(form).getByRole("button", { name: "Save", hidden: true }));
  await waitFor(() =>
    expect(calls("propsSet").at(-1)?.args).toEqual([
      MOCK_WC_ID,
      "src/engine/renderer.ts",
      "svn:executable",
      "*",
    ]),
  );
  // Line endings on a file with a binary mime-type are refused while typing.
  await user().click(
    await within(dlg).findByRole("button", { name: "Add property", hidden: true }),
  );
  const again = within(dlg).getByRole("form", { name: "New property", hidden: true });
  await user().type(
    within(again).getByRole("combobox", { name: "Name", hidden: true }),
    "svn:eol-style",
  );
  // A list of the four styles is offered.
  await user().type(within(again).getByRole("combobox", { name: "Value", hidden: true }), "LF");
  expect(within(again).getByRole("alert").textContent).toContain("binary svn:mime-type");
});

test("properties: different values across items show as such; saving sets every item", async () => {
  const { WcSessionProvider } = await import("../wc-session");
  const { PropertiesDialog } = await import("./properties-dialog");
  const { mockWorkingCopies } = await import("../../../ipc/mock/data");
  const { mockAdapter } = await import("../../../ipc/mock/adapter");
  await mockAdapter.commands.propsSet(MOCK_WC_ID, "src/engine/renderer.ts", "owner", "ana");
  await mockAdapter.commands.propsSet(MOCK_WC_ID, "data/big-table.csv", "owner", "binh");
  const wc = mockWorkingCopies.find((w) => w.id === MOCK_WC_ID) ?? null;
  renderApp(
    <WcSessionProvider wc={wc} files={[]}>
      <PropertiesDialog
        paths={["src/engine/renderer.ts", "data/big-table.csv"]}
        onClose={() => {}}
      />
    </WcSessionProvider>,
  );
  const dlg = await dialog("Properties of 2 items");
  expect(await within(dlg).findByText("(different values)")).toBeTruthy();
  // Saving sets the value on every item.
  await user().click(within(dlg).getByRole("button", { name: "Edit owner", hidden: true }));
  const edit = within(dlg).getByRole("form", { name: "Edit property", hidden: true });
  await user().type(within(edit).getByRole("textbox", { name: "Value", hidden: true }), "cuong");
  await user().click(within(edit).getByRole("button", { name: "Save", hidden: true }));
  await waitFor(() =>
    expect(
      calls("propsSet")
        .filter((c) => c.args[3] === "cuong")
        .map((c) => c.args[1]),
    ).toEqual(["src/engine/renderer.ts", "data/big-table.csv"]),
  );
  expect(await within(dlg).findByText("cuong")).toBeTruthy();
});

test("layout (menu): custom folders are saved for the repository, detection can fill them", async () => {
  renderApp(<AppShell />);
  await ready();
  await emit("menu://branch/layout");
  const dlg = await dialog("Repository layout");
  const auto = within(dlg).getByRole("radio", {
    name: "Detect trunk, branches and tags automatically",
    hidden: true,
  }) as HTMLInputElement;
  await waitFor(() => expect(auto.checked).toBe(true));
  await user().click(within(dlg).getByRole("radio", { name: "Custom folders", hidden: true }));
  const trunk = within(dlg).getByRole("textbox", { name: "Trunk folder", hidden: true });
  await user().clear(trunk);
  const save = within(dlg).getByRole("button", { name: "Save", hidden: true }) as HTMLButtonElement;
  expect(within(dlg).getByRole("alert").textContent).toBe("Enter the trunk folder.");
  expect(save.disabled).toBe(true);
  await user().type(trunk, "main/");
  const branches = within(dlg).getByRole("textbox", {
    name: "Branch folders (one per line)",
    hidden: true,
  });
  await user().clear(branches);
  await user().type(branches, "dev{Enter}/{{project}/features");
  await user().click(save);
  await waitFor(() => expect(calls("layoutSet").length).toBe(1));
  expect(calls("layoutSet")[0]?.args).toEqual([
    "00000000-0000-0000-0000-000000000001",
    { mode: "custom", trunk: "/main", branches: ["/dev", "/{project}/features"], tags: ["/tags"] },
  ]);
  expect(await screen.findByText("Repository layout saved")).toBeTruthy();

  // Reopened: the stored layout is shown; "Use detected" puts back what detection found.
  await emit("menu://wc/layout");
  const again = await dialog("Repository layout");
  const trunk2 = within(again).getByRole("textbox", {
    name: "Trunk folder",
    hidden: true,
  }) as HTMLInputElement;
  await waitFor(() => expect(trunk2.value).toBe("/main"));
  await user().click(
    await within(again).findByRole("button", { name: "Use detected", hidden: true }),
  );
  expect(trunk2.value).toBe("/trunk");
});

test("wc-busy banner: another program's lock; cleanup removes it only after the confirmation tick", async () => {
  renderApp(<AppShell />);
  await ready();
  await emit("wc://banner", { wcId: MOCK_WC_ID, kind: "wc-busy", data: [""] });
  const banner = await screen.findByText("Working copy in use by another program");
  expect(screen.getByText(/holds its lock \(whole working copy\)/)).toBeTruthy();
  await user().click(screen.getByRole("button", { name: "Cleanup…" }));
  const dlg = await dialog("Clean up");
  expect(within(dlg).getByText(/Another program holds this working copy's lock/)).toBeTruthy();
  const submit = within(dlg).getByRole("button", {
    name: "Clean up",
    hidden: true,
  }) as HTMLButtonElement;
  await user().click(
    within(dlg).getByRole("checkbox", { name: "Remove another program's lock", hidden: true }),
  );
  expect(submit.disabled).toBe(true);
  const confirm = within(dlg).getByRole("checkbox", {
    name: "I closed every other program that uses this working copy",
    hidden: true,
  });
  await user().click(confirm);
  expect(submit.disabled).toBe(false);
  await user().click(submit);
  await waitFor(() => expect(calls("cleanup").length).toBe(1));
  expect(calls("cleanup")[0]?.args[0]).toEqual({
    wcId: MOCK_WC_ID,
    removeUnversioned: false,
    removeIgnored: false,
    vacuumPristines: false,
    breakForeignLocks: true,
  });
  // The finished cleanup clears the banner.
  await waitFor(() => expect(banner.isConnected).toBe(false));
});

test("cleanup: without the confirmation the lock option is never sent", async () => {
  renderApp(<AppShell />);
  await ready();
  await emit("menu://wc/cleanup");
  const dlg = await dialog("Clean up");
  const box = within(dlg).getByRole("checkbox", {
    name: "Remove another program's lock",
    hidden: true,
  });
  await user().click(box);
  await user().click(box);
  await user().click(within(dlg).getByRole("button", { name: "Clean up", hidden: true }));
  await waitFor(() => expect(calls("cleanup").length).toBe(1));
  expect(calls("cleanup")[0]?.args[0]).toMatchObject({ breakForeignLocks: false });
});

test("banners: a commit refused as out of date, and a working copy on a tag", async () => {
  setMockWcBranch(MOCK_WC_ID, {
    kind: "tag",
    name: "v1.2",
    url: "https://svn.example.com/repos/game/tags/v1.2",
  });
  renderApp(<AppShell />);
  await ready();
  expect(await screen.findByText("On a tag")).toBeTruthy();
  expect(screen.getByText(/v1\.2 is a tag/)).toBeTruthy();

  await emit("op://started", { opId: "c1", kind: "commit", wcId: MOCK_WC_ID, title: "Commit" });
  await emit("op://failed", {
    opId: "c1",
    error: {
      code: "out-of-date",
      message: "out of date",
      detail: null,
      path: null,
      retryable: false,
      actions: ["update"],
    },
  });
  expect(await screen.findByText("Out of date")).toBeTruthy();
  const strip = screen.getByText("Out of date").closest("[role=status]") as HTMLElement;
  await user().click(within(strip).getByRole("button", { name: "Update" }));
  await waitFor(() => expect(calls("update").length).toBe(1));
  // The update that follows ends the out-of-date state.
  await waitFor(() => expect(screen.queryByText("Out of date")).toBeNull());

  const tag = screen.getByText("On a tag").closest("[role=status]") as HTMLElement;
  await user().click(within(tag).getByRole("button", { name: "Dismiss" }));
  await waitFor(() => expect(screen.queryByText("On a tag")).toBeNull());
});

test("an operation refused because another program holds the lock shows the wc-busy banner", async () => {
  renderApp(<AppShell />);
  await ready();
  await emit("op://started", { opId: "u1", kind: "update", wcId: MOCK_WC_ID, title: "Update" });
  await emit("op://failed", {
    opId: "u1",
    error: {
      code: "wc-busy",
      message: "locked by another program",
      detail: null,
      path: null,
      retryable: true,
      actions: null,
    },
  });
  expect(await screen.findByText("Working copy in use by another program")).toBeTruthy();
});
