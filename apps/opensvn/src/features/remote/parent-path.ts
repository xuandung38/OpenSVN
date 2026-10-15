import type { ParentRepo } from "../../ipc/bindings";
import { toAppError } from "../../ipc/client";

/**
 * The repositories listed by a server URL that is an `SVNParentPath` location (VisualSVN Server's
 * `https://host/svn/`), taken from the `path-not-found` error the backend returns for it. `null`
 * for any other error, and when the server lists none: the error message then stands as it is.
 */
export function parentRepos(e: unknown): ParentRepo[] | null {
  const err = toAppError(e);
  if (err.code !== "path-not-found") return null;
  const repos = err.repositories;
  return Array.isArray(repos) && repos.length > 0 ? repos : null;
}
