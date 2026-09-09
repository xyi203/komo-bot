// Two vocabularies meet here, and they are not the same thing.
//
// A workspace **id** (`__default__`, a catalog name, `folder:<base64url path>`)
// is what the composer's picker selects and what rides in `X-Komo-Workspace`.
// A session's **roots** are absolute paths the gateway bound on that session's
// first turn and reports back on `/api/sessions`. Ids only ever go *out*, paths
// only ever come *in*; `workspaceIdForRoot` is the one crossing, for the picker
// that has to display an already-bound session's directory.
//
// Either way the raw value is unreadable — base64 on one side, a full path on
// the other — exactly where the operator is scanning for which project a
// conversation belongs to, so nothing renders one directly.

import type { WorkspaceInfo } from "@/shared/types";

/** The gateway's id for "wherever komo itself lives". */
export const DEFAULT_WORKSPACE = "__default__";

/** The absolute path inside a `folder:` id, or null for any other id.
 *
 *  Deliberately total: an id from a newer client, a truncated one, or one that
 *  simply isn't base64 must degrade to "not a folder id" rather than throw on a
 *  render path. */
export function decodeFolderPath(id: string): string | null {
  const encoded = id.startsWith("folder:") ? id.slice("folder:".length) : null;
  if (!encoded) return null;
  try {
    const binary = atob(encoded.replaceAll("-", "+").replaceAll("_", "/"));
    const bytes = Uint8Array.from(binary, (char) => char.charCodeAt(0));
    const path = new TextDecoder(undefined, { fatal: true }).decode(bytes);
    return path.startsWith("/") ? path : null;
  } catch {
    return null;
  }
}

/** Encode an absolute path as an opaque `folder:` workspace id.
 *
 *  The gateway resolves catalog ids by name and only decodes this form for a
 *  loopback caller (`resolve_folder_workspace` in infra/messaging/api.rs).
 *  base64url is what makes an arbitrary Unicode path safe to carry in the
 *  ASCII-only `X-Komo-Workspace` header. */
export function encodeFolder(path: string): string {
  const bytes = new TextEncoder().encode(path);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return `folder:${btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "")}`;
}

/** The last segment of a path — what a person calls that directory. */
function basename(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  return trimmed.slice(trimmed.lastIndexOf("/") + 1) || trimmed || path;
}

/** How a workspace id should read on screen.
 *
 *  Catalog name first (the gateway's own naming wins), then the folder path's
 *  last segment, then the id itself — which by then can only be a catalog id
 *  this client hasn't loaded yet, and those are already human-shaped. */
export function workspaceLabel(id: string, workspaces: WorkspaceInfo[]): string {
  const known = workspaces.find((workspace) => workspace.id === id);
  if (known) return known.name;
  if (id === DEFAULT_WORKSPACE) return "默认 workspace";
  const path = decodeFolderPath(id);
  return path ? basename(path) : id;
}

/** The full path behind a workspace id, when one is knowable — for `title`
 *  attributes, where the whole location is what disambiguates two folders that
 *  share a basename. */
export function workspacePath(id: string, workspaces: WorkspaceInfo[]): string | null {
  return workspaces.find((workspace) => workspace.id === id)?.path ?? decodeFolderPath(id);
}

/** How a session's bound root should read on screen: the catalog's name for
 *  that directory, else its last segment. */
export function rootLabel(root: string, workspaces: WorkspaceInfo[]): string {
  return workspaces.find((workspace) => workspace.path === root)?.name ?? basename(root);
}

/** The workspace id that names a bound root, for the picker that displays it.
 *
 *  The gateway ignores `X-Komo-Workspace` once a session is bound, so this only
 *  ever decides what the operator sees — never where the turn runs. */
export function workspaceIdForRoot(root: string, workspaces: WorkspaceInfo[]): string {
  return workspaces.find((workspace) => workspace.path === root)?.id ?? encodeFolder(root);
}
