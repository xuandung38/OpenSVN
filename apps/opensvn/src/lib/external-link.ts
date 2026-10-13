import { MOCK_ALLOWED } from "../ipc/client";

/**
 * Opens an https URL in the system browser, never inside the app WebView. In Tauri the opener is
 * `@tauri-apps/plugin-opener`'s `openUrl` (registered at startup by `installTauriOpener`; the Rust side
 * scopes it to https). `window.open` is a convenience only for dev/mock browser sessions.
 */
type Opener = (url: string) => Promise<void> | void;

let opener: Opener | null = null;

export function setExternalOpener(fn: Opener | null): void {
  opener = fn;
}

/** Wire the Tauri opener plugin. Call once at app init when running inside Tauri. */
export async function installTauriOpener(): Promise<void> {
  const { openUrl } = await import("@tauri-apps/plugin-opener");
  setExternalOpener((url) => openUrl(url));
}

export async function openExternalUrl(url: string): Promise<void> {
  const u = new URL(url);
  if (u.protocol !== "https:") {
    throw new Error(`refusing to open non-https URL: ${u.protocol}`);
  }
  if (opener) {
    await opener(u.toString());
  } else if (MOCK_ALLOWED) {
    window.open(u.toString(), "_blank", "noopener,noreferrer");
  } else {
    // Release build without an opener: fail loudly rather than navigating inside the WebView.
    throw new Error("No external opener registered");
  }
}
