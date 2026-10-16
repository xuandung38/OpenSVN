import { act, configure, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, describe, expect, test, vi } from "vitest";
import { AppShell } from "../app/app-shell";
import { mockEmit } from "../ipc/mock/adapter";
import { MOCK_WC_ID } from "../ipc/mock/data";
import { clearMockWorkingCopies, resetWcMock, setMockWcBranch } from "../ipc/mock/wc-mock-commands";
import type { Lang } from "../lib/i18n/i18n";
import { expectNoSeriousA11yViolations, prepareDocument } from "../test-axe";
import { renderApp, setupMockIpc, stubLayout } from "../test-utils";

// Automated accessibility check (axe-core) of the main window: Welcome screen, the three views, the
// sidebar sections, the toolbar popovers and menus, in both UI languages. Booting the window in jsdom is CPU heavy; waits are
// generous so a loaded machine slows the run down instead of failing it.

vi.mock("../features/wc/diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

vi.setConfig({ testTimeout: 180_000 });
configure({ asyncUtilTimeout: 60_000 });

beforeAll(() => stubLayout());
beforeEach(() => resetWcMock());

const LABELS = {
  en: {
    views: "Views",
    welcome: "Welcome to OpenSVN",
    history: "History",
    browse: "Browse",
    repo: /Current repository/,
    sections: "Sidebar sections",
    sideHistory: "History",
    incoming: /^Incoming/,
    wcMenu: "Working copy actions",
  },
  vi: {
    views: "Chế độ xem",
    welcome: "Chào mừng đến OpenSVN",
    history: "Lịch sử",
    browse: "Duyệt",
    repo: /Repository hiện tại/,
    sections: "Các mục thanh bên",
    sideHistory: "Lịch sử",
    incoming: /^Sắp về/,
    wcMenu: "Thao tác working copy",
  },
} as const;

/** The panel controlled by the tab named `name` of `tablist` (Radix links them by id). */
function panelOf(tablist: HTMLElement, name: string): HTMLElement {
  const id = within(tablist).getByRole("tab", { name }).getAttribute("aria-controls");
  const panel = id ? document.getElementById(id) : null;
  if (!panel) throw new Error(`no panel for the ${name} tab`);
  return panel;
}

for (const lang of ["en", "vi"] as Lang[]) {
  describe(`main window (${lang})`, () => {
    const l = LABELS[lang];
    beforeAll(async () => {
      prepareDocument(lang);
      await setupMockIpc(lang);
    });

    test("welcome screen", async () => {
      clearMockWorkingCopies();
      renderApp(<AppShell />);
      await screen.findByText(l.welcome);
      await expectNoSeriousA11yViolations(`welcome (${lang})`);
    });

    test("banner strip: another program's lock, out of date, on a tag", async () => {
      setMockWcBranch(MOCK_WC_ID, {
        kind: "tag",
        name: "v1.2",
        url: "https://svn.example.com/repos/game/tags/v1.2",
      });
      renderApp(<AppShell />);
      await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });
      await act(async () => {
        mockEmit("wc://banner", { wcId: MOCK_WC_ID, kind: "wc-busy", data: ["src"] });
        mockEmit("op://started", { opId: "c1", kind: "commit", wcId: MOCK_WC_ID, title: "Commit" });
        mockEmit("op://failed", {
          opId: "c1",
          error: {
            code: "out-of-date",
            message: "out of date",
            detail: null,
            path: null,
            retryable: false,
            actions: ["update"],
          },
        });
      });
      await waitFor(() => expect(screen.getAllByRole("status").length).toBeGreaterThanOrEqual(2));
      expect(screen.getAllByRole("alert").length).toBeGreaterThanOrEqual(1);
      await expectNoSeriousA11yViolations(`banners (${lang})`);
    });

    test("changes, history and browse views and the toolbar popovers", async () => {
      const user = userEvent.setup();
      renderApp(<AppShell />);
      await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });
      await expectNoSeriousA11yViolations(`changes view (${lang})`);

      const views = screen.getByRole("tablist", { name: l.views });
      await user.click(within(views).getByRole("tab", { name: l.history }));
      const revisions = await waitFor(() =>
        within(panelOf(views, l.history)).getAllByRole("option"),
      );
      if (revisions[0]) await user.click(revisions[0]);
      await expectNoSeriousA11yViolations(`history view with a revision selected (${lang})`);

      await user.click(within(views).getByRole("tab", { name: l.browse }));
      await waitFor(() => within(panelOf(views, l.browse)).getAllByRole("treeitem"));
      await expectNoSeriousA11yViolations(`browse view (${lang})`);

      await user.click(screen.getByRole("button", { name: l.repo }));
      await screen.findByRole("dialog");
      await expectNoSeriousA11yViolations(`repository picker (${lang})`);
      await user.keyboard("{Escape}");
      await waitFor(() => {
        if (screen.queryByRole("dialog")) throw new Error("picker still open");
      });

      await user.click(screen.getByRole("button", { name: /^Branch\s*trunk/ }));
      await screen.findByRole("dialog");
      await expectNoSeriousA11yViolations(`branch popover (${lang})`);
    });

    test("sidebar sections, the working copy menu and a file's context menu", async () => {
      const user = userEvent.setup();
      renderApp(<AppShell />);
      const row = await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });

      fireEvent.contextMenu(row);
      await screen.findByRole("menu");
      await expectNoSeriousA11yViolations(`file context menu (${lang})`);
      await user.keyboard("{Escape}");
      await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());

      await user.click(screen.getByRole("button", { name: l.wcMenu }));
      await screen.findByRole("menu");
      await expectNoSeriousA11yViolations(`working copy menu (${lang})`);
      await user.keyboard("{Escape}");
      await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());

      const sections = screen.getByRole("tablist", { name: l.sections });
      await user.click(within(sections).getByRole("tab", { name: l.sideHistory }));
      await expectNoSeriousA11yViolations(`sidebar history (${lang})`);
      await user.click(within(sections).getByRole("tab", { name: l.incoming }));
      await expectNoSeriousA11yViolations(`sidebar incoming (${lang})`);
    });
  });
}
