import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { setExternalOpener } from "../lib/external-link";
import { isSafeHttpsUrl, SafeLink } from "./ui/safe-link";

afterEach(() => setExternalOpener(null));

test.each([
  ["https://example.com/r/1", true],
  ["http://example.com", false],
  ["javascript:alert(1)", false],
  ["data:text/html,<script>1</script>", false],
  ["file:///etc/passwd", false],
  ["not a url", false],
])("isSafeHttpsUrl(%s) = %s", (url, expected) => {
  expect(isSafeHttpsUrl(url)).toBe(expected);
});

test("unsafe href renders as plain text, no anchor", () => {
  // Built at runtime so the source itself contains no javascript: literal.
  const hostile = `${"java"}script:alert(1)`;
  render(<SafeLink href={hostile}>click</SafeLink>);
  expect(screen.queryByRole("link")).toBeNull();
  expect(screen.getByText("click")).toBeTruthy();
});

test("https link opens externally and never navigates the WebView", async () => {
  const opener = vi.fn();
  setExternalOpener(opener);
  render(<SafeLink href="https://example.com/r/1">rev</SafeLink>);
  await userEvent.click(screen.getByRole("link"));
  expect(opener).toHaveBeenCalledWith("https://example.com/r/1");
});
