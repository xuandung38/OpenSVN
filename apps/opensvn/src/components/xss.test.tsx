import { render, screen } from "@testing-library/react";
import { act } from "react";
import { describe, expect, test } from "vitest";
import { FileRow } from "./list/file-row";
import { RevisionRow } from "./list/revision-row";
import { ActivityRow } from "./ui/activity-row";
import { Banner } from "./ui/banner";
import { Dialog } from "./ui/dialog";
import { EmptyState } from "./ui/empty-state";
import { SafeText } from "./ui/safe-text";
import { ToastProvider, useToast } from "./ui/toast";

const SCRIPT = "<script>window.__pwned=1</script>";
const IMG = '<img src=x onerror="window.__pwned=1">';

/** No element created from the hostile string may exist; the text itself must be visible. */
function expectInert(container: HTMLElement, payload: string) {
  expect(container.querySelector("script")).toBeNull();
  expect(container.querySelector("img")).toBeNull();
  expect(container.textContent).toContain(payload);
  expect((window as unknown as { __pwned?: number }).__pwned).toBeUndefined();
}

describe("untrusted strings render as text", () => {
  test.each([SCRIPT, IMG])("SafeText %s", (p) => {
    const { container } = render(<SafeText>{p}</SafeText>);
    expectInert(container, p);
  });

  test.each([SCRIPT, IMG])("FileRow path %s", (p) => {
    const { container } = render(<FileRow path={`dir/${p}`} status="M" />);
    // Path is split into dir/name spans; the concatenation still shows the literal string.
    expect(container.querySelector("script")).toBeNull();
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain(p);
  });

  test.each([SCRIPT, IMG])("RevisionRow message and author %s", (p) => {
    const { container } = render(<RevisionRow rev={1} message={p} author={p} when="now" />);
    expect(container.querySelector("script")).toBeNull();
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain(p);
  });

  test("Banner, EmptyState, ActivityRow", () => {
    const { container } = render(
      <>
        <Banner title={IMG}>{SCRIPT}</Banner>
        <EmptyState title={SCRIPT} description={IMG} />
        <ActivityRow title={IMG} percent={10} state="running" />
      </>,
    );
    expect(container.querySelector("script")).toBeNull();
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain(SCRIPT);
    expect(container.textContent).toContain(IMG);
  });

  test("Dialog title", () => {
    render(
      <Dialog open onClose={() => undefined} title={SCRIPT}>
        body
      </Dialog>,
    );
    expect(document.querySelector("script")).toBeNull();
    expect(screen.getByText(SCRIPT)).toBeTruthy();
  });

  test("Toast", async () => {
    function Fire() {
      const t = useToast();
      return (
        <button type="button" onClick={() => t.show(IMG)}>
          go
        </button>
      );
    }
    const { container } = render(
      <ToastProvider>
        <Fire />
      </ToastProvider>,
    );
    await act(async () => screen.getByText("go").click());
    expectInert(container, IMG);
  });
});
