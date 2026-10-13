import { render, screen } from "@testing-library/react";
import { expect, test } from "vitest";
import { App } from "./App";

test("renders greeting", () => {
  render(<App />);
  expect(screen.getByRole("heading").textContent).toBe("Hello OpenSVN");
});
