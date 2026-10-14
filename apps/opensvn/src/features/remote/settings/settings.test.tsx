import { screen, waitFor, within } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import { SETTING_RANGES } from "../../../ipc/bindings";
import { mockAdapter, mockCalls, setMockRefusedSetting } from "../../../ipc/mock/adapter";
import { mockSettings } from "../../../ipc/mock/data";
import { installFakeClock, renderApp, setupMockIpc } from "../../../test-utils";
import {
  inRange,
  isHttpsTemplate,
  isProxyUrl,
  RANGED_KEYS,
  rangeOf,
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

// The ranges of the Rust validator (settings/validate.rs SETTING_RANGES), as the bindings carry
// them. A Rust test checks the bindings against the validator; this pins what the UI enforces.
const BACKEND_RANGES = {
  "general.checkIntervalMin": { min: 1, max: 1440 },
  "network.maxConnections": { min: 1, max: 8 },
  "network.timeoutSec": { min: 5, max: 3600 },
  "cache.maxSizeGb": { min: 1, max: 4096 },
};

test("every ranged setting uses the backend's bounds, inclusive", () => {
  expect(new Set(RANGED_KEYS)).toEqual(new Set(Object.keys(BACKEND_RANGES)));
  for (const key of RANGED_KEYS) {
    const r = BACKEND_RANGES[key];
    expect(rangeOf(key), key).toEqual(r);
    const errs = (n: number) => validateDraft({ ...mockSettings }, { [key]: n })[key];
    expect(errs(r.min), key).toBeUndefined();
    expect(errs(r.max), key).toBeUndefined();
    expect(errs(r.min - 1), key).toBe("range");
    expect(errs(r.max + 1), key).toBe("range");
    expect(errs(r.min + 0.5), key).toBe("range");
  }
});

test("URL fields refuse credentials, like the backend", () => {
  expect(isProxyUrl("http://u:p@proxy:3128")).toBe(false);
  expect(isHttpsTemplate("https://u:p@{server}/r{rev}")).toBe(false);
});

/** Opens the sheet on `page` and returns the dialog and its Save button. */
async function openOn(page: string) {
  renderApp(<SettingsSheet open onClose={() => undefined} />);
  const dialog = await screen.findByRole("dialog", { name: /Settings|Cài đặt/, hidden: true });
  // The language comes from the settings query, so the page button may appear a moment later.
  await user().click(await within(dialog).findByRole("button", { name: page }));
  const save = within(dialog).getByRole("button", { name: /^(Save|Lưu)$/ }) as HTMLButtonElement;
  return { dialog, save };
}

test("the number inputs carry the backend's min/max, and labels show the range", async () => {
  const { dialog } = await openOn("Network");
  const conns = (await within(dialog).findByLabelText(
    "Parallel connections (1–8)",
  )) as HTMLInputElement;
  expect([conns.min, conns.max]).toEqual(["1", "8"]);
  const timeout = within(dialog).getByLabelText("Timeout (seconds, 5–3600)") as HTMLInputElement;
  expect([timeout.min, timeout.max]).toEqual(["5", "3600"]);
  await user().click(within(dialog).getByRole("button", { name: "General" }));
  const interval = (await within(dialog).findByLabelText(
    "Check for updates every (minutes, 1–1440)",
  )) as HTMLInputElement;
  expect([interval.min, interval.max]).toEqual(["1", "1440"]);
  await user().click(within(dialog).getByRole("button", { name: "Advanced" }));
  const cache = (await within(dialog).findByLabelText(
    "Maximum cache size (GB, 1–4096)",
  )) as HTMLInputElement;
  expect([cache.min, cache.max]).toEqual(["1", "4096"]);
  for (const input of [conns, timeout, interval, cache]) {
    expect(Object.values(SETTING_RANGES).some((r) => String(r.max) === input.max)).toBe(true);
  }
});

test("16 connections: inline error before Save, Save stays disabled until the value is valid", async () => {
  mockCalls.length = 0;
  const { dialog, save } = await openOn("Network");
  const conns = await within(dialog).findByLabelText(/Parallel connections/);
  await user().clear(conns);
  await user().type(conns, "16");
  expect(await within(dialog).findByText("Enter a whole number from 1 to 8")).toBeTruthy();
  expect(conns.getAttribute("aria-invalid")).toBe("true");
  expect(save.disabled).toBe(true);
  await user().click(save);
  expect(mockCalls.some((c) => c.name === "settingsSet")).toBe(false);
  await user().clear(conns);
  await user().type(conns, "8");
  await waitFor(() =>
    expect(within(dialog).queryByText("Enter a whole number from 1 to 8")).toBeNull(),
  );
  expect(save.disabled).toBe(false);
});

test("the inline range error is localized (vi)", async () => {
  await mockAdapter.commands.settingsSet({ "general.language": "vi" });
  try {
    const { dialog, save } = await openOn("Mạng");
    const conns = await within(dialog).findByLabelText("Số kết nối song song (1–8)");
    await user().clear(conns);
    await user().type(conns, "16");
    expect(await within(dialog).findByText("Nhập số nguyên từ 1 đến 8")).toBeTruthy();
    expect(save.disabled).toBe(true);
  } finally {
    await mockAdapter.commands.settingsSet({ "general.language": "en" });
  }
});

test("a value the backend still refuses is shown on its field, never as the raw message", async () => {
  setMockRefusedSetting("network.proxy");
  try {
    const { dialog, save } = await openOn("Network");
    await user().selectOptions(within(dialog).getByRole("combobox"), "custom");
    const url = await within(dialog).findByLabelText("Custom proxy URL", { selector: "input" });
    await user().type(url, "http://proxy.example:3128");
    expect(save.disabled).toBe(false);
    await user().click(save);
    expect(await within(dialog).findByText("The app did not accept this value")).toBeTruthy();
    expect(
      within(dialog).getByText("Could not save settings: check the highlighted field"),
    ).toBeTruthy();
    expect(dialog.textContent).not.toMatch(/Invalid value for setting/);
    expect(save.disabled).toBe(true);
    // Editing the field clears the refusal.
    await user().type(url, "1");
    await waitFor(() =>
      expect(within(dialog).queryByText("The app did not accept this value")).toBeNull(),
    );
    expect(save.disabled).toBe(false);
  } finally {
    setMockRefusedSetting(null);
  }
});
