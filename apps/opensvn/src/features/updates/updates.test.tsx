import { act, configure, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, expect, test, vi } from "vitest";
import { AppShell } from "../../app/app-shell";
import {
  mockCalls,
  mockEmit,
  mockUpdater,
  resetMockUpdater,
  setMockUpdateChunkMs,
} from "../../ipc/mock/adapter";
import { mockSettings } from "../../ipc/mock/data";
import { resetWcMock } from "../../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../test-utils";
import { SettingsSheet } from "../remote/settings/settings-sheet";
import { MENU_ACTIONS } from "../wc/use-wc-shortcuts";
import { useAutoUpdateCheck } from "./auto-check";
import { UpdateDialog } from "./update-dialog";
import {
  AUTO_CHECK_INTERVAL_MS,
  autoCheckDue,
  percentOf,
  readLastAutoCheck,
  updateErrorKey,
  writeLastAutoCheck,
} from "./update-model";

vi.mock("../wc/diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

vi.setConfig({ testTimeout: 120_000 });
const { user } = installFakeClock();
configure({ asyncUtilTimeout: 8_000 });

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
}, 20_000);

beforeEach(() => {
  resetMockUpdater();
  resetWcMock();
  setMockUpdateChunkMs(10);
  mockCalls.length = 0;
  localStorage.clear();
  Object.assign(mockSettings, { "updates.autoCheck": true });
});

const noop = () => undefined;
const calls = (name: string) => mockCalls.filter((c) => c.name === name);
const dialog = () => screen.findByRole("dialog", { name: "App updates", hidden: true });
/** Lets pending timers and promises run on the fake clock. */
const flush = (ms = 200) =>
  act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
/** The whole window, with the menu subscriptions active (the file list loaded). */
async function renderShell() {
  renderApp(<AppShell />);
  await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });
}

test("daily check timing, progress and error texts", () => {
  const day = AUTO_CHECK_INTERVAL_MS;
  expect(autoCheckDue(10 * day, null)).toBe(true);
  expect(autoCheckDue(10 * day, 10 * day - 1000)).toBe(false);
  expect(autoCheckDue(10 * day, 9 * day)).toBe(true);
  // A last check "in the future" (clock moved back) does not block checks for good.
  expect(autoCheckDue(10 * day, 11 * day)).toBe(true);
  writeLastAutoCheck(1234);
  expect(readLastAutoCheck()).toBe(1234);
  localStorage.setItem("opensvn.updates.lastAutoCheck", "garbage");
  expect(readLastAutoCheck()).toBeNull();

  expect(percentOf(1, 4)).toBe(25);
  expect(percentOf(5, 4)).toBe(100);
  expect(percentOf(5, null)).toBeNull();
  expect(percentOf(5, 0)).toBeNull();

  expect(updateErrorKey({ code: "forbidden" })).toBe("errForbidden");
  expect(updateErrorKey({ code: "network" })).toBe("errNetwork");
  expect(updateErrorKey({ code: "timeout" })).toBe("errNetwork");
  expect(updateErrorKey({ code: "internal" })).toBeNull();
});

test("the menu item opens the update check instead of 'not available yet'", async () => {
  expect(MENU_ACTIONS["menu://app/check-app-updates"]).toBe("checkAppUpdates");
  resetMockUpdater({ token: true, next: null });
  await renderShell();
  await act(async () => mockEmit("menu://app/check-app-updates"));
  const dlg = await dialog();
  await within(dlg).findByText("OpenSVN 0.1.0 is the newest version.");
  expect(screen.queryByText(/not available yet/i)).toBeNull();
});

test("without a token the check says 'not configured', sends nothing and leads to Settings", async () => {
  await renderShell();
  await act(async () => mockEmit("menu://app/check-app-updates"));
  const dlg = await dialog();
  await within(dlg).findByText(/no release access token is stored/);
  expect(mockUpdater.requests).toBe(0);
  await user().click(within(dlg).getByRole("button", { name: "Open Settings", hidden: true }));
  const sheet = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  // Settings opens on its Updates page.
  expect(
    within(sheet)
      .getByRole("button", { name: "Updates", hidden: true })
      .getAttribute("aria-current"),
  ).toBe("page");
  await within(sheet).findByText("Not configured");
});

test("a build without update source says so and sends nothing", async () => {
  resetMockUpdater({ token: true, buildConfigured: false });
  renderApp(<UpdateDialog open onClose={noop} onOpenSettings={noop} />);
  const dlg = await dialog();
  await within(dlg).findByText(/This build cannot update itself/);
  expect(within(dlg).queryByRole("button", { name: "Open Settings", hidden: true })).toBeNull();
  expect(mockUpdater.requests).toBe(0);
});

test("available: notes as plain text, Install with progress, then the restart prompt", async () => {
  resetMockUpdater({
    token: true,
    next: { version: "0.2.0", notes: "## Features\n- <b>faster</b> status" },
  });
  renderApp(<UpdateDialog open onClose={noop} />);
  const dlg = await dialog();
  await within(dlg).findByText("OpenSVN 0.2.0 is available.");
  within(dlg).getByText("You have 0.1.0.");
  // Release notes are text, never markup.
  expect(within(dlg).getByText(/<b>faster<\/b> status/)).toBeTruthy();
  expect(dlg.querySelector("b")).toBeNull();

  setMockUpdateChunkMs(200);
  await user().click(within(dlg).getByRole("button", { name: "Install", hidden: true }));
  await within(dlg).findByText("Downloading the update…");
  const bar = within(dlg).getByRole("progressbar", { name: "Download progress", hidden: true });
  await waitFor(() => expect(Number(bar.getAttribute("value"))).toBeGreaterThan(0));
  within(dlg).getByText(/of 3\.8 MB/);
  await within(dlg).findByText("The update is installed. Restart OpenSVN to use it.");
  expect(mockUpdater.restarts).toBe(0);
  await user().click(within(dlg).getByRole("button", { name: "Restart now", hidden: true }));
  await waitFor(() => expect(mockUpdater.restarts).toBe(1));
});

test("a failed install can be tried again", async () => {
  resetMockUpdater({ token: true, next: { version: "0.2.0", notes: null }, installFails: true });
  renderApp(<UpdateDialog open onClose={noop} />);
  const dlg = await dialog();
  await within(dlg).findByText("This release has no notes.");
  await user().click(within(dlg).getByRole("button", { name: "Install", hidden: true }));
  await within(dlg).findByText(/The update could not be installed/);
  mockUpdater.installFails = false;
  await user().click(within(dlg).getByRole("button", { name: "Retry", hidden: true }));
  await within(dlg).findByText(/The update is installed/);
});

test("a refused token shows its own error, marks the token invalid, and Retry checks again", async () => {
  resetMockUpdater({ token: true, next: "forbidden" });
  renderApp(
    <>
      <SettingsSheet open onClose={noop} initialPage="updates" />
      <UpdateDialog open onClose={noop} />
    </>,
  );
  const dlg = await dialog();
  await within(dlg).findByText(/The release server refused the token/);
  const sheet = screen.getByRole("dialog", { name: "Settings", hidden: true });
  await within(sheet).findByText(/Invalid: the release server refused it/);

  mockUpdater.next = null;
  await user().click(within(dlg).getByRole("button", { name: "Retry", hidden: true }));
  await within(dlg).findByText("OpenSVN 0.1.0 is the newest version.");
  await within(sheet).findByText("Configured");
});

test("offline: a clear error and Retry", async () => {
  resetMockUpdater({ token: true, next: "offline" });
  renderApp(<UpdateDialog open onClose={noop} />);
  const dlg = await dialog();
  await within(dlg).findByText(/could not be reached/);
  expect(within(dlg).getByRole("button", { name: "Retry", hidden: true })).toBeTruthy();
});

test("Settings › Updates: the token is write-only, saved, never shown, and can be cleared", async () => {
  const secret = "github_pat_SECRET123";
  renderApp(<SettingsSheet open onClose={noop} initialPage="updates" />);
  const sheet = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  await within(sheet).findByText("0.1.0");
  await within(sheet).findByText("Not configured");
  const save = within(sheet).getByRole("button", { name: "Save token", hidden: true });
  expect((save as HTMLButtonElement).disabled).toBe(true);

  const field = (await within(sheet).findByLabelText("New token")) as HTMLInputElement;
  expect(field.type).toBe("password");
  await user().type(field, secret);
  await user().click(save);
  await within(sheet).findByText("Configured");
  expect(field.value).toBe("");
  expect(calls("updaterTokenSet").map((c) => c.args[0])).toEqual([secret]);
  expect(document.body.innerHTML).not.toContain(secret);

  await user().click(within(sheet).getByRole("button", { name: "Clear token", hidden: true }));
  await user().click(within(sheet).getByRole("button", { name: "Confirm", hidden: true }));
  await within(sheet).findByText("Not configured");
  expect(mockUpdater.token).toBe(false);
});

test("Settings › Updates: a refused token value shows the backend's reason", async () => {
  renderApp(<SettingsSheet open onClose={noop} initialPage="updates" />);
  const sheet = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  await user().type(await within(sheet).findByLabelText("New token"), "a\u0007b");
  await user().click(within(sheet).getByRole("button", { name: "Save token", hidden: true }));
  await within(sheet).findByText(/Could not save the token: The token is not valid/);
  expect(mockUpdater.token).toBe(false);
});

test("Settings › Updates: the daily check setting is saved with Save, and Check opens the dialog", async () => {
  resetMockUpdater({ token: true });
  renderApp(<SettingsSheet open onClose={noop} initialPage="updates" />);
  const sheet = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  const auto = (await within(sheet).findByLabelText(/once a day/)) as HTMLInputElement;
  expect(auto.checked).toBe(true);
  await user().click(auto);
  await user().click(within(sheet).getByRole("button", { name: "Save", hidden: true }));
  await waitFor(() =>
    expect(calls("settingsSet").at(-1)?.args[0]).toEqual({ "updates.autoCheck": false }),
  );
});

test("Settings › Updates: Check for updates runs the check", async () => {
  resetMockUpdater({ token: true, next: { version: "0.3.0", notes: null } });
  renderApp(<SettingsSheet open onClose={noop} initialPage="updates" />);
  const sheet = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  await user().click(
    await within(sheet).findByRole("button", { name: "Check for updates", hidden: true }),
  );
  const dlg = await dialog();
  await within(dlg).findByText("OpenSVN 0.3.0 is available.");
});

function AutoCheck({ now }: { now: () => number }) {
  useAutoUpdateCheck(now);
  return null;
}

test("start-up check: once a day, only with a token and the setting on, shown as a toast", async () => {
  const day = AUTO_CHECK_INTERVAL_MS;
  resetMockUpdater({ token: true, next: { version: "0.2.0", notes: null } });
  const first = renderApp(<AutoCheck now={() => 100 * day} />);
  await screen.findByText("OpenSVN 0.2.0 is available. Open Settings › Updates to install it.");
  expect(mockUpdater.requests).toBe(1);
  expect(readLastAutoCheck()).toBe(100 * day);
  first.unmount();

  // Same day: no request.
  const second = renderApp(<AutoCheck now={() => 100 * day + 1000} />);
  await flush();
  expect(mockUpdater.requests).toBe(1);
  second.unmount();

  // Next day, setting off: no request.
  Object.assign(mockSettings, { "updates.autoCheck": false });
  const third = renderApp(<AutoCheck now={() => 102 * day} />);
  await flush();
  expect(mockUpdater.requests).toBe(1);
  third.unmount();
});

test("start-up check without a token asks nothing and records nothing", async () => {
  renderApp(<AutoCheck now={() => 5} />);
  await waitFor(() => expect(calls("updaterTokenStatus").length).toBeGreaterThan(0));
  await flush();
  expect(calls("updateCheck")).toHaveLength(0);
  expect(readLastAutoCheck()).toBeNull();
});

test("start-up check failures stay silent", async () => {
  resetMockUpdater({ token: true, next: "offline" });
  renderApp(<AutoCheck now={() => 5} />);
  await waitFor(() => expect(calls("updateCheck")).toHaveLength(1));
  await flush();
  expect(screen.queryByText(/could not be reached/)).toBeNull();
});
