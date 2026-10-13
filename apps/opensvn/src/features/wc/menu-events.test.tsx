import { act, configure, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, expect, test, vi } from "vitest";
import { AppShell } from "../../app/app-shell";
import { MENU_EVENTS } from "../../ipc/events";
import { mockCalls, mockEmit, setMockOpDuration } from "../../ipc/mock/adapter";
import { MOCK_WC_ID } from "../../ipc/mock/data";
import { resetWcMock } from "../../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../test-utils";
import { MENU_ACTIONS } from "./use-wc-shortcuts";

vi.mock("./diff/diff-merge-editor", () => ({
  DiffMergeEditor: ({ mode }: { mode: string }) => <div data-testid="editor" data-mode={mode} />,
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
  mockCalls.length = 0;
  setMockOpDuration(40);
});

const emit = (name: string) => act(async () => mockEmit(name));
const mode = () => screen.getByTestId("editor").getAttribute("data-mode");

test("every subscribed menu event has an action and every action is subscribed", () => {
  expect(new Set(Object.keys(MENU_ACTIONS))).toEqual(new Set(MENU_EVENTS));
  expect(Object.keys(MENU_ACTIONS)).toHaveLength(MENU_EVENTS.length);
});

test("Edit > Find focuses the filter; View > Diff mode / Hide whitespace toggle once per key press", async () => {
  renderApp(<AppShell />);
  await user().click(await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ }));
  await screen.findByTestId("editor");
  expect(mode()).toBe("split");

  await emit("menu://edit.find");
  await waitFor(() => expect(document.activeElement?.hasAttribute("data-wc-filter")).toBe(true));
  (document.activeElement as HTMLElement).blur();

  await emit("menu://view.diff-mode");
  await waitFor(() => expect(mode()).toBe("unified"));

  // Key press plus the native accelerator's echo, in both orders: one toggle each time.
  const now = vi.spyOn(Date, "now").mockReturnValue(1_000_000);
  await user().keyboard("{Control>}{Alt>}d{/Alt}{/Control}");
  await emit("menu://view.diff-mode");
  await waitFor(() => expect(mode()).toBe("split"));
  now.mockReturnValue(2_000_000);
  await emit("menu://view.diff-mode");
  now.mockReturnValue(2_000_050);
  await user().keyboard("{Control>}{Alt>}d{/Alt}{/Control}");
  await waitFor(() => expect(mode()).toBe("unified"));
  // More than 400 ms apart: two separate requests.
  now.mockReturnValue(3_000_000);
  await user().keyboard("{Control>}{Alt>}d{/Alt}{/Control}");
  now.mockReturnValue(3_000_401);
  await emit("menu://view.diff-mode");
  await waitFor(() => expect(mode()).toBe("unified"));
  now.mockRestore();

  const ws = screen.getByRole("checkbox", { name: "Hide whitespace changes" }) as HTMLInputElement;
  expect(ws.checked).toBe(false);
  await emit("menu://view.hide-whitespace");
  await waitFor(() => expect(ws.checked).toBe(true));
});

test("menu-only items: shortcuts help, not-yet items, sidebar, copy URL and close working copy", async () => {
  renderApp(<AppShell />);
  await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });

  await emit("menu://help.shortcuts");
  const help = await screen.findByRole("dialog", { name: "Keyboard shortcuts", hidden: true });
  expect(await within(help).findByText("Find / Filter")).toBeTruthy();
  await user().click(within(help).getByRole("button", { name: "Close", hidden: true }));
  await waitFor(() => expect(within(help).queryByText("Find / Filter")).toBeNull());

  await emit("menu://branch.merge");
  expect(await screen.findByText("This menu item is not available yet.")).toBeTruthy();

  await emit("menu://edit.copy-url");
  await waitFor(async () =>
    expect(await navigator.clipboard.readText()).toBe("https://svn.example.com/repos/game/trunk"),
  );

  expect(screen.getByRole("button", { name: "Working copy actions" })).toBeTruthy();
  await emit("menu://view.sidebar");
  await waitFor(() =>
    expect(screen.queryByRole("button", { name: "Working copy actions" })).toBeNull(),
  );
  await emit("menu://view.sidebar");
  expect(await screen.findByRole("button", { name: "Working copy actions" })).toBeTruthy();

  await emit("menu://wc.reveal");
  await waitFor(() =>
    expect(mockCalls.filter((c) => c.name === "revealInOs").at(-1)?.args).toEqual([MOCK_WC_ID, ""]),
  );

  await emit("menu://file.close-wc");
  await waitFor(() =>
    expect(mockCalls.filter((c) => c.name === "wcClose").at(-1)?.args).toEqual([MOCK_WC_ID]),
  );
  expect((await screen.findAllByText("No working copy")).length).toBeGreaterThan(0);
});
