import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { configure, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, describe, expect, test, vi } from "vitest";
import { AppShell } from "../app/app-shell";
import { ToastProvider } from "../components/ui/toast";
import { ActivityPanel } from "../features/activity/activity-panel";
import { applyProgress, newOpState } from "../ipc/events";
import { setMockOpDuration } from "../ipc/mock/adapter";
import { MOCK_WC_ID } from "../ipc/mock/data";
import { markMockWcIncomplete, resetWcMock } from "../ipc/mock/wc-mock-commands";
import { I18nProvider, type Lang } from "../lib/i18n/i18n";
import { expectNoSeriousA11yViolations, prepareDocument } from "../test-axe";
import { renderApp, setupMockIpc, stubLayout } from "../test-utils";

// axe-core over the screens of an interrupted checkout: the banner of an incomplete working copy,
// the checkout dialog offering to continue, and an operation waiting for a lost connection.

vi.mock("../features/wc/diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

vi.setConfig({ testTimeout: 180_000 });
configure({ asyncUtilTimeout: 60_000 });

beforeAll(() => stubLayout());
beforeEach(() => {
  resetWcMock();
  setMockOpDuration(40);
});

const LABELS = {
  en: {
    bannerTitle: "Checkout incomplete",
    dialog: "Checkout repository",
    url: "Repository URL",
    folder: "Folder name",
    checkout: "Checkout",
    lost: "Connection lost",
    reconnected: "Reconnected",
  },
  vi: {
    bannerTitle: "Checkout chưa hoàn tất",
    dialog: "Checkout repository",
    url: "URL repository",
    folder: "Tên thư mục",
    checkout: "Checkout",
    lost: "Mất kết nối",
    reconnected: "Đã kết nối lại",
  },
} as const;

for (const lang of ["en", "vi"] as Lang[]) {
  describe(`interrupted checkout (${lang})`, () => {
    const l = LABELS[lang];
    beforeAll(async () => {
      prepareDocument(lang);
      await setupMockIpc(lang);
    });

    test("the banner of an incomplete working copy", async () => {
      markMockWcIncomplete(MOCK_WC_ID, 1200);
      renderApp(<AppShell />);
      await screen.findByRole("option", { name: /renderer\.ts/ });
      await screen.findByText(l.bannerTitle);
      await expectNoSeriousA11yViolations(`incomplete banner (${lang})`);
    });

    test("the checkout dialog offering to continue an unfinished checkout", async () => {
      const user = userEvent.setup();
      markMockWcIncomplete("wc-docs", 1200);
      renderApp(<AppShell />);
      await screen.findByRole("option", { name: /renderer\.ts/ });
      await user.keyboard("{Control>}{Shift>}o{/Shift}{/Control}");
      const dlg = await screen.findByRole("dialog", { name: l.dialog, hidden: true });
      await user.type(
        within(dlg).getByRole("combobox", { name: l.url }),
        "https://svn.example.com/repos/docs",
      );
      const name = within(dlg).getByRole("textbox", { name: l.folder });
      await user.clear(name);
      await user.type(name, "design-docs");
      const submit = within(dlg).getByRole("button", { name: l.checkout }) as HTMLButtonElement;
      await waitFor(() => expect(submit.disabled).toBe(false));
      await user.click(submit);
      await within(dlg).findByTestId("checkout-incomplete");
      await expectNoSeriousA11yViolations(`checkout dialog, continue offered (${lang})`);
    });

    test("an operation waiting for a lost connection", async () => {
      const op = applyProgress(
        newOpState(
          {
            opId: "op1",
            kind: "checkout",
            wcId: null,
            title: "Checkout REPO",
            rootPath: "/home/me/src/REPO",
            background: false,
          },
          0,
        ),
        {
          opId: "op1",
          phase: "checkout",
          current: 3,
          total: null,
          bytesDone: 2048,
          bytesTotal: null,
          currentPath: null,
          stage: "waiting-for-connection",
          entries: [],
          entriesDropped: 0,
          counts: {
            added: 3,
            deleted: 0,
            updated: 0,
            merged: 0,
            conflicted: 0,
            treeConflicted: 0,
            existed: 0,
            restored: 0,
            skipped: 0,
          },
          connectionWait: { retryInSecs: 8, waitedSecs: 64, maxWaitSecs: 600, attempt: 3 },
        },
        Date.now(),
      );
      const qc = new QueryClient();
      const ui = (o: typeof op) => (
        <QueryClientProvider client={qc}>
          <I18nProvider lang={lang}>
            <ToastProvider>
              <ActivityPanel ops={[o]} onCancel={() => {}} />
            </ToastProvider>
          </I18nProvider>
        </QueryClientProvider>
      );
      const r = render(ui(op));
      await screen.findByTestId("connection-wait");
      expect(screen.getByTestId("connection-wait-live").textContent).toBe(l.lost);
      await expectNoSeriousA11yViolations(`waiting for the connection (${lang})`);
      r.rerender(ui({ ...op, progress: op.progress && { ...op.progress, stage: "fetching" } }));
      expect(screen.queryByTestId("connection-wait")).toBeNull();
      expect(screen.getByTestId("connection-wait-live").textContent).toBe(l.reconnected);
      await expectNoSeriousA11yViolations(`reconnected (${lang})`);
    });
  });
}
