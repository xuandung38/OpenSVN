import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { expect, test, vi } from "vitest";
import { clampSidebar, ResizableSidebar } from "./layout/resizable-sidebar";
import { Button } from "./ui/button";
import { Dialog } from "./ui/dialog";
import { SegmentedControl } from "./ui/segmented-control";
import { StatusGlyph } from "./ui/status-glyph";

test("status glyph has an accessible name (not colour-only)", () => {
  render(<StatusGlyph code="C" />);
  expect(screen.getByRole("img", { name: "Conflicted" })).toBeTruthy();
});

test("segmented control reports selection via aria-pressed", async () => {
  function Host() {
    const [v, setV] = useState("split");
    return (
      <SegmentedControl
        label="mode"
        value={v}
        onChange={setV}
        options={[
          { value: "split", label: "Split" },
          { value: "unified", label: "Unified" },
        ]}
      />
    );
  }
  render(<Host />);
  await userEvent.click(screen.getByRole("button", { name: "Unified" }));
  expect(screen.getByRole("button", { name: "Unified" }).getAttribute("aria-pressed")).toBe("true");
  expect(screen.getByRole("button", { name: "Split" }).getAttribute("aria-pressed")).toBe("false");
});

test("button default type is button and disabled blocks clicks", async () => {
  const onClick = vi.fn();
  render(
    <Button disabled onClick={onClick}>
      x
    </Button>,
  );
  await userEvent.click(screen.getByRole("button"));
  expect(onClick).not.toHaveBeenCalled();
  expect(screen.getByRole("button").getAttribute("type")).toBe("button");
});

test("dialog opens, Esc and backdrop request close", async () => {
  const onClose = vi.fn();
  render(
    <Dialog open onClose={onClose} title="Confirm">
      <input aria-label="first" />
    </Dialog>,
  );
  const dlg = screen.getByRole("dialog", { name: "Confirm", hidden: true });
  expect(dlg.hasAttribute("open")).toBe(true);
  await userEvent.keyboard("{Escape}");
  dlg.click();
  expect(onClose).toHaveBeenCalled();
});

test("sidebar width clamps to 220-600 and works from the keyboard", async () => {
  expect(clampSidebar(10)).toBe(220);
  expect(clampSidebar(9999)).toBe(600);
  render(
    <ResizableSidebar>
      <span>content</span>
    </ResizableSidebar>,
  );
  const sep = screen.getByRole("separator");
  expect(sep.getAttribute("aria-valuenow")).toBe("330");
  sep.focus();
  await userEvent.keyboard("{Home}");
  expect(sep.getAttribute("aria-valuenow")).toBe("220");
  await userEvent.keyboard("{End}");
  expect(sep.getAttribute("aria-valuenow")).toBe("600");
});
