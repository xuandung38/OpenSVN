import { screen, waitFor, within } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import { AppShell } from "../../app/app-shell";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../test-utils";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
}, 20_000);

// Fake clock: findBy/waitFor budgets count simulated time, so a loaded machine cannot time them out.
const { user } = installFakeClock();
// The per-test timeout is only a hang guard now: under CPU load rendering alone can take many
// wall-clock seconds, while findBy/waitFor budgets run on the fake clock.
vi.setConfig({ testTimeout: 120_000 });

test("history -> open file at revision jumps to Browse with the file previewed", async () => {
  renderApp(<AppShell />);
  const views = await screen.findByRole("tablist", { name: "Views" });
  await user().click(within(views).getByRole("tab", { name: "History" }));
  await user().click(await screen.findByRole("option", { name: /Fix shadow cascade flicker/ }));
  await user().click(
    await screen.findByRole("button", { name: /Actions: \/trunk\/src\/engine\/renderer\.ts/ }),
  );
  await user().click(await screen.findByRole("menuitem", { name: "Open file at this revision" }));
  expect(within(views).getByRole("tab", { name: "Browse" }).getAttribute("aria-selected")).toBe(
    "true",
  );
  await waitFor(() => expect(document.body.textContent).toContain("export function render"), {
    timeout: 4000,
  });
  expect(screen.getByRole("tab", { name: "Preview" }).getAttribute("aria-selected")).toBe("true");
});

test("browse -> show log of a folder scopes History and the scope chip clears it", async () => {
  renderApp(<AppShell />);
  const views = await screen.findByRole("tablist", { name: "Views" });
  await user().click(within(views).getByRole("tab", { name: "Browse" }));
  await screen.findByRole("option", { name: /README\.md/ });
  await user().click(screen.getByRole("button", { name: "Actions" }));
  await user().click(await screen.findByRole("menuitem", { name: "Show log" }));
  expect(within(views).getByRole("tab", { name: "History" }).getAttribute("aria-selected")).toBe(
    "true",
  );
  const clear = await screen.findByRole("button", { name: "Clear scope" });
  await user().click(clear);
  expect(screen.queryByRole("button", { name: "Clear scope" })).toBeNull();
});

test("settings opens from the toolbar", async () => {
  renderApp(<AppShell />);
  await user().click(await screen.findByRole("button", { name: "Settings" }));
  expect(await screen.findByRole("dialog", { name: "Settings", hidden: true })).toBeTruthy();
});
