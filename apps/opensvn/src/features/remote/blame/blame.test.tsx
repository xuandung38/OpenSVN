import { screen, waitFor } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import { MOCK_REPO_ROOT } from "../../../ipc/mock/remote-data";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import { BlameView } from "./blame-view";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});

// Fake clock: findBy/waitFor budgets count simulated time, so a loaded machine cannot time them out.
const { user } = installFakeClock();
// The per-test timeout is only a hang guard now: under CPU load rendering alone can take many
// wall-clock seconds, while findBy/waitFor budgets run on the fake clock.
vi.setConfig({ testTimeout: 120_000 });

test("blame loads through the operation outcome and shows revision once per run", async () => {
  renderApp(<BlameView url={`${MOCK_REPO_ROOT}/trunk/src/engine/renderer.ts`} rev={null} />);
  expect(screen.getByText("Computing blame…")).toBeTruthy();
  const first = await screen.findAllByRole("option", {}, { timeout: 4000 });
  expect(first.length).toBeGreaterThan(3);
  expect(first[0]?.textContent).toContain("r48213");
  // Source text is rendered as text.
  expect(document.body.textContent).toContain("export function render");
});

test("Enter on a blame line opens that revision", async () => {
  const onOpen = vi.fn();
  renderApp(
    <BlameView
      url={`${MOCK_REPO_ROOT}/trunk/src/engine/renderer.ts`}
      rev={null}
      onOpenRevision={onOpen}
    />,
  );
  const list = await screen.findByRole("listbox", { name: "Blame" }, { timeout: 4000 });
  await waitFor(() => expect(screen.getAllByRole("option").length).toBeGreaterThan(0));
  list.focus();
  await user().keyboard("{Enter}");
  expect(onOpen).toHaveBeenCalledWith(48213);
});
