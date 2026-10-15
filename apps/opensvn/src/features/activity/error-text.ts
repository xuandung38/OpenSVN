import type { AppError } from "../../ipc/bindings";
import type { ActivityKey } from "./activity-strings";

/** What an error row shows: a short sentence, plus the technical text behind "Details". */
export interface ErrorText {
  /** Localised short message, or `null` to show the backend's own message. */
  key: ActivityKey | null;
  /** Backend message (shown when `key` is null). */
  message: string;
  /** Full technical text: the raw message, the detail and the code; `null` when it adds nothing. */
  detail: string | null;
}

/**
 * Transport failures, most specific first. The backend's text is the reqwest/hyper error chain
 * ("HTTP request failed: error decoding response body: ... peer closed connection ..."): useful
 * for a bug report, meaningless to the user.
 */
const TRANSPORT: ReadonlyArray<[RegExp, ActivityKey]> = [
  [
    /peer closed|connection (was )?(closed|reset|aborted)|reset by peer|broken pipe|unexpected eof|error reading a body|incomplete message|connection closed before/i,
    "errConnectionClosed",
  ],
  [/timed? ?out|timeout|deadline/i, "errTimeout"],
  [/connection refused/i, "errRefused"],
  [
    /dns|failed to lookup|name or service not known|no such host|nodename nor servname|name resolution/i,
    "errHostNotFound",
  ],
  [/certificate|handshake|\btls\b|\bssl\b|rustls/i, "errTls"],
  [
    /http request failed|error sending request|error decoding response|hyper|tcp connect/i,
    "errNetwork",
  ],
];

/**
 * Writes cut off after they were sent, recognised by the backend's own sentence: never sent
 * again, so the user must refresh before trying again. Checked before the transport chain,
 * whose "connection closed" would read as a plain retryable failure.
 */
const UNSURE: ReadonlyArray<[RegExp, ActivityKey]> = [
  [/the commit may or may not have been applied/i, "errCommitUnsure"],
  [/may or may not\s+have reached the server/i, "errWriteUnsure"],
];

/** Codes with a fixed short text; the backend's message stays in the details. */
const BY_CODE: Partial<Record<AppError["code"], ActivityKey>> = {
  "unrelated-resources": "errUnrelated",
};

/** Codes whose message may be a raw transport chain. */
const TRANSPORT_CODES: ReadonlySet<AppError["code"]> = new Set(["network", "timeout"]);

/** Maps an operation error to a short localised text, keeping the raw text as the detail. */
export function errorText(error: Pick<AppError, "code" | "message" | "detail">): ErrorText {
  const raw = [error.message, error.detail].filter(Boolean).join("\n");
  const byCode = BY_CODE[error.code];
  if (byCode) {
    return { key: byCode, message: error.message, detail: `${raw}\n(${error.code})` };
  }
  const unsure = UNSURE.find(([re]) => re.test(error.message));
  if (unsure) {
    return { key: unsure[1], message: error.message, detail: `${raw}\n(${error.code})` };
  }
  const looksTransport =
    TRANSPORT_CODES.has(error.code) || /http request failed|error sending request/i.test(raw);
  if (looksTransport) {
    const hit = TRANSPORT.find(([re]) => re.test(raw));
    const key: ActivityKey = hit ? hit[1] : error.code === "timeout" ? "errTimeout" : "errNetwork";
    return { key, message: error.message, detail: `${raw}\n(${error.code})` };
  }
  return { key: null, message: error.message, detail: error.detail };
}
