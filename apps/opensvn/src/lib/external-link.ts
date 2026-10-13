/**
 * Opens an https URL outside the app WebView. The Tauri side registers a real opener at startup
 * via `setExternalOpener`; the default (browser/mock mode) uses window.open with no opener access.
 */
type Opener = (url: string) => Promise<void> | void;

let opener: Opener = (url) => {
  window.open(url, "_blank", "noopener,noreferrer");
};

export function setExternalOpener(fn: Opener): void {
  opener = fn;
}

export async function openExternalUrl(url: string): Promise<void> {
  const u = new URL(url);
  if (u.protocol !== "https:") {
    throw new Error(`refusing to open non-https URL: ${u.protocol}`);
  }
  await opener(u.toString());
}
