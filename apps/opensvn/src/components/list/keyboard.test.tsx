import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, expect, test, vi } from "vitest";
import { FileRow } from "./file-row";
import { VirtualList } from "./virtual-list";

// jsdom has no layout: give the scroll element a viewport so the virtualizer mounts rows.
beforeAll(() => {
  const rect = { width: 400, height: 280, top: 0, left: 0, right: 400, bottom: 280, x: 0, y: 0 };
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({
    ...rect,
    toJSON: () => rect,
  });
  Object.defineProperty(HTMLElement.prototype, "offsetHeight", { configurable: true, value: 280 });
  Object.defineProperty(HTMLElement.prototype, "offsetWidth", { configurable: true, value: 400 });
  Object.defineProperty(HTMLElement.prototype, "clientHeight", { configurable: true, value: 280 });
});

// Hang guard only (no findBy/waitFor here): userEvent on real timers is slow on a loaded machine.
vi.setConfig({ testTimeout: 120_000 });

const PATHS = Array.from({ length: 100 }, (_, i) => `dir/file-${i}.ts`);

function setup() {
  const onActivate = vi.fn();
  const onToggle = vi.fn();
  render(
    <VirtualList
      label="files"
      count={PATHS.length}
      rowHeight={28}
      onActivate={onActivate}
      onToggle={onToggle}
      renderRow={(i, ctx) => (
        <FileRow id={ctx.id} active={ctx.active} path={PATHS[i] ?? ""} status="M" />
      )}
    />,
  );
  const list = screen.getByRole("listbox", { name: "files" });
  return { list, onActivate, onToggle };
}

const activeIndex = (list: HTMLElement) =>
  Number(list.getAttribute("aria-activedescendant")?.split("-opt-")[1]);

test("listbox is a single tab stop with aria-activedescendant", async () => {
  const { list } = setup();
  expect(list.getAttribute("tabindex")).toBe("0");
  await userEvent.tab();
  expect(document.activeElement).toBe(list);
  expect(activeIndex(list)).toBe(0);
  expect(document.getElementById(list.getAttribute("aria-activedescendant") ?? "")).not.toBeNull();
  // Rows themselves are not tab stops and the checkbox is out of the tab order.
  expect(screen.getAllByRole("option")[0]?.getAttribute("tabindex")).toBeNull();
});

test("arrow/page/home/end keys move the active row (even to unmounted rows)", async () => {
  const { list } = setup();
  list.focus();
  await userEvent.keyboard("{ArrowDown}{ArrowDown}");
  expect(activeIndex(list)).toBe(2);
  await userEvent.keyboard("{ArrowUp}");
  expect(activeIndex(list)).toBe(1);
  await userEvent.keyboard("{End}");
  expect(activeIndex(list)).toBe(99);
  await userEvent.keyboard("{ArrowDown}");
  expect(activeIndex(list)).toBe(99);
  await userEvent.keyboard("{Home}");
  expect(activeIndex(list)).toBe(0);
  await userEvent.keyboard("{PageDown}");
  expect(activeIndex(list)).toBeGreaterThan(1);
  await userEvent.keyboard("{PageUp}");
  expect(activeIndex(list)).toBe(0);
});

test("Enter activates and Space toggles the active row", async () => {
  const { list, onActivate, onToggle } = setup();
  list.focus();
  await userEvent.keyboard("{ArrowDown}{Enter}");
  expect(onActivate).toHaveBeenCalledWith(1);
  await userEvent.keyboard(" ");
  expect(onToggle).toHaveBeenCalledWith(1);
});
