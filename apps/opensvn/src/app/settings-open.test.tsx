import { act, configure, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";
import { mockEmit } from "../ipc/mock/adapter";
import { clearMockWorkingCopies, resetWcMock } from "../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../test-utils";
import { AppShell } from "./app-shell";

// Every way to open Settings (native menu event, Mod+, and the toolbar gear), with and without a
// working copy. A fresh install (as on a new Mac) starts on the Welcome screen with none.

vi.mock("../features/wc/diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

vi.setConfig({ testTimeout: 120_000 });
const { user } = installFakeClock();
configure({ asyncUtilTimeout: 8_000 });

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
}, 20_000);

beforeEach(() => resetWcMock());

let restorePlatform: (() => void) | null = null;
/** Pretends to run on `platform` (`navigator.platform`, as WKWebView reports "MacIntel"). */
function asPlatform(platform: string, userAgent: string) {
  const own = (k: "platform" | "userAgent", v: string) =>
    Object.defineProperty(navigator, k, { configurable: true, get: () => v });
  own("platform", platform);
  own("userAgent", userAgent);
  restorePlatform = () => {
    delete (navigator as unknown as Record<string, unknown>).platform;
    delete (navigator as unknown as Record<string, unknown>).userAgent;
  };
}
const MAC_UA =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko)";
const LINUX_UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko)";
afterEach(() => {
  restorePlatform?.();
  restorePlatform = null;
});

// The <dialog> stays mounted while closed; jsdom leaves a closed one out of the role tree.
const settingsDialog = () => {
  const el = document.querySelector<HTMLDialogElement>('dialog[aria-label="Settings"]');
  if (!el) throw new Error("the Settings sheet is not mounted");
  return el;
};
const expectOpen = () => waitFor(() => expect(settingsDialog().open).toBe(true));
const close = async () => {
  await user().click(screen.getByRole("button", { name: "Cancel", hidden: true }));
  await waitFor(() => expect(settingsDialog().open).toBe(false));
};

async function renderWelcome() {
  clearMockWorkingCopies();
  renderApp(<AppShell />);
  await screen.findByText("Welcome to OpenSVN");
}

async function renderWithWc() {
  renderApp(<AppShell />);
  await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });
}

test("the native menu item opens Settings on the Welcome screen and with a working copy", async () => {
  await renderWelcome();
  expect(settingsDialog().open).toBe(false);
  await act(async () => mockEmit("menu://app/settings"));
  await expectOpen();
  await close();
});

test("the native menu item opens Settings with a working copy open", async () => {
  await renderWithWc();
  await act(async () => mockEmit("menu://app/settings"));
  await expectOpen();
});

test("the old dotted event name is refused, as Tauri refuses it", () => {
  expect(() => mockEmit("menu://app.settings")).toThrow(/illegal event name/);
});

test("the toolbar gear opens Settings on the Welcome screen", async () => {
  await renderWelcome();
  await user().click(screen.getByRole("button", { name: "Settings" }));
  await expectOpen();
});

test("Cmd+, (metaKey) opens Settings on macOS, once even with the native menu's echo", async () => {
  asPlatform("MacIntel", MAC_UA);
  await renderWelcome();

  // Ctrl+, is not the macOS shortcut.
  await user().keyboard("{Control>},{/Control}");
  expect(settingsDialog().open).toBe(false);

  const now = vi.spyOn(Date, "now").mockReturnValue(5_000_000);
  await user().keyboard("{Meta>},{/Meta}");
  await expectOpen();
  await close();
  // The menu event for the same key press arrives second and is dropped.
  now.mockReturnValue(5_000_100);
  await act(async () => mockEmit("menu://app/settings"));
  expect(settingsDialog().open).toBe(false);
  now.mockRestore();
});

test("Cmd+, opens Settings on macOS when navigator.platform is empty (user agent fallback)", async () => {
  asPlatform("", MAC_UA);
  await renderWithWc();
  await user().keyboard("{Meta>},{/Meta}");
  await expectOpen();
});

test("Ctrl+, opens Settings on Linux and Windows; Meta+, does not", async () => {
  asPlatform("Linux x86_64", LINUX_UA);
  await renderWelcome();
  await user().keyboard("{Meta>},{/Meta}");
  expect(settingsDialog().open).toBe(false);
  await user().keyboard("{Control>},{/Control}");
  await expectOpen();
});

test("Settings still opens and closes on a WebKit without showModal/close (Safari < 15.4)", async () => {
  const proto = HTMLDialogElement.prototype as unknown as Record<string, unknown>;
  const saved = { showModal: proto.showModal, close: proto.close };
  proto.showModal = undefined;
  proto.close = undefined;
  try {
    await renderWelcome();
    await user().click(screen.getByRole("button", { name: "Settings" }));
    await expectOpen();
    await close();
  } finally {
    proto.showModal = saved.showModal;
    proto.close = saved.close;
  }
});
