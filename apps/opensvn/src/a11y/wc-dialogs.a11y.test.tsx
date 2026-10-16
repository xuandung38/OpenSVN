import { act, configure, screen, waitFor, within } from "@testing-library/react";
import { useEffect } from "react";
import { beforeAll, beforeEach, describe, expect, test, vi } from "vitest";
import { WcDialogHost } from "../features/wc/dialogs/wc-dialog-host";
import { useWcSession, type WcDialog, WcSessionProvider } from "../features/wc/wc-session";
import { MOCK_WC_ID } from "../ipc/mock/data";
import { resetWcMock } from "../ipc/mock/wc-mock-commands";
import { useStatus, useWorkingCopies } from "../ipc/queries";
import type { Lang } from "../lib/i18n/i18n";
import { expectNoSeriousA11yViolations, prepareDocument } from "../test-axe";
import { renderApp, setupMockIpc, stubLayout } from "../test-utils";

// Automated accessibility check (axe-core) of every dialog the working-copy session can open
// (WcDialogHost), each in its open state, in both UI languages.

vi.mock("../features/wc/diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

vi.setConfig({ testTimeout: 180_000 });
configure({ asyncUtilTimeout: 60_000 });

beforeAll(() => stubLayout());
beforeEach(() => resetWcMock());

/** Opens `dialog` once the working copy and its status are loaded (dialogs read both). */
function OpenOnLoad({ dialog }: { dialog: WcDialog }) {
  const { wc, files, openDialog } = useWcSession();
  const ready = wc !== null && files.length > 0;
  useEffect(() => {
    if (ready) openDialog(dialog);
  }, [ready, dialog, openDialog]);
  return null;
}

function Harness({ dialog }: { dialog: WcDialog }) {
  const wcs = useWorkingCopies();
  const wc = wcs.data?.find((w) => w.id === MOCK_WC_ID) ?? null;
  const status = useStatus(wc?.id ?? null);
  return (
    <WcSessionProvider wc={wc} files={status.data ?? []}>
      <OpenOnLoad dialog={dialog} />
      <WcDialogHost />
    </WcSessionProvider>
  );
}

const CASES: [string, WcDialog][] = [
  ["checkout", { kind: "checkout" }],
  ["discard selected", { kind: "discard", paths: ["src/engine/renderer.ts"], all: false }],
  ["discard all", { kind: "discard", paths: [], all: true }],
  ["ignore", { kind: "ignore", path: "tools/readme-renamed.md" }],
  ["rename", { kind: "rename", path: "src/engine/renderer.ts" }],
  ["delete", { kind: "delete", paths: ["src/engine/renderer.ts", "data/big-table.csv"] }],
  ["lock", { kind: "lock", mode: "lock", paths: ["assets/audio/theme.ogg"] }],
  [
    "lock held by someone else",
    {
      kind: "lock",
      mode: "lock",
      paths: ["assets/audio/theme.ogg"],
      conflicts: [
        {
          path: "assets/audio/theme.ogg",
          owner: "binh",
          comment: "mixing",
          createdAt: "2026-10-12T08:00:00Z",
        },
      ],
    },
  ],
  ["unlock", { kind: "lock", mode: "unlock", paths: ["docs/spec.docx"] }],
  ["changelist", { kind: "changelist", paths: ["src/engine/renderer.ts"] }],
  ["update to revision", { kind: "update-rev" }],
  ["switch", { kind: "switch" }],
  ["cleanup", { kind: "cleanup" }],
  ["choose folders", { kind: "sparse" }],
  ["cleanup (another program's lock)", { kind: "cleanup", foreignLock: true }],
  ["properties (root)", { kind: "properties", paths: [""] }],
  [
    "properties (several items)",
    { kind: "properties", paths: ["assets/audio/theme.ogg", "src/engine/renderer.ts"] },
  ],
  ["repository layout", { kind: "layout" }],
  ["rename repository", { kind: "repo-rename", wcId: MOCK_WC_ID, name: "game-client" }],
  ["remove repository", { kind: "repo-remove", wcId: MOCK_WC_ID, name: "game-client" }],
  ["conflict resolver (text conflict)", { kind: "resolver", path: "src/engine/physics.ts" }],
  ["conflict resolver (tree conflict)", { kind: "resolver", path: "art" }],
];

for (const lang of ["en", "vi"] as Lang[]) {
  describe(`working-copy dialogs (${lang})`, () => {
    beforeAll(async () => {
      prepareDocument(lang);
      await setupMockIpc(lang);
    });

    for (const [name, dialog] of CASES) {
      test(name, async () => {
        renderApp(<Harness dialog={dialog} />);
        const dlg = await screen.findByRole("dialog", { hidden: true });
        // Guard against scanning an empty shell: an open dialog always has its own buttons.
        await waitFor(() =>
          expect(within(dlg).getAllByRole("button", { hidden: true }).length).toBeGreaterThan(0),
        );
        // Let the dialog's own queries (recent URLs, branches, conflict text...) settle.
        await act(() => new Promise((r) => setTimeout(r, 150)));
        await expectNoSeriousA11yViolations(`${name} (${lang})`);
      });
    }
  });
}
