import { configure, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";
import { AppShell } from "../../../app/app-shell";
import { mockAdapter, mockCalls, setMockOpDuration } from "../../../ipc/mock/adapter";
import { MOCK_WC_ID } from "../../../ipc/mock/data";
import { markMockWcIncomplete, resetWcMock } from "../../../ipc/mock/wc-mock-commands";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../../test-utils";

vi.mock("../diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

// Each test boots the whole window; the timeout is only a hang guard (waits use the fake clock).
vi.setConfig({ testTimeout: 120_000 });
const { user } = installFakeClock();
configure({ asyncUtilTimeout: 8_000 });

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
}, 20_000);

beforeEach(() => {
  resetWcMock();
  mockCalls.length = 0;
  setMockOpDuration(40);
});

afterEach(async () => {
  await mockAdapter.commands.settingsSet({ "general.language": "en" });
});

const lastCall = (name: string) => mockCalls.filter((c) => c.name === name).at(-1);
const calls = (name: string) => mockCalls.filter((c) => c.name === name).length;

const TEXT = {
  en: {
    dialog: "Checkout repository",
    url: "Repository URL",
    folder: "Folder name",
    checkout: "Checkout",
    note: "This folder already holds an unfinished checkout of this URL (revision 1200).",
    continue: "Continue checkout",
    latest: "Update to latest instead",
    bannerTitle: "Checkout incomplete",
    bannerText: /Continue it at revision 1200/,
    cleanupBanner: "Working copy is locked",
  },
  vi: {
    dialog: "Checkout repository",
    url: "URL repository",
    folder: "Tên thư mục",
    checkout: "Checkout",
    note: "Thư mục này đang chứa một checkout chưa hoàn tất của URL này (revision 1200).",
    continue: "Tiếp tục checkout",
    latest: "Update lên bản mới nhất",
    bannerTitle: "Checkout chưa hoàn tất",
    bannerText: /Tiếp tục tại revision 1200/,
    cleanupBanner: "Working copy đang bị khóa",
  },
} as const;

for (const lang of ["en", "vi"] as const) {
  const t = TEXT[lang];

  test(`${lang}: an incomplete working copy shows a banner with Continue checkout and Update to latest`, async () => {
    await mockAdapter.commands.settingsSet({ "general.language": lang });
    markMockWcIncomplete(MOCK_WC_ID, 1200);
    renderApp(<AppShell />);
    await screen.findByRole("option", { name: /renderer\.ts/ });
    const title = await screen.findByText(t.bannerTitle);
    const banner = title.closest("[role=status]") as HTMLElement;
    expect(banner.textContent).toMatch(t.bannerText);
    // Not an error and not a cleanup case.
    expect(banner.getAttribute("role")).toBe("status");
    expect(screen.queryByText(t.cleanupBanner)).toBeNull();
    await user().click(within(banner).getByRole("button", { name: t.continue }));
    await waitFor(() => expect(lastCall("wcContinue")?.args).toEqual([MOCK_WC_ID]));
    expect(calls("update")).toBe(0);
  });

  test(`${lang}: Update to latest instead keeps the choices of the interrupted checkout`, async () => {
    await mockAdapter.commands.settingsSet({ "general.language": lang });
    markMockWcIncomplete(MOCK_WC_ID, 1200);
    renderApp(<AppShell />);
    await screen.findByRole("option", { name: /renderer\.ts/ });
    const banner = (await screen.findByText(t.bannerTitle)).closest("[role=status]") as HTMLElement;
    await user().click(within(banner).getByRole("button", { name: t.latest }));
    // Not a plain update: the backend reads the checkout journal (ignore-externals), as Continue.
    await waitFor(() => expect(lastCall("wcUpdateLatest")?.args).toEqual([MOCK_WC_ID]));
    expect(calls("update")).toBe(0);
    expect(calls("wcContinue")).toBe(0);
  });
}

test("checking out into the folder of an unfinished checkout of the same URL offers to continue it", async () => {
  markMockWcIncomplete("wc-docs", 1200);
  renderApp(<AppShell />);
  await screen.findByRole("option", { name: /renderer\.ts/ });
  await user().keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
  const t = TEXT.en;
  const dlg = await screen.findByRole("dialog", { name: t.dialog, hidden: true });
  await user().type(
    within(dlg).getByRole("combobox", { name: t.url }),
    "https://svn.example.com/repos/docs",
  );
  const name = within(dlg).getByRole("textbox", { name: t.folder });
  await user().clear(name);
  await user().type(name, "design-docs");
  const submit = within(dlg).getByRole("button", { name: t.checkout }) as HTMLButtonElement;
  await waitFor(() => expect(submit.disabled).toBe(false));
  await user().click(submit);
  const note = await within(dlg).findByTestId("checkout-incomplete");
  expect(note.textContent).toContain(t.note);
  // Offered, not shown as a failure.
  expect(within(dlg).queryByRole("alert")).toBeNull();
  await user().click(within(note).getByRole("button", { name: t.continue }));
  await waitFor(() => expect(lastCall("wcContinue")?.args).toEqual(["wc-docs"]));
  // The dialog follows the continued checkout and closes when it is done.
  await waitFor(() =>
    expect(screen.queryByRole("dialog", { name: t.dialog, hidden: true })).toBeNull(),
  );
});
