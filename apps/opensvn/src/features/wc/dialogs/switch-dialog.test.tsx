import { configure, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useEffect } from "react";
import { beforeAll, beforeEach, describe, expect, test } from "vitest";
import { mockCalls } from "../../../ipc/mock/adapter";
import { MOCK_WC_ID } from "../../../ipc/mock/data";
import { MOCK_REPO_ROOT } from "../../../ipc/mock/remote-data";
import { resetWcMock } from "../../../ipc/mock/wc-mock-commands";
import { useStatus, useWorkingCopies } from "../../../ipc/queries";
import { renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import { useWcSession, WcSessionProvider } from "../wc-session";
import { WcDialogHost } from "./wc-dialog-host";

configure({ asyncUtilTimeout: 10_000 });

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});
beforeEach(() => {
  resetWcMock();
  mockCalls.length = 0;
});

function OpenSwitch() {
  const { wc, openDialog } = useWcSession();
  useEffect(() => {
    if (wc) openDialog({ kind: "switch" });
  }, [wc, openDialog]);
  return null;
}

function Harness() {
  const wcs = useWorkingCopies();
  const wc = wcs.data?.find((w) => w.id === MOCK_WC_ID) ?? null;
  const status = useStatus(wc?.id ?? null);
  return (
    <WcSessionProvider wc={wc} files={status.data ?? []}>
      <OpenSwitch />
      <WcDialogHost />
    </WcSessionProvider>
  );
}

async function switchTo(url: string, ignoreAncestry: boolean) {
  renderApp(<Harness />);
  const dlg = await screen.findByRole("dialog");
  const field = within(dlg).getByLabelText("URL in the same repository");
  fireEvent.change(field, { target: { value: url } });
  const box = within(dlg).getByRole("checkbox", { name: "Ignore ancestry" });
  expect((box as HTMLInputElement).checked).toBe(false);
  if (ignoreAncestry) {
    await userEvent.click(box);
    expect(within(dlg).getByText(/shares no history with the working copy/)).toBeTruthy();
  }
  await userEvent.click(within(dlg).getByRole("button", { name: "Switch" }));
  await waitFor(() => expect(mockCalls.some((c) => c.name === "switch")).toBe(true));
  return mockCalls.find((c) => c.name === "switch")?.args;
}

describe("switch dialog", () => {
  test("checks ancestry by default", async () => {
    const url = `${MOCK_REPO_ROOT}/branches/feature`;
    expect(await switchTo(url, false)).toEqual([MOCK_WC_ID, url, null, false]);
  });

  test("ignore ancestry is passed through when ticked, with a warning", async () => {
    const url = `${MOCK_REPO_ROOT}/vendor/other`;
    expect(await switchTo(url, true)).toEqual([MOCK_WC_ID, url, null, true]);
  });
});
