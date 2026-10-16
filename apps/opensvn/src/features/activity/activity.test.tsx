import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, describe, expect, test, vi } from "vitest";
import { ToastProvider } from "../../components/ui/toast";
import type { AppError, OperationHandle, OpProgress, WorkingCopy } from "../../ipc/bindings";
import { applyProgress, newOpState, OP_LOG_LIMIT, type OpState } from "../../ipc/events";
import { mockCalls } from "../../ipc/mock/adapter";
import { queryKeys } from "../../ipc/query-keys";
import { I18nProvider, type Lang } from "../../lib/i18n/i18n";
import { setupMockIpc, stubLayout } from "../../test-utils";
import { lockBanner, ownOperationOn, relatedPaths } from "../wc/own-operation";
import { ActivityPanel } from "./activity-panel";
import { errorText } from "./error-text";
import { formatElapsed, formatLogLine, logText } from "./op-log";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});

beforeEach(() => {
  mockCalls.length = 0;
});

const handle = (over: Partial<OperationHandle> = {}): OperationHandle => ({
  opId: "op1",
  kind: "checkout",
  wcId: null,
  title: "Checkout REPO",
  rootPath: "/home/me/src/REPO",
  background: false,
  ...over,
});

const progress = (over: Partial<OpProgress> = {}): OpProgress => ({
  opId: "op1",
  phase: "checkout",
  current: 3,
  total: null,
  bytesDone: 2048,
  bytesTotal: null,
  currentPath: "src/lib.rs",
  stage: "fetching",
  entries: [
    { action: "added", path: "" },
    { action: "added", path: "src" },
    { action: "added", path: "src/lib.rs" },
  ],
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
  ...over,
});

const transportError = (message: string, code: AppError["code"] = "network"): AppError => ({
  code,
  message,
  detail: null,
  path: null,
  retryable: true,
  actions: ["retry"],
});

const OWNER_TEXT =
  "HTTP request failed: error decoding response body: request or response body error: error reading a body from connection: peer closed connection without sending TLS close_notify";

function renderPanel(ops: OpState[], lang: Lang = "en") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  qc.setQueryData(queryKeys.ops, Object.fromEntries(ops.map((o) => [o.handle.opId, o])));
  const ui = (list: OpState[]) => (
    <QueryClientProvider client={qc}>
      <I18nProvider lang={lang}>
        <ToastProvider>
          <ActivityPanel ops={list} onCancel={() => {}} />
        </ToastProvider>
      </I18nProvider>
    </QueryClientProvider>
  );
  const r = render(ui(ops));
  return { ...r, qc, rerender: (list: OpState[]) => r.rerender(ui(list)) };
}

describe("error mapping", () => {
  test("the owner's raw transport text becomes one short sentence, the raw text a detail", () => {
    const t = errorText(transportError(OWNER_TEXT));
    expect(t.key).toBe("errConnectionClosed");
    expect(t.detail).toContain("peer closed connection");
    expect(t.detail).toContain("(network)");
  });

  test.each([
    ["HTTP request failed: operation timed out", "timeout", "errTimeout"],
    [
      "HTTP request failed: error sending request: tcp connect error: Connection refused (os error 111)",
      "network",
      "errRefused",
    ],
    [
      "HTTP request failed: error sending request: dns error: failed to lookup address information",
      "network",
      "errHostNotFound",
    ],
    [
      "HTTP request failed: error sending request: invalid peer certificate: UnknownIssuer",
      "network",
      "errTls",
    ],
    ["HTTP request failed: something new", "network", "errNetwork"],
    ["Connection reset", "network", "errConnectionClosed"],
  ] as const)("%s → %s", (message, code, key) => {
    expect(errorText(transportError(message, code)).key).toBe(key);
  });

  test("a write or commit cut after it was sent says its outcome is unknown", () => {
    const lock = errorText({
      code: "network",
      message:
        "the connection broke while the lock request was being sent; it may or may not have reached the server. Refresh the lock state before trying again",
      detail: OWNER_TEXT,
    });
    expect(lock.key).toBe("errWriteUnsure");
    expect(lock.detail).toContain("peer closed connection");
    const commit = errorText({
      code: "network",
      message:
        "the commit may or may not have been applied: the result of merging transaction '5-a' is unknown, so the transaction was kept; check the repository's youngest revision before retrying",
      detail: OWNER_TEXT,
    });
    expect(commit.key).toBe("errCommitUnsure");
  });

  test("a switch refused for unrelated history gets the short text that names the way out", () => {
    const t = errorText({
      code: "unrelated-resources",
      message: "'https://svn.example.test/repo/other' shares no common ancestry with '/w/wc'",
      detail:
        "E195012: 'https://svn.example.test/repo/other' shares no common ancestry with '/w/wc'",
    });
    expect(t.key).toBe("errUnrelated");
    expect(t.detail).toContain("E195012");
    expect(t.detail).toContain("(unrelated-resources)");
  });

  test("other errors keep the backend's message and detail", () => {
    const t = errorText({ code: "out-of-date", message: "File is out of date", detail: "E160028" });
    expect(t).toEqual({ key: null, message: "File is out of date", detail: "E160028" });
  });

  test("an error row shows the short text, expands to the full text and the details", async () => {
    const op: OpState = {
      ...newOpState(handle({ kind: "update", title: "Update REPO" }), 0),
      state: "failed",
      error: OWNER_TEXT,
      appError: transportError(OWNER_TEXT),
      endedAt: 1000,
    };
    renderPanel([op], "vi");
    expect(screen.getByText("Update REPO: Máy chủ đã đóng kết nối")).toBeTruthy();
    expect(screen.queryByText(/peer closed/)).toBeNull();
    const toggle = screen.getByRole("button", { name: "Chi tiết: Update REPO" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    await userEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    await userEvent.click(screen.getByText("Chi tiết", { selector: "summary" }));
    expect(screen.getByText(/peer closed connection/)).toBeTruthy();
  });
});

describe("progress state", () => {
  test("the log keeps the newest lines and counts what it dropped", () => {
    let op = newOpState(handle(), 0);
    const many = Array.from({ length: OP_LOG_LIMIT + 10 }, (_, i) => ({
      action: "added" as const,
      path: `f${i}`,
    }));
    op = applyProgress(op, progress({ entries: many, entriesDropped: 5 }), 10);
    expect(op.log).toHaveLength(OP_LOG_LIMIT);
    expect(op.log.at(-1)?.path).toBe(`f${OP_LOG_LIMIT + 9}`);
    expect(op.logDropped).toBe(15);
  });

  test("the rate is measured over time and smoothed", () => {
    let op = newOpState(handle(), 0);
    op = applyProgress(op, progress({ bytesDone: 1000 }), 1000);
    expect(op.rate).toBe(1000);
    // Inside the window: no new sample.
    op = applyProgress(op, progress({ bytesDone: 5000 }), 1200);
    expect(op.rate).toBe(1000);
    op = applyProgress(op, progress({ bytesDone: 3000 }), 2000);
    expect(op.rate).toBeGreaterThan(1000);
  });

  test("log lines read like svn output", () => {
    expect(formatLogLine({ action: "added", path: "src/lib.rs" })).toBe("A    src/lib.rs");
    expect(formatLogLine({ action: "merged", path: "" })).toBe("G    .");
    expect(formatLogLine({ action: "tree-conflicted", path: "x" })).toBe("C    x");
    expect(formatElapsed(65_000)).toBe("1:05");
    expect(formatElapsed(3_725_000)).toBe("1:02:05");
  });
});

describe("details panel", () => {
  test("a running row opens the details: stage, path, files, bytes, rate, elapsed and the log", async () => {
    vi.spyOn(Date, "now").mockReturnValue(61_000);
    const op = applyProgress(newOpState(handle(), 0), progress(), 1000);
    renderPanel([op]);
    const open = screen.getByRole("button", { name: "Show details of Checkout REPO" });
    open.focus();
    await userEvent.keyboard("{Enter}");
    const dlg = screen.getByRole("dialog", { name: "Checkout REPO" });
    expect(within(dlg).getByTestId("op-stage").textContent).toBe("Fetching");
    expect(within(dlg).getAllByText("src/lib.rs").length).toBeGreaterThan(0);
    expect(within(dlg).getByText("3")).toBeTruthy();
    expect(within(dlg).getByText("2.0 KB")).toBeTruthy();
    expect(within(dlg).getByText("2.0 KB/s")).toBeTruthy();
    expect(within(dlg).getByText("1:01")).toBeTruthy();
    expect(within(dlg).getByRole("status").textContent).toBe("Checkout REPO: Fetching");
    const log = within(dlg).getByRole("listbox", { name: "Recent paths" });
    const rows = within(log).getAllByRole("option");
    expect(rows.map((r) => r.textContent)).toEqual(["A.", "Asrc", "Asrc/lib.rs"]);
    // Keyboard moves through the log.
    log.focus();
    await userEvent.keyboard("{End}");
    expect(log.getAttribute("aria-activedescendant")).toBe(rows[2]?.id);
    expect(within(dlg).queryByRole("region", { name: "Summary" })).toBeNull();
    vi.restoreAllMocks();
  });

  test("a finished checkout keeps its log and shows the summary, copy and open folder", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
    let op = applyProgress(
      newOpState(handle(), 0),
      progress({
        counts: {
          added: 3,
          deleted: 0,
          updated: 1,
          merged: 0,
          conflicted: 2,
          treeConflicted: 0,
          existed: 0,
          restored: 0,
          skipped: 0,
        },
      }),
      1000,
    );
    op = {
      ...op,
      state: "done",
      endedAt: 5000,
      result: { kind: "checkedOut", wcId: "wc-new", newRev: 42 },
      warnings: ["ext/lib"],
    };
    renderPanel([op]);
    await userEvent.click(screen.getByRole("button", { name: "Show details of Checkout REPO" }));
    const dlg = screen.getByRole("dialog", { name: "Checkout REPO" });
    expect(within(dlg).getByTestId("op-stage").textContent).toBe("Done");
    const counts = within(dlg).getByTestId("op-counts");
    expect(counts.textContent).toContain("Added: 3");
    expect(counts.textContent).toContain("Updated: 1");
    expect(counts.textContent).toContain("Conflicts: 2");
    expect(within(dlg).getByText("At revision 42")).toBeTruthy();
    expect(within(dlg).getByText("ext/lib")).toBeTruthy();
    expect(within(dlg).getAllByRole("option")).toHaveLength(3);
    expect(within(dlg).getByText("0:05")).toBeTruthy();

    await userEvent.click(within(dlg).getByRole("button", { name: "Copy log" }));
    const copied = writeText.mock.calls[0]?.[0] as string;
    expect(copied).toContain("A    src/lib.rs");
    expect(copied).toContain("Conflicts: 2");
    expect(copied).toContain("r42");
    expect(within(dlg).getByRole("button", { name: "Log copied" })).toBeTruthy();

    await userEvent.click(within(dlg).getByRole("button", { name: "Open folder" }));
    await act(async () => {});
    expect(mockCalls.some((c) => c.name === "revealInOs" && c.args[0] === "wc-new")).toBe(true);

    await userEvent.click(within(dlg).getByRole("button", { name: "Close" }));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  test("copy log uses the whole text of a failed operation too", () => {
    const op: OpState = {
      ...newOpState(handle(), 0),
      state: "failed",
      appError: transportError(OWNER_TEXT),
      log: [{ action: "added", path: "a" }],
    };
    const text = logText(op, (k) => k);
    expect(text).toContain("A    a");
    expect(text).toContain("peer closed");
  });
});

describe("background operations", () => {
  test("a failed background check is a small indicator, not a red row", async () => {
    const bg: OpState = {
      ...newOpState(
        handle({
          opId: "bg",
          kind: "status-remote",
          title: "Check REPO for updates",
          background: true,
        }),
        0,
      ),
      state: "failed",
      error: OWNER_TEXT,
      appError: transportError(OWNER_TEXT),
    };
    const { rerender, qc } = renderPanel([bg]);
    expect(screen.queryByRole("progressbar")).toBeNull();
    const indicator = screen.getByRole("button", { name: /A background check failed/ });
    await userEvent.click(indicator);
    expect(
      screen.getByText("Check REPO for updates: Connection was closed by the server"),
    ).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(qc.getQueryData<Record<string, OpState>>(queryKeys.ops)).toEqual({});
    rerender([]);
    expect(screen.getByText("No recent activity.")).toBeTruthy();
  });
});

describe("lock banner", () => {
  const wc: Pick<WorkingCopy, "id" | "rootPath"> = { id: "wc1", rootPath: "/home/me/src/REPO/" };
  const running = (over: Partial<OperationHandle>) => newOpState(handle(over), 0);

  test("a checkout of ours writing the folder replaces the stale-lock banner", () => {
    const op = running({});
    expect(ownOperationOn([op], wc)).toBe(op);
    expect(lockBanner([op], wc, true)).toEqual({ kind: "in-progress", op });
    // Even before any lock was reported, a checkout filling the folder says so.
    expect(lockBanner([op], wc, false)).toEqual({ kind: "in-progress", op });
  });

  test("the stale-lock banner shows only when no operation of ours holds the working copy", () => {
    expect(lockBanner([], wc, true)).toEqual({ kind: "stale" });
    expect(lockBanner([], wc, false)).toBeNull();
    const done = { ...running({}), state: "done" as const };
    expect(lockBanner([done], wc, true)).toEqual({ kind: "stale" });
    const elsewhere = running({ rootPath: "/home/me/src/OTHER" });
    expect(lockBanner([elsewhere], wc, true)).toEqual({ kind: "stale" });
    // A background or read-only operation never holds the lock.
    const check = running({ kind: "status-remote", wcId: "wc1", rootPath: null });
    expect(lockBanner([check], wc, true)).toEqual({ kind: "stale" });
    const bg = running({ kind: "update", wcId: "wc1", background: true });
    expect(lockBanner([bg], wc, true)).toEqual({ kind: "stale" });
  });

  test("an update of ours matched by id holds the lock; no banner unless one was reported", () => {
    const update = running({ kind: "update", wcId: "wc1", rootPath: null });
    expect(lockBanner([update], wc, false)).toBeNull();
    expect(lockBanner([update], wc, true)).toEqual({ kind: "in-progress", op: update });
  });

  test("paths match through trailing separators, nesting and Windows case", () => {
    expect(relatedPaths("/a/b/", "/a/b")).toBe(true);
    expect(relatedPaths("/a/b/c", "/a/b")).toBe(true);
    expect(relatedPaths("/a/bc", "/a/b")).toBe(false);
    expect(relatedPaths("C:\\Work\\Repo", "c:/work/repo/")).toBe(true);
  });
});

describe("waiting for a lost connection", () => {
  const waiting = (retryInSecs: number, now: number): OpState =>
    applyProgress(
      newOpState(handle(), 0),
      progress({
        stage: "waiting-for-connection",
        entries: [],
        connectionWait: { retryInSecs, waitedSecs: 64, maxWaitSecs: 600, attempt: 3 },
      }),
      now,
    );

  test("the next attempt is due after the announced delay; another stage clears it", () => {
    const op = waiting(8, 1000);
    expect(op.retryAt).toBe(9000);
    expect(
      applyProgress(op, progress({ stage: "fetching", entries: [] }), 2000).retryAt,
    ).toBeNull();
  });

  for (const [lang, text, retry, cancel] of [
    ["en", "Connection lost, retrying in 8s…", "Retry now", "Cancel"],
    ["vi", "Mất kết nối, thử lại sau 8 giây…", "Thử lại ngay", "Hủy"],
  ] as const) {
    test(`${lang}: the panel counts down, retries now and cancels`, async () => {
      const cancelled: string[] = [];
      const qc = new QueryClient();
      render(
        <QueryClientProvider client={qc}>
          <I18nProvider lang={lang}>
            <ToastProvider>
              <ActivityPanel ops={[waiting(8, Date.now())]} onCancel={(id) => cancelled.push(id)} />
            </ToastProvider>
          </I18nProvider>
        </QueryClientProvider>,
      );
      const notice = screen.getByTestId("connection-wait");
      expect(within(notice).getByRole("status").textContent).toBe(text);
      await userEvent.click(within(notice).getByRole("button", { name: retry }));
      expect(mockCalls.filter((c) => c.name === "opRetryNow").map((c) => c.args)).toEqual([
        ["op1"],
      ]);
      await userEvent.click(within(notice).getByRole("button", { name: cancel }));
      expect(cancelled).toEqual(["op1"]);
    });
  }

  test("once the countdown is over it says it is retrying", () => {
    renderPanel([waiting(0, Date.now())]);
    expect(screen.getByTestId("connection-wait").textContent).toContain(
      "Connection lost, retrying…",
    );
  });

  test("an operation that is not waiting shows no notice", () => {
    renderPanel([applyProgress(newOpState(handle(), 0), progress(), 1000)]);
    expect(screen.queryByTestId("connection-wait")).toBeNull();
  });
});
