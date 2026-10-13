import { render, screen } from "@testing-library/react";
import { expect, test } from "vitest";
import { Tabs } from "./ui/tabs";

test("every tab's aria-controls resolves to a tabpanel; only the active panel has content", () => {
  const { container } = render(
    <Tabs
      label="t"
      value="a"
      onValueChange={() => undefined}
      tabs={[
        { id: "a", label: "A" },
        { id: "b", label: "B" },
      ]}
      panels={{ a: <p>panel-a</p>, b: <p>panel-b</p> }}
    />,
  );
  for (const tab of screen.getAllByRole("tab")) {
    const id = tab.getAttribute("aria-controls");
    expect(id && container.ownerDocument.getElementById(id)).toBeTruthy();
  }
  expect(screen.getByText("panel-a")).toBeTruthy();
  expect(screen.queryByText("panel-b")).toBeNull();
});
