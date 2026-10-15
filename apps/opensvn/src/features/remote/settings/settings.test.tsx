import { screen, waitFor, within } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import { SETTING_RANGES } from "../../../ipc/bindings";
import { mockAdapter, mockCalls, setMockRefusedSetting } from "../../../ipc/mock/adapter";
import { mockSettings, mockTrustedCerts } from "../../../ipc/mock/data";
import { installFakeClock, renderApp, setupMockIpc } from "../../../test-utils";
import {
  inRange,
  isGlobalIgnores,
  isHttpsTemplate,
  isProxyUrl,
  MAX_GLOBAL_IGNORES,
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
  const programs = await within(dialog).findAllByDisplayValue("None configured");
  // Diff tool, merge tool, editor and terminal: all display-only.
  expect(programs).toHaveLength(4);
  for (const p of programs) expect((p as HTMLInputElement).readOnly).toBe(true);
});

test("the diff tool is chosen through program_choose (native picker) and can be cleared", async () => {
  renderApp(<SettingsSheet open onClose={() => undefined} />);
  const dialog = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  await user().click(within(dialog).getByRole("button", { name: "Diff & Merge" }));
  await user().click(
    await within(dialog).findByRole("button", { name: "Choose… External diff tool" }),
  );
  // Only the kind and the arguments cross the IPC boundary; the program path comes from Rust.
  const call = mockCalls.filter((c) => c.name === "programChoose").at(-1);
  expect(call?.args).toEqual(["diff-tool", ["{left}", "{right}"]]);
  const shown = await within(dialog).findByDisplayValue("/usr/bin/meld {left} {right}");
  expect((shown as HTMLInputElement).readOnly).toBe(true);
  await user().click(within(dialog).getByRole("button", { name: "Clear External diff tool" }));
  await waitFor(() =>
    expect(within(dialog).queryByDisplayValue("/usr/bin/meld {left} {right}")).toBeNull(),
  );
  expect(mockCalls.some((c) => c.name === "programClear" && c.args[0] === "diff-tool")).toBe(true);
});

test.each([
  ["merge-tool", "External merge tool"],
  ["editor", "Editor"],
  ["terminal", "Terminal"],
] as const)("the %s is chosen through program_choose and can be cleared", async (kind, label) => {
  mockCalls.length = 0;
  renderApp(<SettingsSheet open onClose={() => undefined} />);
  const dialog = await screen.findByRole("dialog", { name: "Settings", hidden: true });
  await user().click(within(dialog).getByRole("button", { name: "Diff & Merge" }));
  await user().click(await within(dialog).findByRole("button", { name: `Choose… ${label}` }));
  const call = mockCalls.filter((c) => c.name === "programChoose").at(-1);
  // No path from the WebView: the kind and the arguments to keep (none yet) only.
  expect(call?.args).toEqual([kind, []]);
  expect(await within(dialog).findByDisplayValue("/usr/bin/meld")).toBeTruthy();
  await user().click(within(dialog).getByRole("button", { name: `Clear ${label}` }));
  await waitFor(() => expect(within(dialog).queryByDisplayValue("/usr/bin/meld")).toBeNull());
  expect(mockCalls.some((c) => c.name === "programClear" && c.args[0] === kind)).toBe(true);
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

// The ranges of the Rust validator (settings/validate.rs SETTING_RANGES), as the bindings carry
// them. A Rust test checks the bindings against the validator; this pins what the UI enforces.
const BACKEND_RANGES = {
  "general.checkIntervalMin": { min: 1, max: 1440 },
  "network.maxConnections": { min: 1, max: 8 },
  "network.timeoutSec": { min: 5, max: 3600 },
  "network.reconnectWaitMin": { min: 1, max: 120 },
  "cache.maxSizeGb": { min: 1, max: 4096 },
  "appearance.fontSize": { min: 11, max: 20 },
  "diff.tabWidth": { min: 1, max: 16 },
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

test("a number in range that the backend refuses says so, not 'enter a whole number'", async () => {
  setMockRefusedSetting("network.timeoutSec");
  try {
    const { dialog, save } = await openOn("Network");
    const timeout = await within(dialog).findByLabelText("Timeout (seconds, 5–3600)");
    await user().clear(timeout);
    await user().type(timeout, "30");
    await user().click(save);
    expect(await within(dialog).findByText("The app did not accept this value")).toBeTruthy();
    expect(within(dialog).queryByText("Enter a whole number from 5 to 3600")).toBeNull();
    expect(timeout.getAttribute("aria-invalid")).toBe("true");
  } finally {
    setMockRefusedSetting(null);
  }
});

test("global ignores: patterns separated by whitespace, no control characters, bounded size", () => {
  expect(isGlobalIgnores("")).toBe(true);
  expect(isGlobalIgnores("*.o *.obj\n.DS_Store\tbuild")).toBe(true);
  expect(isGlobalIgnores("a\u0000b")).toBe(false);
  expect(isGlobalIgnores("a\u001b[31m")).toBe(false);
  expect(isGlobalIgnores("x".repeat(MAX_GLOBAL_IGNORES))).toBe(true);
  expect(isGlobalIgnores("x".repeat(MAX_GLOBAL_IGNORES + 1))).toBe(false);
  // Bytes, like the backend: 2 bytes per "é".
  expect(isGlobalIgnores("é".repeat(MAX_GLOBAL_IGNORES / 2 + 1))).toBe(false);
});

test("general: open last working copy and log level are saved", async () => {
  mockCalls.length = 0;
  const { dialog, save } = await openOn("General");
  await user().click(
    await within(dialog).findByLabelText("Open the last working copy when the app starts"),
  );
  await user().selectOptions(within(dialog).getByLabelText("Log level"), "debug");
  await user().click(save);
  await waitFor(() => expect(mockCalls.some((c) => c.name === "settingsSet")).toBe(true));
  expect(mockCalls.find((c) => c.name === "settingsSet")?.args[0]).toEqual({
    "general.openLastWc": false,
    "general.logLevel": "debug",
  });
  await mockAdapter.commands.settingsSet({
    "general.openLastWc": true,
    "general.logLevel": "info",
  });
});

test("appearance: font size is range-checked and density is saved", async () => {
  mockCalls.length = 0;
  const { dialog, save } = await openOn("Appearance");
  const size = (await within(dialog).findByLabelText("Font size (px, 11–20)")) as HTMLInputElement;
  expect([size.min, size.max]).toEqual(["11", "20"]);
  await user().clear(size);
  await user().type(size, "30");
  expect(await within(dialog).findByText("Enter a whole number from 11 to 20")).toBeTruthy();
  expect(save.disabled).toBe(true);
  await user().clear(size);
  await user().type(size, "15");
  await user().click(within(dialog).getByRole("button", { name: "Compact" }));
  await user().click(save);
  await waitFor(() => expect(mockCalls.some((c) => c.name === "settingsSet")).toBe(true));
  expect(mockCalls.find((c) => c.name === "settingsSet")?.args[0]).toEqual({
    "appearance.fontSize": 15,
    "appearance.density": "compact",
  });
  await mockAdapter.commands.settingsSet({
    "appearance.fontSize": 13,
    "appearance.density": "comfortable",
  });
});

test("diff: tab width is range-checked", async () => {
  const { dialog, save } = await openOn("Diff & Merge");
  const tab = (await within(dialog).findByLabelText(
    "Tab width (columns, 1–16)",
  )) as HTMLInputElement;
  expect([tab.min, tab.max]).toEqual(["1", "16"]);
  await user().clear(tab);
  await user().type(tab, "0");
  expect(await within(dialog).findByText("Enter a whole number from 1 to 16")).toBeTruthy();
  expect(save.disabled).toBe(true);
  await user().clear(tab);
  await user().type(tab, "8");
  expect(save.disabled).toBe(false);
});

test("advanced: global ignores are saved as typed; a control character blocks Save", async () => {
  mockCalls.length = 0;
  const { dialog, save } = await openOn("Advanced");
  const field = await within(dialog).findByLabelText("Global ignore patterns");
  await user().type(field, "*.tmp build");
  await user().click(save);
  await waitFor(() => expect(mockCalls.some((c) => c.name === "settingsSet")).toBe(true));
  expect(mockCalls.find((c) => c.name === "settingsSet")?.args[0]).toEqual({
    "wc.globalIgnores": "*.tmp build",
  });
  // After Save the field shows the stored value; a control character blocks the next Save.
  await user().clear(field);
  await user().type(field, "a\u0007");
  expect(
    await within(dialog).findByText("At most 4096 characters, no control characters"),
  ).toBeTruthy();
  expect(save.disabled).toBe(true);
  await mockAdapter.commands.settingsSet({ "wc.globalIgnores": "" });
});

test("advanced: Clear cache asks first, then calls cache_clear", async () => {
  mockCalls.length = 0;
  const { dialog } = await openOn("Advanced");
  await user().click(await within(dialog).findByRole("button", { name: "Clear cache" }));
  expect(mockCalls.some((c) => c.name === "cacheClear")).toBe(false);
  const ask = within(dialog).getByRole("group", {
    name: "Clear the cache? Working copies are scanned again.",
  });
  await user().click(within(ask).getByRole("button", { name: "Cancel" }));
  expect(mockCalls.some((c) => c.name === "cacheClear")).toBe(false);
  // Focus returns to the button that asked.
  expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Clear cache" }));
  await user().click(within(dialog).getByRole("button", { name: "Clear cache" }));
  const confirm = within(dialog).getByRole("button", { name: "Confirm" });
  expect(document.activeElement).toBe(confirm);
  await user().click(confirm);
  await waitFor(() => expect(mockCalls.some((c) => c.name === "cacheClear")).toBe(true));
  expect(await screen.findByText("Cache cleared")).toBeTruthy();
});

test("accounts: trusted certificates list host:port, fingerprint and expiry; remove asks first", async () => {
  const saved = mockTrustedCerts.map((c) => ({ ...c }));
  mockCalls.length = 0;
  try {
    const { dialog } = await openOn("Accounts");
    const section = await within(dialog).findByRole("region", { name: "Trusted certificates" });
    expect(await within(section).findByText("svn.example.com:443")).toBeTruthy();
    expect(within(section).getByText("Expires 2027-03-01")).toBeTruthy();
    expect(within(section).getByText(/SHA-256 3F:9A:1C:0B/)).toBeTruthy();
    await user().click(
      within(section).getByRole("button", { name: "Remove: svn.example.com:443" }),
    );
    expect(mockCalls.some((c) => c.name === "certsRemove")).toBe(false);
    await user().click(within(section).getByRole("button", { name: "Confirm" }));
    await waitFor(() => expect(mockCalls.some((c) => c.name === "certsRemove")).toBe(true));
    expect(mockCalls.find((c) => c.name === "certsRemove")?.args[0]).toEqual({
      host: "svn.example.com",
      port: 443,
      sha256: saved[0]?.sha256,
    });
    expect(await within(section).findByText("No trusted certificates")).toBeTruthy();
  } finally {
    mockTrustedCerts.splice(0, mockTrustedCerts.length, ...saved);
  }
});

test("accounts: a hostile certificate subject renders as text", async () => {
  const saved = mockTrustedCerts.map((c) => ({ ...c }));
  mockTrustedCerts.splice(0, mockTrustedCerts.length, {
    host: "h.test",
    port: 8443,
    sha256: "ab".repeat(32),
    subject: "<img src=x onerror=alert(1)>",
    expiresAt: null,
    trustedAt: "2026-01-01T00:00:00Z",
  });
  try {
    const { dialog } = await openOn("Accounts");
    expect(await within(dialog).findByText("<img src=x onerror=alert(1)>")).toBeTruthy();
    expect(dialog.querySelector("img")).toBeNull();
    expect(within(dialog).getByText("No expiry recorded")).toBeTruthy();
  } finally {
    mockTrustedCerts.splice(0, mockTrustedCerts.length, ...saved);
  }
});
