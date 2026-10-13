/**
 * Expands the VisualSVN web URL templates from Settings > Integrations. Placeholders: `{server}`
 * (host[:port]), `{repo}` (last segment of the repository root), `{rev}`, `{revSpec}` (`head` or
 * `r<N>`), `{path}` (path from the repository root without leading slash, each segment
 * percent-encoded). Returns null unless the result is an https URL.
 */
export function expandWebUrl(
  template: string,
  ctx: { repoRootUrl: string; rev?: number; path?: string },
): string | null {
  if (!template.trim()) return null;
  let root: URL;
  try {
    root = new URL(ctx.repoRootUrl);
  } catch {
    return null;
  }
  const repo = decodeURIComponent(root.pathname.split("/").filter(Boolean).pop() ?? "");
  const path = (ctx.path ?? "").split("/").filter(Boolean).map(encodeURIComponent).join("/");
  const out = template
    .replaceAll("{server}", root.host)
    .replaceAll("{repo}", encodeURIComponent(repo))
    .replaceAll("{rev}", String(ctx.rev ?? ""))
    .replaceAll("{revSpec}", ctx.rev === undefined ? "head" : `r${ctx.rev}`)
    .replaceAll("{path}", path);
  try {
    return new URL(out).protocol === "https:" ? out : null;
  } catch {
    return null;
  }
}
