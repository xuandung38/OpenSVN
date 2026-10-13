import { useCallback, useEffect, useState } from "react";
import { useEventBus } from "../../../app/providers";
import { useToast } from "../../../components/ui/toast";
import type { AuthResponse } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { type AuthPromptInfo, normalizeAuthPrompt } from "./auth-model";
import { certDecisionFor } from "./cert-decision";
import { SignInDialog } from "./sign-in-dialog";
import { TrustCertDialog } from "./trust-cert-dialog";

/** Wire form of the Windows SSO answer; see contract request (AuthResponse has no negotiate variant yet). */
type AuthResponseWithSso = AuthResponse | { kind: "negotiate" };

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
    async (requestId: string, response: AuthResponseWithSso) => {
      setQueue((q) => q.filter((p) => p.requestId !== requestId));
      try {
        await unwrap(ipc().commands.authRespond(requestId, response as AuthResponse));
      } catch (e) {
        toast.show(e instanceof Error ? e.message : String(e));
      }
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
        onSso={() => void respond(current.requestId, { kind: "negotiate" })}
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
