import { screen, waitFor, within } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import { mockCalls } from "../../../ipc/mock/adapter";
import { mockSettings } from "../../../ipc/mock/data";
import { installFakeClock, renderApp, setupMockIpc } from "../../../test-utils";
import {
  inRange,
  isHttpsTemplate,
  isProxyUrl,
  validateDraft,
  withoutUnchanged,
} from "./settings-model";
import { SettingsSheet } from "./settings-sheet";

beforeAll(async () => {
  await setupMockIpc("en");
});

// Fake clock: findBy/waitFor budgets count simulated time, so a loaded machine cannot time them out.
const { user } = installFakeClock();
// The per-test timeout is only a hang guard now: under CPU load rendering alone can take many
// wall-clock seconds, while findBy/waitFor budgets run on the fake clock.
vi.setConfig({ testTimeout: 120_000 });

test("template, proxy and range validation", () => {
  expect(isHttpsTemplate("")).toBe(true);
  expect(isHttpsTemplate("https://{server}/!/#{repo}/commit/r{rev}/")).toBe(true);
  expect(isHttpsTemplate("http://{server}/x")).toBe(false);
  expect(isHttpsTemplate("javascript:alert(1)")).toBe(false);
  expect(isHttpsTemplate("{server}/x")).toBe(false);
  expect(isProxyUrl("http://proxy:3128")).toBe(true);
  expect(isProxyUrl("ftp://x")).toBe(false);
  expect(inRange(4, 1, 16)).toBe(true);
  expect(inRange(0, 1, 16)).toBe(false);
  expect(inRange(Number.NaN, 1, 16)).toBe(false);
  expect(inRange(2.5, 1, 16)).toBe(false);
});

test("validateDraft flags only the bad draft values", () => {
  const errors = validateDraft(
    { ...mockSettings },
    {
      "integrations.webFileUrl": "http://x",
      "network.maxConnections": 99,
      "network.proxy": { mode: "custom", url: "nope" },
    },
  );
  expect(errors).toEqual({
    "integrations.webFileUrl": "https",
    "network.maxConnections": "range",
    "network.proxy": "proxy",
  });
});

test("Save is disabled until something changes and sends only the changed keys", async () => {
  mockCalls.length = 0;
  const onClose = () => undefined;
  renderApp(<SettingsSheet open onClose={onClose} />);
  const dialog = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  const save = within(dialog).getByRole("button", { name: "Save" }) as HTMLButtonElement;
  await waitFor(() =>
    expect(within(dialog).getByLabelText(/Check for updates every/)).toBeTruthy(),
  );
  expect(save.disabled).toBe(true);
  const interval = within(dialog).getByLabelText(/Check for updates every/);
  await user().clear(interval);
  await user().type(interval, "15");
  expect(save.disabled).toBe(false);
  await user().click(save);
  await waitFor(() => expect(mockCalls.some((c) => c.name === "settingsSet")).toBe(true));
  expect(mockCalls.find((c) => c.name === "settingsSet")?.args[0]).toEqual({
    "general.checkIntervalMin": 15,
  });
});

test("an invalid URL template shows an error and blocks Save", async () => {
  renderApp(<SettingsSheet open onClose={() => undefined} />);
  const dialog = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  await user().click(within(dialog).getByRole("button", { name: "Integrations" }));
  const field = await within(dialog).findByLabelText(/Revision URL template/);
  await user().clear(field);
  await user().type(field, "http://insecure/{rev}");
  expect(await within(dialog).findByText(/Must start with https/)).toBeTruthy();
  expect((within(dialog).getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(
    true,
  );
});

test("accounts page lists accounts as text and signs out", async () => {
  mockCalls.length = 0;
  renderApp(<SettingsSheet open onClose={() => undefined} />);
  const dialog = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  await user().click(within(dialog).getByRole("button", { name: "Accounts" }));
  await user().click(await within(dialog).findByRole("button", { name: /Sign out:/ }));
  await waitFor(() => expect(mockCalls.some((c) => c.name === "accountsSignOut")).toBe(true));
});

test("paths and commands are display-only: no editable path/command inputs", async () => {
  renderApp(<SettingsSheet open onClose={() => undefined} />);
  const dialog = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  const root = await within(dialog).findByDisplayValue(mockSettings["general.checkoutRoot"]);
  expect((root as HTMLInputElement).readOnly).toBe(true);
  // The default checkout folder is chosen in the native dialog by Rust; no path is sent.
  await user().click(within(dialog).getByRole("button", { name: "Change…" }));
  expect(await within(dialog).findByDisplayValue("/Users/dev/projects")).toBeTruthy();
  expect(mockCalls.filter((c) => c.name === "checkoutRootChoose").at(-1)?.args).toEqual([]);
  await user().click(within(dialog).getByRole("button", { name: "Diff & Merge" }));
  expect(
    ((await within(dialog).findByDisplayValue("None configured")) as HTMLInputElement).readOnly,
  ).toBe(true);
});

test("the diff tool is chosen through program_choose (native picker) and can be cleared", async () => {
  renderApp(<SettingsSheet open onClose={() => undefined} />);
  const dialog = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  await user().click(within(dialog).getByRole("button", { name: "Diff & Merge" }));
  await user().click(await within(dialog).findByRole("button", { name: "Choose…" }));
  // Only the kind and the arguments cross the IPC boundary; the program path comes from Rust.
  const call = mockCalls.filter((c) => c.name === "programChoose").at(-1);
  expect(call?.args).toEqual(["diff-tool", ["{left}", "{right}"]]);
  const shown = await within(dialog).findByDisplayValue("/usr/bin/meld {left} {right}");
  expect((shown as HTMLInputElement).readOnly).toBe(true);
  await user().click(within(dialog).getByRole("button", { name: "Clear" }));
  expect(await within(dialog).findByDisplayValue("None configured")).toBeTruthy();
  expect(mockCalls.some((c) => c.name === "programClear" && c.args[0] === "diff-tool")).toBe(true);
});

test("changing a value and changing it back removes the key, so Save disables again", async () => {
  renderApp(<SettingsSheet open onClose={() => undefined} />);
  const dialog = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  const interval = await within(dialog).findByLabelText(/Check for updates every/);
  const original = (interval as HTMLInputElement).value;
  const save = within(dialog).getByRole("button", { name: "Save" }) as HTMLButtonElement;
  await user().clear(interval);
  await user().type(interval, "99");
  expect(save.disabled).toBe(false);
  await user().clear(interval);
  await user().type(interval, original);
  expect(save.disabled).toBe(true);
});

test("withoutUnchanged compares objects deeply", () => {
  const s = { ...mockSettings };
  expect(
    withoutUnchanged(s, { "network.proxy": { mode: "system" }, "network.timeoutSec": 5 }),
  ).toEqual({ "network.timeoutSec": 5 });
});

test("a diff tool per file type is chosen through Rust and can be removed", async () => {
  renderApp(<SettingsSheet open onClose={() => undefined} />);
  const dialog = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  await user().click(within(dialog).getByRole("button", { name: "Diff & Merge" }));
  const section = await within(dialog).findByRole("region", { name: "Diff tools by file type" });
  const choose = within(section).getByRole("button", { name: "Choose program…" });
  expect((choose as HTMLButtonElement).disabled).toBe(true);
  await user().type(within(section).getByLabelText("File extension"), ".docx");
  await user().click(choose);
  const call = mockCalls.filter((c) => c.name === "programChooseForExtension").at(-1);
  expect(call?.args).toEqual(["docx", ["{left}", "{right}"]]);
  expect(await within(section).findByText(".docx")).toBeTruthy();
  await user().click(within(section).getByRole("button", { name: "Remove" }));
  expect(await within(section).findByText("No file type has its own tool.")).toBeTruthy();
  expect(mockCalls.some((c) => c.name === "programClearForExtension" && c.args[0] === "docx")).toBe(
    true,
  );
});
