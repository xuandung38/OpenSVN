import { configure, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, expect, test, vi } from "vitest";
import { mockCalls, setMockOpDuration } from "../../../ipc/mock/adapter";
import { MOCK_WC_ID, mockWorkingCopies } from "../../../ipc/mock/data";
import { MOCK_REPO_ROOT } from "../../../ipc/mock/remote-data";
import { resetWcMock } from "../../../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import { CheckoutDialog } from "../dialogs/checkout-dialog";
import { WcDialogHost } from "../dialogs/wc-dialog-host";
import { WcSessionProvider } from "../wc-session";
import { WelcomeScreen } from "../welcome/welcome-screen";
import { SparseDialog } from "./sparse-dialog";

vi.setConfig({ testTimeout: 120_000 });
const { user } = installFakeClock();
configure({ asyncUtilTimeout: 8_000 });

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});

beforeEach(() => {
  resetWcMock();
  mockCalls.length = 0;
  setMockOpDuration(20);
});

const lastCall = (name: string) => mockCalls.filter((c) => c.name === name).at(-1);
const mainWc = mockWorkingCopies.find((w) => w.id === MOCK_WC_ID) ?? mockWorkingCopies[0];

test("checkout: Choose folders sends a depth per folder (parents first), the root keeps its depth", async () => {
  renderApp(<CheckoutDialog onClose={() => undefined} />);
  const dlg = await screen.findByRole("dialog", { name: "Checkout repository", hidden: true });
  const choose = within(dlg).getByRole("button", { name: "Choose folders…" }) as HTMLButtonElement;
  expect(choose.disabled).toBe(true); // no URL yet
  await user().type(
    within(dlg).getByRole("combobox", { name: "Repository URL" }),
    `${MOCK_REPO_ROOT}/trunk`,
  );
  await user().selectOptions(within(dlg).getByRole("combobox", { name: "Scope" }), "empty");
  await user().click(choose);
  expect(choose.getAttribute("aria-expanded")).toBe("true");
  const tree = await within(dlg).findByRole("region", { name: "Folders to check out" });
  const src = await within(tree).findByRole("combobox", { name: "Depth of src" });
  // Below an empty root a folder is left out unless chosen.
  expect(within(tree).getAllByText("not included").length).toBe(3);
  await user().selectOptions(src, "infinity");
  // Open assets and choose one folder deep inside: assets comes in empty.
  await user().click(within(tree).getByRole("button", { name: "Show folders in assets" }));
  await user().selectOptions(
    await within(tree).findByRole("combobox", { name: "Depth of assets/textures" }),
    "files",
  );
  expect(within(dlg).getByText("2 folder choices")).toBeTruthy();
  await user().click(within(dlg).getByRole("button", { name: "Checkout" }));
  await waitFor(() => expect(lastCall("checkout")).toBeTruthy());
  const req = lastCall("checkout")?.args[0] as Record<string, unknown>;
  expect(req.depth).toBe("empty");
  expect(req.sparse).toEqual([
    { path: "assets/textures", depth: "files" },
    { path: "src", depth: "infinity" },
  ]);
});

test("checkout: choosing what the folder gets anyway, or another URL, leaves no choice", async () => {
  renderApp(<CheckoutDialog onClose={() => undefined} />);
  const dlg = await screen.findByRole("dialog", { name: "Checkout repository", hidden: true });
  const url = within(dlg).getByRole("combobox", { name: "Repository URL" });
  await user().type(url, `${MOCK_REPO_ROOT}/trunk`);
  await user().click(within(dlg).getByRole("button", { name: "Choose folders…" }));
  const src = await within(dlg).findByRole("combobox", { name: "Depth of src" });
  expect((src as HTMLSelectElement).value).toBe("");
  expect(within(src).getByRole("option", { name: "As above (Everything)" })).toBeTruthy();
  await user().selectOptions(src, "infinity");
  expect(within(dlg).queryByText(/folder choices/)).toBeNull();
  await user().selectOptions(src, "exclude");
  expect(within(dlg).getByText("1 folder choices")).toBeTruthy();
  await user().type(url, "/src");
  await waitFor(() => expect(within(dlg).queryByText(/folder choices/)).toBeNull());
  await user().click(within(dlg).getByRole("button", { name: "Checkout" }));
  await waitFor(() => expect(lastCall("checkout")).toBeTruthy());
  const req = lastCall("checkout")?.args[0] as Record<string, unknown>;
  expect(req.sparse).toBeNull();
});

test("Choose folders on a working copy starts from its depths and sends only the changes", async () => {
  renderApp(
    <WcSessionProvider wc={mainWc ?? null} files={[]}>
      <SparseDialog onClose={() => undefined} />
    </WcSessionProvider>,
  );
  const dlg = await screen.findByRole("dialog", { name: "Choose folders", hidden: true });
  const apply = within(dlg).getByRole("button", { name: "Apply" }) as HTMLButtonElement;
  const assets = await within(dlg).findByRole("combobox", { name: "Depth of assets" });
  // The working copy has assets at `immediates`, the rest at the root's `infinity`.
  expect((assets as HTMLSelectElement).value).toBe("immediates");
  expect(
    (within(dlg).getByRole("combobox", { name: "Depth of src" }) as HTMLSelectElement).value,
  ).toBe("");
  expect(within(dlg).getByText("Nothing changes yet.")).toBeTruthy();
  expect(apply.disabled).toBe(true);
  await user().selectOptions(
    within(dlg).getByRole("combobox", { name: "Depth of notes" }),
    "exclude",
  );
  await user().selectOptions(assets, "");
  expect(within(dlg).getByText("2 folder changes, applied from the top down.")).toBeTruthy();
  expect(apply.disabled).toBe(false);
  await user().click(apply);
  await waitFor(() => expect(lastCall("wcSparseApply")).toBeTruthy());
  expect(lastCall("wcSparseApply")?.args).toEqual([
    MOCK_WC_ID,
    [
      { path: "assets", depth: "infinity" },
      { path: "notes", depth: "exclude" },
    ],
  ]);
});

test("menu://wc/sparse opens Choose folders for the open working copy", async () => {
  const { AppShell } = await import("../../../app/app-shell");
  const { mockEmit } = await import("../../../ipc/mock/adapter");
  renderApp(<AppShell />);
  await screen.findByRole("option", { name: /renderer\.ts/ });
  mockEmit("menu://wc/sparse");
  expect(await screen.findByRole("dialog", { name: "Choose folders", hidden: true })).toBeTruthy();
  await waitFor(() => expect(lastCall("wcDepths")?.args[0]).toBe(MOCK_WC_ID));
});

test("welcome: recent repositories start a checkout of that URL and can be removed", async () => {
  renderApp(
    <WcSessionProvider wc={null} files={[]}>
      <WelcomeScreen />
      <WcDialogHost />
    </WcSessionProvider>,
  );
  const list = await screen.findByRole("region", { name: "Recent repositories" });
  const items = within(list).getAllByRole("listitem");
  expect(items.map((i) => i.textContent)).toEqual([
    "https://svn.example.com/repos/game/trunk",
    "https://svn.example.com/repos/docs",
  ]);
  await user().click(
    within(list).getByRole("button", {
      name: "Remove https://svn.example.com/repos/docs from recent repositories",
    }),
  );
  await waitFor(() => expect(within(list).getAllByRole("listitem").length).toBe(1));
  expect(lastCall("recentUrlRemove")?.args[0]).toBe("https://svn.example.com/repos/docs");
  await user().click(
    within(list).getByRole("button", {
      name: "Check out https://svn.example.com/repos/game/trunk",
    }),
  );
  const dlg = await screen.findByRole("dialog", { name: "Checkout repository", hidden: true });
  expect(
    (within(dlg).getByRole("combobox", { name: "Repository URL" }) as HTMLInputElement).value,
  ).toBe("https://svn.example.com/repos/game/trunk");
});
