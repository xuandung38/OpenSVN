import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, expect, test, vi } from "vitest";
import type { DiffTarget, FileDiff } from "../../../ipc/bindings";
import { mockCalls } from "../../../ipc/mock/adapter";
import { expectNoSeriousA11yViolations } from "../../../test-axe";
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

const png = (fill: string) =>
  `data:image/svg+xml,${encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg"><rect fill="${fill}"/></svg>`)}`;

test("images: side by side, onion skin with an opacity slider, swipe; all by keyboard", async () => {
  const user = userEvent.setup();
  const { container } = renderWithQuery(
    <DiffBody
      diff={{
        ...base,
        binary: true,
        mime: "image/png",
        image: { leftUrl: png("red"), rightUrl: png("blue") },
      }}
      file={null}
      mode="split"
      path="hero.png"
      onLoadFull={vi.fn()}
      onOpenExternal={vi.fn()}
      onOpenDefault={vi.fn()}
    />,
  );
  const modes = screen.getByRole("group", { name: "Image comparison" });
  const side = within(modes).getByRole("button", { name: "Side by side" });
  expect(side.getAttribute("aria-pressed")).toBe("true");
  expect(screen.getAllByRole("img")).toHaveLength(2);
  await expectNoSeriousA11yViolations("image side by side", container);

  // Keyboard only: Tab to the mode buttons, Enter to pick onion skin, arrows on the slider.
  side.focus();
  await user.tab();
  await user.keyboard("{Enter}");
  expect(
    within(modes).getByRole("button", { name: "Onion skin" }).getAttribute("aria-pressed"),
  ).toBe("true");
  const opacity = screen.getByRole("slider", { name: "Opacity of the new image" });
  const overlay = () => container.querySelector<HTMLImageElement>("img[data-overlay]");
  expect(overlay()?.style.opacity).toBe("0.5");
  // The slider is the next tab stop after the mode buttons. jsdom does not step a range input on
  // arrow keys (browsers do natively; `pnpm test:browser` covers that), so set its value.
  await user.tab();
  await user.tab();
  expect(document.activeElement).toBe(opacity);
  fireEvent.change(opacity, { target: { value: "60" } });
  expect(overlay()?.style.opacity).toBe("0.6");
  expect(opacity.getAttribute("aria-valuetext")).toBe("60%");
  await expectNoSeriousA11yViolations("image onion skin", container);

  await user.click(within(modes).getByRole("button", { name: "Swipe" }));
  const swipe = screen.getByRole("slider", { name: "Swipe position" });
  fireEvent.change(swipe, { target: { value: "0" } });
  expect(overlay()?.style.clipPath).toBe("inset(0 100% 0 0)");
  fireEvent.change(swipe, { target: { value: "100" } });
  expect(overlay()?.style.clipPath).toBe("inset(0 0% 0 0)");
  await expectNoSeriousA11yViolations("image swipe", container);
});

test("an image with one side only offers side by side alone", () => {
  renderWithQuery(
    <DiffBody
      diff={{
        ...base,
        binary: true,
        mime: "image/png",
        image: { leftUrl: null, rightUrl: png("blue") },
      }}
      file={null}
      mode="split"
      path="new.png"
      onLoadFull={vi.fn()}
      onOpenExternal={vi.fn()}
      onOpenDefault={vi.fn()}
    />,
  );
  expect(screen.queryByRole("group", { name: "Image comparison" })).toBeNull();
  expect(screen.getByText(/only side by side is available/)).toBeTruthy();
  expect(screen.getAllByRole("img")).toHaveLength(1);
});
