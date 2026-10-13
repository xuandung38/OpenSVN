import type { CertDecision } from "../../../ipc/bindings";

/** What the user chose in the trust dialog. `replacePin` is the explicit re-pin for a changed certificate. */
export type CertDecisionChoice = "reject" | "acceptOnce" | "acceptAlways" | "replacePin";

/**
 * Maps the UI choice to the `auth_respond` decision. Re-pinning a changed certificate uses
 * `acceptAlways`, but only ever reached through the confirmed `replacePin` action. Contract request:
 * a distinct `replacePin` decision so the backend can enforce and audit it; switch to it when it lands.
 */
export function certDecisionFor(c: CertDecisionChoice): CertDecision {
  return c === "replacePin" ? "acceptAlways" : c;
}
