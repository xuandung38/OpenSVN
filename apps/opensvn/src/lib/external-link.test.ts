import { afterEach, expect, test, vi } from "vitest";
import { openExternalUrl, setExternalOpener } from "./external-link";

afterEach(() => {
  setExternalOpener(null);
  vi.restoreAllMocks();
});

test("registered opener receives only https URLs", async () => {
  const opener = vi.fn();
  setExternalOpener(opener);
  await openExternalUrl("https://example.com/a");
  expect(opener).toHaveBeenCalledWith("https://example.com/a");
  await expect(openExternalUrl("http://example.com")).rejects.toThrow(/non-https/);
  await expect(openExternalUrl("file:///etc/passwd")).rejects.toThrow(/non-https/);
  expect(opener).toHaveBeenCalledTimes(1);
});
