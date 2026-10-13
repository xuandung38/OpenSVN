import { screen, waitFor, within } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import { mockCalls } from "../../../ipc/mock/adapter";
import { MOCK_REPO_ROOT, myLocks, otherLocks } from "../../../ipc/mock/remote-data";
import { installFakeClock, renderApp, setupMockIpc } from "../../../test-utils";
import { LockDialog } from "./lock-dialog";

beforeAll(async () => {
  await setupMockIpc("en");
});

// Fake clock: findBy/waitFor budgets count simulated time, so a loaded machine cannot time them out.
const { user } = installFakeClock();
// The per-test timeout is only a hang guard now: under CPU load rendering alone can take many
// wall-clock seconds, while findBy/waitFor budgets run on the fake clock.
vi.setConfig({ testTimeout: 120_000 });

const FILE = `${MOCK_REPO_ROOT}/trunk/assets/models/crate.fbx`;
const free = `${MOCK_REPO_ROOT}/trunk/README.md`;

test("locking a free file sends a plain lock with the comment", async () => {
  mockCalls.length = 0;
  const onClose = vi.fn();
  renderApp(<LockDialog mode="lock" targets={[{ url: free, lock: null }]} onClose={onClose} />);
  const dialog = await screen.findByRole("dialog", { name: "Lock to edit", hidden: true });
  await user().type(within(dialog).getByLabelText(/Comment/), "editing");
  await user().click(within(dialog).getByRole("button", { name: "Lock" }));
  await waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(mockCalls.find((c) => c.name === "lock")?.args[0]).toEqual({
    targets: { kind: "url", urls: [free] },
    comment: "editing",
    steal: null,
  });
  myLocks.delete(free);
});

test("a lock held by someone else shows owner/comment and needs a tick before stealing", async () => {
  mockCalls.length = 0;
  renderApp(
    <LockDialog
      mode="lock"
      onClose={() => undefined}
      targets={[
        {
          url: FILE,
          lock: {
            owner: "an",
            comment: "reworking <b>model</b>",
            createdAt: "2026-10-12T08:00:00Z",
            token: null,
            mine: false,
          },
        },
      ]}
    />,
  );
  const dialog = await screen.findByRole("dialog", { name: "Lock to edit", hidden: true });
  expect(within(dialog).getByText(/Locked by an/)).toBeTruthy();
  // Comment from the server is plain text.
  expect(within(dialog).getByText(/reworking <b>model<\/b>/)).toBeTruthy();
  expect(dialog.querySelector("b")).toBeNull();
  const steal = within(dialog).getByRole("button", { name: "Steal lock" }) as HTMLButtonElement;
  expect(steal.disabled).toBe(true);
  await user().click(within(dialog).getByLabelText(/I understand and want to steal/));
  expect(steal.disabled).toBe(false);
  await user().click(steal);
  await waitFor(() => expect(mockCalls.some((c) => c.name === "lock")).toBe(true));
  expect(mockCalls.find((c) => c.name === "lock")?.args[0]).toMatchObject({ steal: true });
  otherLocks.add(FILE);
  myLocks.delete(FILE);
});

test("server locked-by-other error reveals the steal option", async () => {
  otherLocks.add(FILE);
  renderApp(
    <LockDialog mode="lock" targets={[{ url: FILE, lock: null }]} onClose={() => undefined} />,
  );
  const dialog = await screen.findByRole("dialog", { name: "Lock to edit", hidden: true });
  await user().click(within(dialog).getByRole("button", { name: "Lock" }));
  expect(await within(dialog).findByRole("button", { name: "Steal lock" })).toBeTruthy();
  expect(
    (within(dialog).getByRole("button", { name: "Steal lock" }) as HTMLButtonElement).disabled,
  ).toBe(true);
});

test("unlocking someone else's lock requires confirming the break", async () => {
  mockCalls.length = 0;
  renderApp(
    <LockDialog
      mode="unlock"
      onClose={() => undefined}
      targets={[
        {
          url: FILE,
          lock: {
            owner: "an",
            comment: null,
            createdAt: "2026-10-12T08:00:00Z",
            token: null,
            mine: false,
          },
        },
      ]}
    />,
  );
  const dialog = await screen.findByRole("dialog", { name: "Unlock", hidden: true });
  const button = within(dialog).getByRole("button", { name: "Unlock" }) as HTMLButtonElement;
  expect(button.disabled).toBe(true);
  await user().click(within(dialog).getByLabelText(/Break the lock/));
  await user().click(button);
  await waitFor(() => expect(mockCalls.some((c) => c.name === "unlock")).toBe(true));
  expect(mockCalls.find((c) => c.name === "unlock")?.args[0]).toEqual({
    targets: { kind: "url", urls: [FILE] },
    breakLock: true,
  });
});
