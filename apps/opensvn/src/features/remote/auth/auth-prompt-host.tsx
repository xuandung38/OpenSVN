import { useCallback, useEffect, useState } from "react";
import { useEventBus } from "../../../app/providers";
import { useToast } from "../../../components/ui/toast";
import type { AuthResponse } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { type AuthPromptInfo, normalizeAuthPrompt } from "./auth-model";
import { certDecisionFor } from "./cert-decision";
import { SignInDialog } from "./sign-in-dialog";
import { TrustCertDialog } from "./trust-cert-dialog";

/**
 * Sends the answer; if `auth_respond` fails for any reason, sends `cancel` for the same request so
 * the engine (which pauses the operation until it gets an answer) is never left waiting.
 * Returns the first error, or null on success.
 */
export async function answerAuthPrompt(
  requestId: string,
  response: AuthResponse,
): Promise<Error | null> {
  try {
    await unwrap(ipc().commands.authRespond(requestId, response));
    return null;
  } catch (e) {
    const first = e instanceof Error ? e : new Error(String(e));
    if (response.kind !== "cancel") {
      try {
        await unwrap(ipc().commands.authRespond(requestId, { kind: "cancel" }));
      } catch {
        // Nothing more to do: the backend rejected both answers; the first error is reported.
      }
    }
    return first;
  }
}

/**
 * Listens for `auth://prompt` and shows one dialog at a time (the engine pauses the operation until
 * `auth_respond`). Prompts queue FIFO; closing a dialog answers `cancel`/`reject` so no operation
 * is left waiting.
 */
export function AuthPromptHost() {
  const bus = useEventBus();
  const toast = useToast();
  const [queue, setQueue] = useState<AuthPromptInfo[]>([]);

  useEffect(
    () =>
      bus.on("auth://prompt", (p) => {
        const info = normalizeAuthPrompt(p);
        if (info) setQueue((q) => [...q, info]);
      }),
    [bus],
  );

  const respond = useCallback(
    async (requestId: string, response: AuthResponse) => {
      setQueue((q) => q.filter((p) => p.requestId !== requestId));
      const err = await answerAuthPrompt(requestId, response);
      if (err) toast.show(err.message);
    },
    [toast],
  );

  const current = queue[0];
  if (!current) return null;
  if (current.kind === "credentials") {
    return (
      <SignInDialog
        key={current.requestId}
        prompt={current}
        onSubmit={(a) => void respond(current.requestId, { kind: "credentials", ...a })}
        onCancel={() => void respond(current.requestId, { kind: "cancel" })}
      />
    );
  }
  return (
    <TrustCertDialog
      key={current.requestId}
      prompt={current}
      onDecision={(d) =>
        void respond(current.requestId, {
          kind: "certificate",
          decision: certDecisionFor(d),
        })
      }
    />
  );
}
