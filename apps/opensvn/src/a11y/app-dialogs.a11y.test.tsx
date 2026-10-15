import { act, configure, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { beforeAll, describe, expect, test, vi } from "vitest";
import { ActivityPanel } from "../features/activity/activity-panel";
import { OpDetails } from "../features/activity/op-details";
import type {
  CertFailure,
  CertPromptInfo,
  CredentialsPromptInfo,
} from "../features/remote/auth/auth-model";
import { SignInDialog } from "../features/remote/auth/sign-in-dialog";
import { TrustCertDialog } from "../features/remote/auth/trust-cert-dialog";
import { CreateBranchTagDialog } from "../features/remote/branch/create-branch-tag-dialog";
import { RemoteEditDialog } from "../features/remote/browse/remote-edit-dialog";
import { RevisionDiffDialog } from "../features/remote/history/revision-diff-dialog";
import { LockDialog } from "../features/remote/lock/lock-dialog";
import { SettingsSheet } from "../features/remote/settings/settings-sheet";
import { ShortcutsDialog } from "../features/wc/shortcuts-dialog";
import type { AppError, OperationHandle } from "../ipc/bindings";
import { applyProgress, newOpState, type OpState } from "../ipc/events";
import { mockWorkingCopies } from "../ipc/mock/data";
import { MOCK_REPO_ROOT } from "../ipc/mock/remote-data";
import type { Lang } from "../lib/i18n/i18n";
import { expectNoSeriousA11yViolations, prepareDocument } from "../test-axe";
import { renderApp, setupMockIpc, stubLayout } from "../test-utils";

// Automated accessibility check (axe-core) of the dialogs outside the working-copy session:
// Settings (every page), keyboard shortcuts, create branch/tag, the auth prompts, remote lock and
// edit dialogs, revision diff, operation details and the Activity panel, in both UI languages.

vi.mock("../features/wc/diff/diff-merge-editor", () => ({
  DiffMergeEditor: () => <div data-testid="editor" />,
}));

vi.setConfig({ testTimeout: 180_000 });
configure({ asyncUtilTimeout: 60_000 });

beforeAll(() => stubLayout());

const noop = () => undefined;
const wc = mockWorkingCopies[0] ?? (undefined as never);
const FILE = `${MOCK_REPO_ROOT}/trunk/assets/models/crate.fbx`;

const creds = (over: Partial<CredentialsPromptInfo> = {}): CredentialsPromptInfo => ({
  kind: "credentials",
  requestId: "r1",
  realm: "<https://svn.example.com:443> Repo",
  url: "https://svn.example.com/repos/x",
  reason: "initial",
  attempt: 0,
  offered: ["basic"],
  ...over,
});
const cert = (failure: CertFailure): CertPromptInfo => ({
  kind: "certificate",
  requestId: "c1",
  realm: "svn.example.com:443",
  url: "https://svn.example.com/repos/x",
  cert: {
    host: "svn.example.com",
    subject: "CN=svn.example.com",
    issuer: "CN=Corp CA",
    notBefore: "2026-01-01",
    notAfter: "2027-01-01",
    sha256: "AA:BB",
  },
  failure,
});

const handle = (over: Partial<OperationHandle> = {}): OperationHandle => ({
  opId: "op1",
  kind: "checkout",
  wcId: null,
  title: "Checkout game",
  rootPath: "/Users/dev/work/game",
  background: false,
  ...over,
});
const failure: AppError = {
  code: "network",
  message: "peer closed connection without sending TLS close_notify",
  detail: null,
  path: null,
  retryable: true,
  actions: ["retry"],
};
const running = applyProgress(newOpState(handle(), 0), {
  opId: "op1",
  phase: "checkout",
  current: 3,
  total: 10,
  bytesDone: 2048,
  bytesTotal: null,
  currentPath: "src/lib.rs",
  stage: "fetching",
  entries: [
    { action: "added", path: "src" },
    { action: "added", path: "src/lib.rs" },
  ],
  entriesDropped: 0,
  counts: {
    added: 2,
    deleted: 0,
    updated: 0,
    merged: 0,
    conflicted: 0,
    treeConflicted: 0,
    existed: 0,
    restored: 0,
    skipped: 0,
  },
});
const failed: OpState = {
  ...newOpState(handle({ opId: "op2", kind: "update", title: "Update game" }), 0),
  state: "failed",
  error: failure.message,
  appError: failure,
  endedAt: 1000,
};
const done: OpState = {
  ...newOpState(handle({ opId: "op3", kind: "commit", title: "Commit game" }), 0),
  state: "done",
  result: { kind: "newRev", newRev: 48214 },
  endedAt: 2000,
};

const branchDialog = (kind: "branch" | "tag" | "location") => (
  <CreateBranchTagDialog
    kind={kind}
    sourceUrl={wc.url}
    sourceName={wc.branch?.name ?? wc.url}
    repoRootUrl={wc.repoRootUrl}
    repoUuid={wc.repoUuid}
    currentRefUrl={wc.branch?.url ?? null}
    wcId={wc.id}
    onClose={noop}
  />
);

const DIALOGS: [string, () => ReactElement][] = [
  ["keyboard shortcuts", () => <ShortcutsDialog open onClose={noop} />],
  ["create branch", () => branchDialog("branch")],
  ["create tag", () => branchDialog("tag")],
  ["copy to location", () => branchDialog("location")],
  ["sign in", () => <SignInDialog prompt={creds()} onSubmit={noop} onCancel={noop} />],
  [
    "sign in with Windows account offered",
    () => (
      <SignInDialog
        prompt={creds({ offered: ["negotiate", "basic"] })}
        onSubmit={noop}
        onNegotiate={noop}
        onCancel={noop}
      />
    ),
  ],
  [
    "sign in (rejected)",
    () => (
      <SignInDialog
        prompt={creds({ reason: "rejected", attempt: 2, offered: ["basic", "negotiate"] })}
        onSubmit={noop}
        onCancel={noop}
      />
    ),
  ],
  [
    "sign in (attempts exhausted)",
    () => (
      <SignInDialog
        prompt={creds({ reason: "rejected", attempt: 3 })}
        onSubmit={noop}
        onCancel={noop}
      />
    ),
  ],
  [
    "trust certificate (self-signed)",
    () => <TrustCertDialog prompt={cert("self-signed")} onDecision={noop} />,
  ],
  [
    "trust certificate (expired)",
    () => <TrustCertDialog prompt={cert("expired")} onDecision={noop} />,
  ],
  [
    "trust certificate (changed)",
    () => (
      <TrustCertDialog
        prompt={cert({ changed: { oldSha256: "AA:BB", newSha256: "CC:DD" } })}
        onDecision={noop}
      />
    ),
  ],
  [
    "remote lock (free file)",
    () => (
      <LockDialog
        mode="lock"
        targets={[{ url: `${MOCK_REPO_ROOT}/trunk/README.md`, lock: null }]}
        onClose={noop}
      />
    ),
  ],
  [
    "remote lock (held by someone else)",
    () => (
      <LockDialog
        mode="lock"
        onClose={noop}
        targets={[
          {
            url: FILE,
            lock: {
              owner: "an",
              comment: "reworking model",
              createdAt: "2026-10-12T08:00:00Z",
              token: null,
              mine: false,
            },
          },
        ]}
      />
    ),
  ],
  [
    "remote unlock",
    () => (
      <LockDialog
        mode="unlock"
        onClose={noop}
        targets={[
          {
            url: FILE,
            lock: {
              owner: "you",
              comment: null,
              createdAt: "2026-10-12T08:00:00Z",
              token: "opaquelocktoken:1",
              mine: true,
            },
          },
        ]}
      />
    ),
  ],
  [
    "new folder",
    () => (
      <RemoteEditDialog
        mode="mkdir"
        targetUrl={`${MOCK_REPO_ROOT}/trunk`}
        repoRoot={MOCK_REPO_ROOT}
        onClose={noop}
      />
    ),
  ],
  [
    "remote rename",
    () => (
      <RemoteEditDialog
        mode="rename"
        targetUrl={`${MOCK_REPO_ROOT}/trunk/README.md`}
        repoRoot={MOCK_REPO_ROOT}
        onClose={noop}
      />
    ),
  ],
  [
    "remote delete",
    () => (
      <RemoteEditDialog
        mode="delete"
        targetUrl={`${MOCK_REPO_ROOT}/trunk/README.md`}
        repoRoot={MOCK_REPO_ROOT}
        onClose={noop}
      />
    ),
  ],
  [
    "revision diff",
    () => (
      <RevisionDiffDialog
        url={`${MOCK_REPO_ROOT}/trunk/src/engine/renderer.ts`}
        rev={48210}
        path="/trunk/src/engine/renderer.ts"
        onClose={noop}
      />
    ),
  ],
  ["operation details (running)", () => <OpDetails op={running} onClose={noop} onCopy={noop} />],
  [
    "operation details (failed)",
    () => <OpDetails op={failed} onClose={noop} onCopy={noop} onOpenFolder={noop} />,
  ],
  [
    "operation details (done)",
    () => <OpDetails op={done} onClose={noop} onCopy={noop} onOpenFolder={noop} />,
  ],
];

/** Let a dialog's own queries (settings, diff, branches...) settle before the scan. */
const settle = () => act(() => new Promise((r) => setTimeout(r, 150)));

for (const lang of ["en", "vi"] as Lang[]) {
  describe(`app dialogs (${lang})`, () => {
    beforeAll(async () => {
      prepareDocument(lang);
      await setupMockIpc(lang);
    });

    for (const [name, ui] of DIALOGS) {
      test(name, async () => {
        renderApp(ui());
        const dlg = await screen.findByRole("dialog", { hidden: true });
        // Guard against scanning an empty shell: an open dialog always has its own buttons.
        await waitFor(() =>
          expect(within(dlg).getAllByRole("button", { hidden: true }).length).toBeGreaterThan(0),
        );
        await settle();
        await expectNoSeriousA11yViolations(`${name} (${lang})`);
      });
    }

    test("settings, every page", async () => {
      const user = userEvent.setup();
      renderApp(<SettingsSheet open onClose={noop} />);
      const sheet = await screen.findByRole("dialog", { hidden: true });
      const nav = within(sheet).getByRole("navigation", { hidden: true });
      const pages = within(nav).getAllByRole("button", { hidden: true });
      for (const page of pages) {
        await user.click(page);
        await settle();
        await expectNoSeriousA11yViolations(`settings: ${page.textContent} (${lang})`);
      }
    });

    test("settings: remove-certificate and clear-cache confirmations", async () => {
      const user = userEvent.setup();
      renderApp(<SettingsSheet open onClose={noop} />);
      const sheet = await screen.findByRole("dialog", { hidden: true });
      const nav = within(sheet).getByRole("navigation", { hidden: true });
      const [, accounts] = within(nav).getAllByRole("button", { hidden: true });
      await user.click(accounts ?? (undefined as never));
      const certs = await within(sheet).findByRole("region", {
        name: lang === "vi" ? "Chứng chỉ đã tin cậy" : "Trusted certificates",
        hidden: true,
      });
      await user.click(
        await within(certs).findByRole("button", { name: /svn\.example\.com:443/, hidden: true }),
      );
      await settle();
      expect(within(certs).getByRole("group", { hidden: true })).toBeTruthy();
      await expectNoSeriousA11yViolations(`settings: remove certificate confirm (${lang})`);
      const pages = within(nav).getAllByRole("button", { hidden: true });
      await user.click(pages.at(-1) ?? (undefined as never));
      await user.click(
        await within(sheet).findByRole("button", {
          name: lang === "vi" ? "Xóa cache" : "Clear cache",
          hidden: true,
        }),
      );
      await settle();
      expect(within(sheet).getByRole("group", { hidden: true })).toBeTruthy();
      await expectNoSeriousA11yViolations(`settings: clear cache confirm (${lang})`);
    });

    test("activity panel with running, failed and finished operations", async () => {
      renderApp(<ActivityPanel ops={[running, failed, done]} onCancel={noop} />);
      await settle();
      await expectNoSeriousA11yViolations(`activity panel (${lang})`);
    });
  });
}
