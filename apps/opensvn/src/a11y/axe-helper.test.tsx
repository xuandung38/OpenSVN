import { render } from "@testing-library/react";
import { expect, test } from "vitest";
import { Dialog } from "../components/ui/dialog";
import { expectNoSeriousA11yViolations, prepareDocument } from "../test-axe";

// The helper itself: content of an open <dialog> must really be scanned in jsdom (a missing layout
// API once made axe skip it silently), and serious findings must fail with rule and target.

test("finds serious violations inside an open dialog", async () => {
  prepareDocument("en");
  render(
    <Dialog open onClose={() => undefined} title="Probe">
      <input />
      <button type="button" />
    </Dialog>,
  );
  const failure = expectNoSeriousA11yViolations("probe");
  await expect(failure).rejects.toThrow(/- label \[critical\]/);
  await expect(expectNoSeriousA11yViolations("probe")).rejects.toThrow(
    /- button-name \[critical\][\s\S]*at button/,
  );
});

test("passes a labelled dialog", async () => {
  prepareDocument("en");
  render(
    <Dialog open onClose={() => undefined} title="Probe">
      <label>
        Name <input />
      </label>
      <button type="button">OK</button>
    </Dialog>,
  );
  await expectNoSeriousA11yViolations("labelled probe");
});
