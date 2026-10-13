import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, expect, test } from "vitest";
import { AppShell } from "../../app/app-shell";
import { renderApp, setupMockIpc, stubLayout } from "../../test-utils";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
}, 20_000);

test("history -> open file at revision jumps to Browse with the file previewed", async () => {
  renderApp(<AppShell />);
  const views = await screen.findByRole("tablist", { name: "Views" });
  await userEvent.click(within(views).getByRole("tab", { name: "History" }));
  await userEvent.click(await screen.findByRole("option", { name: /Fix shadow cascade flicker/ }));
  await userEvent.click(
    await screen.findByRole("button", { name: /Actions: \/trunk\/src\/engine\/renderer\.ts/ }),
  );
  await userEvent.click(
    await screen.findByRole("menuitem", { name: "Open file at this revision" }),
  );
  expect(within(views).getByRole("tab", { name: "Browse" }).getAttribute("aria-selected")).toBe(
    "true",
  );
  await waitFor(() => expect(document.body.textContent).toContain("export function render"), {
    timeout: 4000,
  });
  expect(screen.getByRole("tab", { name: "Preview" }).getAttribute("aria-selected")).toBe("true");
}, 20_000);

test("browse -> show log of a folder scopes History and the scope chip clears it", async () => {
  renderApp(<AppShell />);
  const views = await screen.findByRole("tablist", { name: "Views" });
  await userEvent.click(within(views).getByRole("tab", { name: "Browse" }));
  await screen.findByRole("option", { name: /README\.md/ });
  await userEvent.click(screen.getByRole("button", { name: "Actions" }));
  await userEvent.click(await screen.findByRole("menuitem", { name: "Show log" }));
  expect(within(views).getByRole("tab", { name: "History" }).getAttribute("aria-selected")).toBe(
    "true",
  );
  const clear = await screen.findByRole("button", { name: "Clear scope" });
  await userEvent.click(clear);
  expect(screen.queryByRole("button", { name: "Clear scope" })).toBeNull();
}, 20_000);

test("settings opens from the toolbar", async () => {
  renderApp(<AppShell />);
  await userEvent.click(await screen.findByRole("button", { name: "Settings" }));
  expect(await screen.findByRole("dialog", { name: "Settings", hidden: true })).toBeTruthy();
}, 20_000);
