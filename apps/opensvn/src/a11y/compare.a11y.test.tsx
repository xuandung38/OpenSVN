import { act, configure, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, describe, test, vi } from "vitest";
import { AppShell } from "../app/app-shell";
import { mockEmit } from "../ipc/mock/adapter";
import { resetWcMock } from "../ipc/mock/wc-mock-commands";
import type { Lang } from "../lib/i18n/i18n";
import { expectNoSeriousA11yViolations, prepareDocument } from "../test-axe";
import { renderApp, setupMockIpc, stubLayout } from "../test-utils";

// axe-core over Branch > Compare with branch (#18 follow-up): the revision pickers (a bad number
// included), the summary of differences, a file diff, and the working copy with local changes,
// in both UI languages.

vi.mock("../features/wc/diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

vi.setConfig({ testTimeout: 180_000 });
configure({ asyncUtilTimeout: 60_000 });

beforeAll(() => stubLayout());
beforeEach(() => resetWcMock());

const LABELS = {
  en: {
    dialog: "Compare with branch",
    branch: "Branch or tag",
    compare: "Compare",
    rightRev: "Revision of the other folder",
    rightNumber: "Revision of the other folder: revision number",
    local: "Include local changes (compare the working copy itself)",
    table: /^Differences between/,
    diff: "Show diff of src/engine/renderer.ts",
  },
  vi: {
    dialog: "So sánh với branch",
    branch: "Branch hoặc tag",
    compare: "So sánh",
    rightRev: "Revision của thư mục bên kia",
    rightNumber: "Revision của thư mục bên kia: số revision",
    local: "Tính cả thay đổi local (so sánh chính working copy)",
    table: /^Khác biệt giữa/,
    diff: "Xem diff của src/engine/renderer.ts",
  },
} as const;

for (const lang of ["en", "vi"] as Lang[]) {
  describe(`compare with branch (${lang})`, () => {
    const l = LABELS[lang];
    beforeAll(async () => {
      prepareDocument(lang);
      await setupMockIpc(lang);
    });

    test("revisions, the summary, a file diff and local changes", async () => {
      const user = userEvent.setup();
      renderApp(<AppShell />);
      await screen.findByRole("option", { name: /src\/engine\/renderer\.ts/ });
      await act(async () => mockEmit("menu://branch/compare"));
      const dlg = await screen.findByRole("dialog", { name: l.dialog, hidden: true });
      const q = within(dlg);
      await expectNoSeriousA11yViolations(`compare, empty (${lang})`);

      await user.selectOptions(
        await q.findByRole("combobox", { name: l.branch, hidden: true }),
        await q.findByRole("option", { name: "feature-x", hidden: true }),
      );
      await user.selectOptions(q.getByRole("combobox", { name: l.rightRev, hidden: true }), "rev");
      const number = q.getByRole("textbox", { name: l.rightNumber, hidden: true });
      await user.type(number, "x1");
      await q.findByRole("alert", { hidden: true });
      await expectNoSeriousA11yViolations(`compare, bad revision (${lang})`);
      await user.clear(number);
      await user.type(number, "48100");

      await user.click(q.getByRole("button", { name: l.compare, hidden: true }));
      await q.findByRole("table", { name: l.table, hidden: true });
      await expectNoSeriousA11yViolations(`compare, summary (${lang})`);

      await user.click(q.getByRole("button", { name: l.diff, hidden: true }));
      await q.findByTestId("editor");
      await expectNoSeriousA11yViolations(`compare, file diff (${lang})`);

      await user.click(q.getByRole("checkbox", { name: l.local, hidden: true }));
      await user.click(q.getByRole("button", { name: l.compare, hidden: true }));
      await q.findByRole("table", { name: l.table, hidden: true });
      await expectNoSeriousA11yViolations(`compare, local changes (${lang})`);
    });
  });
}
