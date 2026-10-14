import { useEffect, useRef, useState } from "react";
import { ipc, toAppError, unwrap } from "../../ipc/client";

/**
 * Whether `text` has the shape of a VisualSVN Server web-interface link
 * (`https://host[/prefix]/!/#Repo/view/head/trunk`). Shape only: the backend parses and checks it
 * (`svn_types::parse_visualsvn_web_link`), this just decides when to ask.
 */
export function isVisualSvnWebLink(text: string): boolean {
  const s = text.trim();
  let u: URL;
  try {
    u = new URL(s);
  } catch {
    return false;
  }
  return (
    (u.protocol === "https:" || u.protocol === "http:") &&
    (u.pathname.endsWith("/!/") || u.pathname.endsWith("/!")) &&
    s.includes("#")
  );
}

/** Whether `text` is an http(s) URL with a query or fragment (never part of a repository URL). */
export function hasQueryOrFragment(text: string): boolean {
  const s = text.trim();
  if (!/^https?:\/\//i.test(s)) return false;
  return s.includes("?") || s.includes("#");
}

/** State of the web-link conversion for one URL field. */
export type WebLinkState =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "converted"; from: string; to: string; pegRev: number | null }
  | { kind: "failed"; detail: string };

/**
 * Converts a pasted VisualSVN web-interface link in a URL field: the backend probes the
 * repository URL it names (same host, normal sign-in and certificate checks) and, when the server
 * confirms it, `onConverted` replaces the field value. The note stays until the user edits the URL.
 */
export function useWebLinkConversion(
  url: string,
  onConverted: (url: string, pegRev: number | null) => void,
): WebLinkState {
  const [state, setState] = useState<WebLinkState>({ kind: "idle" });
  // The probe depends on the text only; the latest callback is read when it answers.
  const convertedRef = useRef(onConverted);
  convertedRef.current = onConverted;
  const isLink = isVisualSvnWebLink(url);
  const trimmed = url.trim();

  useEffect(() => {
    if (!isLink) {
      // Keep the "converted" note while the field shows the converted URL.
      setState((s) => (s.kind === "converted" && s.to === trimmed ? s : { kind: "idle" }));
      return;
    }
    let live = true;
    setState({ kind: "checking" });
    // Short pause so typing a link by hand probes once, not on every keystroke.
    const timer = setTimeout(() => {
      unwrap(ipc().commands.repoResolveUrl(trimmed))
        .then((r) => {
          if (!live) return;
          if (r.converted) {
            setState({ kind: "converted", from: trimmed, to: r.url, pegRev: r.pegRev });
            convertedRef.current(r.url, r.pegRev);
          } else {
            setState({ kind: "idle" });
          }
        })
        .catch((e) => {
          if (!live) return;
          const err = toAppError(e);
          setState(
            err.code === "cancelled" ? { kind: "idle" } : { kind: "failed", detail: err.message },
          );
        });
    }, 300);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [isLink, trimmed]);

  return state;
}
