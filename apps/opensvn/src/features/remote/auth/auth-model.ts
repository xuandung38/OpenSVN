import type { AuthPromptEvent } from "../../../ipc/bindings";

/** Certificate failure classes of `auth://prompt` (doc 03 section 5). */
export type CertFailure =
  | "unknown-issuer"
  | "self-signed"
  | "expired"
  | "not-yet-valid"
  | "hostname-mismatch"
  | "revoked"
  | { changed: { oldSha256: string; newSha256: string } }
  | { other: string };

export type AuthMethod = "basic" | "negotiate";
export type CredentialReason = "initial" | "rejected" | "negotiate-failed";

export interface CredentialsPromptInfo {
  kind: "credentials";
  requestId: string;
  realm: string;
  url: string;
  reason: CredentialReason;
  /** Failed attempts so far, from 0. */
  attempt: number;
  offered: AuthMethod[];
}

export interface CertInfo {
  host: string;
  subject: string;
  issuer: string;
  notBefore: string;
  notAfter: string;
  sha256: string;
}

export interface CertPromptInfo {
  kind: "certificate";
  requestId: string;
  realm: string;
  url: string;
  cert: CertInfo;
  failure: CertFailure;
}

export type AuthPromptInfo = CredentialsPromptInfo | CertPromptInfo;

/** The server stops asking after this many rejected attempts (doc 01, 4.3). */
export const MAX_AUTH_ATTEMPTS = 3;

const SIMPLE_FAILURES = [
  "unknown-issuer",
  "self-signed",
  "expired",
  "not-yet-valid",
  "hostname-mismatch",
  "revoked",
] as const;

/** Accepts the string form of older bindings and the tagged object form of the current contract. */
export function parseCertFailure(v: unknown): CertFailure {
  if (typeof v === "string") {
    return (SIMPLE_FAILURES as readonly string[]).includes(v) ? (v as CertFailure) : { other: v };
  }
  if (typeof v === "object" && v !== null) {
    const o = v as Record<string, unknown>;
    const ch = o.changed as Record<string, unknown> | undefined;
    if (ch && typeof ch.oldSha256 === "string" && typeof ch.newSha256 === "string") {
      return { changed: { oldSha256: ch.oldSha256, newSha256: ch.newSha256 } };
    }
    if (typeof o.other === "string") return { other: o.other };
  }
  return { other: "unknown" };
}

function asMethods(v: unknown): AuthMethod[] {
  if (!Array.isArray(v)) return ["basic"];
  const m = v.filter((x): x is AuthMethod => x === "basic" || x === "negotiate");
  return m.length > 0 ? m : ["basic"];
}

/**
 * Normalises the `auth://prompt` payload. Fields added by the contract (`reason`, `attempt`,
 * `offered`, structured `cert.failure`) fall back to safe defaults so older backends still work.
 */
export function normalizeAuthPrompt(p: AuthPromptEvent): AuthPromptInfo | null {
  const raw = p as unknown as Record<string, unknown>;
  if (p.kind === "certificate") {
    if (!p.cert) return null;
    const { host, subject, issuer, notBefore, notAfter, sha256 } = p.cert;
    return {
      kind: "certificate",
      requestId: p.requestId,
      realm: p.realm,
      url: p.url,
      cert: { host, subject, issuer, notBefore, notAfter, sha256 },
      failure: parseCertFailure((p.cert as unknown as Record<string, unknown>).failure),
    };
  }
  const reason = raw.reason;
  return {
    kind: "credentials",
    requestId: p.requestId,
    realm: p.realm,
    url: p.url,
    reason:
      reason === "rejected" || reason === "negotiate-failed" || reason === "initial"
        ? reason
        : "initial",
    attempt: typeof raw.attempt === "number" && raw.attempt >= 0 ? raw.attempt : 0,
    offered: asMethods(raw.offered),
  };
}

/** Failures the user may override: only an untrusted issuer or a self-signed certificate. */
export function isTrustable(f: CertFailure): boolean {
  return f === "unknown-issuer" || f === "self-signed";
}

export function isChanged(
  f: CertFailure,
): f is { changed: { oldSha256: string; newSha256: string } } {
  return typeof f === "object" && "changed" in f;
}

/** `https://host:443` including the port, so users see exactly which server asks (not just the realm). */
export function originOf(url: string): string {
  try {
    const u = new URL(url);
    const port = u.port || (u.protocol === "https:" ? "443" : u.protocol === "http:" ? "80" : "");
    return `${u.protocol}//${u.hostname}${port ? `:${port}` : ""}`;
  } catch {
    return url;
  }
}
