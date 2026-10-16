import { configure, screen } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";
import { mockAdapter } from "../ipc/mock/adapter";
import { resetWcMock } from "../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../test-utils";
import { AppShell } from "./app-shell";

// Settings > General > "Open the last working copy when the app starts".

vi.mock("../features/wc/diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

vi.setConfig({ testTimeout: 120_000 });
installFakeClock();
configure({ asyncUtilTimeout: 8_000 });

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
}, 20_000);

beforeEach(() => resetWcMock());
afterEach(async () => {
  await mockAdapter.commands.settingsSet({ "general.openLastWc": true });
});

test("on: the last working copy opens at start", async () => {
  renderApp(<AppShell />);
  expect(await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ })).toBeTruthy();
});

test("off: the app starts with no working copy open", async () => {
  await mockAdapter.commands.settingsSet({ "general.openLastWc": false });
  renderApp(<AppShell />);
  expect((await screen.findAllByText("No working copy")).length).toBeGreaterThan(0);
  expect(screen.queryByRole("option", { name: /src\/engine\/renderer\.ts/ })).toBeNull();
});
