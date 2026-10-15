import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import type { DiffTarget, FileDiff } from "../../../ipc/bindings";
import { mockCalls } from "../../../ipc/mock/adapter";
import { renderWithQuery, setupMockIpc } from "../../../test-utils";
import { displayPath } from "../wc-model";
import { DiffBody } from "./diff-view";

vi.mock("./diff-merge-editor", () => ({
  DiffMergeEditor: ({ left, right }: { left: string; right: string }) => (
    <div data-testid="editor">{`${left}|${right}`}</div>
  ),
}));

const base: FileDiff = {
  path: "logs/huge.log",
  binary: false,
  mime: null,
  left: null,
  right: null,
  hunks: null,
  propChanges: [],
  image: null,
  tooLarge: null,
  tooComplex: null,
  office: null,
};

beforeAll(async () => {
  await setupMockIpc("en");
}, 20_000);

function body(diff: FileDiff, target?: DiffTarget) {
  const onLoadFull = vi.fn();
  const onOpenExternal = vi.fn();
  renderWithQuery(
    <DiffBody
      diff={diff}
      file={null}
      mode="split"
      path={diff.path}
      onLoadFull={onLoadFull}
      onOpenExternal={onOpenExternal}
      onOpenDefault={vi.fn()}
      target={target}
    />,
  );
  return { onLoadFull, onOpenExternal };
}

test("too large without hunks shows the large-file banner, never 'no content change'", () => {
  // What the real engine sends for a side over 5 MB: no sides, no hunks.
  const { onLoadFull, onOpenExternal } = body({ ...base, tooLarge: true });
  expect(screen.getByText("Large file")).toBeTruthy();
  expect(screen.getByText(/larger than 5 MB and was not loaded/)).toBeTruthy();
  expect(screen.queryByText("No content change")).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "Load full diff" }));
  expect(onLoadFull).toHaveBeenCalledOnce();
  fireEvent.click(screen.getByRole("button", { name: "Open in external tool" }));
  expect(onOpenExternal).toHaveBeenCalledOnce();
});

test("too large with property rows keeps the banner and lists the properties", () => {
  body({
    ...base,
    tooLarge: true,
    hunks: [],
    propChanges: [{ name: "svn:eol-style", before: null, after: "native" }],
  });
  expect(screen.getByText("Large file")).toBeTruthy();
  expect(screen.getByText("svn:eol-style")).toBeTruthy();
});

test("too large with hunks shows the hunks under the banner", () => {
  body({
    ...base,
    tooLarge: true,
    hunks: [{ oldStart: 2, oldLines: 1, newStart: 2, newLines: 1, lines: ["-old", "+new"] }],
  });
  expect(screen.getByText("Large file")).toBeTruthy();
  expect(screen.getByText(/Showing only the changed hunks/)).toBeTruthy();
  expect(screen.getByText("+new")).toBeTruthy();
});

test("a forced load with both sides shows the editor even when still flagged", () => {
  body({
    ...base,
    tooLarge: true,
    left: { content: "a\n", encoding: "utf-8", eol: "LF", label: "base" },
    right: { content: "b\n", encoding: "utf-8", eol: "LF", label: "working" },
  });
  expect(screen.queryByText("Large file")).toBeNull();
});

test("the working copy root (empty path) gets a readable label", () => {
  expect(displayPath("", "proj (working copy root)")).toBe("proj (working copy root)");
  expect(displayPath("src/a.ts", "x")).toBe("src/a.ts");
});

test("an Office diff says it compares extracted text, or why there is none", () => {
  body({
    ...base,
    path: "docs/contract.docx",
    left: { content: "Điều 1\n", encoding: "UTF-8", eol: "LF", label: "base" },
    right: { content: "Điều 2\n", encoding: "UTF-8", eol: "LF", label: "working" },
    office: { kind: "word", extracted: true, reason: null },
  });
  expect(screen.getByRole("note").textContent).toMatch(/text extracted from both Word documents/);
  expect(screen.getByTestId("editor").textContent).toBe("Điều 1\n|Điều 2\n");
});

test("an Office file without text explains the reason and keeps the binary view", () => {
  body({
    ...base,
    path: "old.xls",
    binary: true,
    mime: "application/octet-stream",
    office: {
      kind: "excel",
      extracted: false,
      reason: "Excel 97-2003 workbooks (.xls) are not supported",
    },
  });
  expect(screen.getByRole("note").textContent).toMatch(/Excel 97-2003.*external diff tool/);
  expect(screen.getByText("Binary file")).toBeTruthy();
});

test("an Office diff offers to open each version that exists in its default app", async () => {
  const target: DiffTarget = { kind: "rev", url: "https://svn.example.com/r/a.docx", rev: 12 };
  body(
    {
      ...base,
      path: "a.docx",
      left: null,
      right: { content: "x\n", encoding: "UTF-8", eol: "LF", label: "a.docx@r12" },
      office: { kind: "word", extracted: true, reason: null },
    },
    target,
  );
  // The file was added in r12: there is no r11 to open.
  expect(screen.queryByRole("button", { name: "Open r11" })).toBeNull();
  mockCalls.length = 0;
  fireEvent.click(screen.getByRole("button", { name: "Open r12" }));
  await waitFor(() =>
    expect(mockCalls.find((c) => c.name === "diffOpenVersion")?.args).toEqual([target, "right"]),
  );
});

test("legacy Office files offer no open buttons and point to the external tool", () => {
  body(
    {
      ...base,
      path: "old.doc",
      binary: true,
      office: {
        kind: "word",
        extracted: false,
        reason: "Word 97-2003 documents (.doc) are not supported",
      },
    },
    { kind: "wc", wcId: "w", path: "old.doc" },
  );
  expect(screen.queryByRole("button", { name: /^Open (r\d|base|working)/ })).toBeNull();
  expect(screen.getByRole("note").textContent).toMatch(/external diff tool/);
});
