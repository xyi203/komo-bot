// Grouping the session list by the directory a session is bound to. Kept out of
// the component (which imports the store, and the store touches the DOM) so it
// stays unit-testable.

import type { SessionSummary, WorkspaceInfo } from "@/shared/types";
import { rootLabel } from "@/shared/lib/workspace";

/** Key of the group holding sessions with no bound directory. */
const UNBOUND = "";

export interface RootGroup {
  /** The session's primary root, or "" for the unbound group. */
  root: string;
  label: string;
  entries: SessionSummary[];
}

/** Sessions grouped by their primary root, newest group first.
 *
 *  Groups are ordered by their most recent session rather than by directory
 *  name: the sidebar is a recency list, and sorting the *groups* alphabetically
 *  would bury whichever project is being worked on. Within a group the incoming
 *  order is preserved. Sessions with no binding at all — chat channels, remote
 *  callers — go last however recent they are: they name no project to scan for. */
export function groupByWorkspace(
  sessions: SessionSummary[],
  workspaces: WorkspaceInfo[],
): RootGroup[] {
  const groups = new Map<string, SessionSummary[]>();
  for (const item of sessions) {
    const root = item.roots?.[0] ?? UNBOUND;
    const entries = groups.get(root);
    if (entries) entries.push(item);
    else groups.set(root, [item]);
  }
  return Array.from(groups)
    .map(([root, entries]) => ({
      root,
      label: root === UNBOUND ? "无 workspace" : rootLabel(root, workspaces),
      entries,
      newest: Math.max(...entries.map((entry) => entry.created_at)),
    }))
    .sort((a, b) => {
      if ((a.root === UNBOUND) !== (b.root === UNBOUND)) return a.root === UNBOUND ? 1 : -1;
      return b.newest - a.newest;
    })
    .map(({ newest: _newest, ...group }) => group);
}
