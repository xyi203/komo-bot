import { describe, expect, it } from "vitest";

import type { SessionSummary, WorkspaceInfo } from "@/shared/types";
import { groupByWorkspace } from "./grouping";

function session(id: string, roots: string[] | undefined, created_at: number): SessionSummary {
  return { id, roots, created_at, messages: 2, user_turns: 1 };
}

const workspaces: WorkspaceInfo[] = [
  { id: "__default__", name: "komo", path: "/repo/komo" },
  { id: "notes", name: "notes", path: "/ws/notes" },
];

describe("groupByWorkspace", () => {
  it("puts each session under its primary root", () => {
    const groups = groupByWorkspace(
      [
        session("a", ["/ws/notes"], 3),
        session("b", ["/repo/komo"], 2),
        session("c", ["/ws/notes", "/repo/komo"], 1),
      ],
      workspaces,
    );
    expect(groups.map((g) => [g.root, g.entries.map((e) => e.id)])).toEqual([
      ["/ws/notes", ["a", "c"]],
      ["/repo/komo", ["b"]],
    ]);
  });

  it("orders groups by their most recent session, not by name", () => {
    const groups = groupByWorkspace(
      [session("old", ["/repo/komo"], 10), session("new", ["/ws/notes"], 99)],
      workspaces,
    );
    expect(groups.map((g) => g.root)).toEqual(["/ws/notes", "/repo/komo"]);
  });

  it("labels a root by its catalog name, and an unlisted directory by its last segment", () => {
    const groups = groupByWorkspace(
      [session("a", ["/repo/komo"], 3), session("b", ["/foo/bar"], 2)],
      workspaces,
    );
    expect(groups.map((g) => g.label)).toEqual(["komo", "bar"]);
  });

  it("collects unbound sessions into one group, last however recent", () => {
    // A home session, a chat channel's, a remote caller's — no directory to name.
    const groups = groupByWorkspace(
      [session("home", [], 99), session("legacy", undefined, 98), session("work", ["/foo/bar"], 1)],
      workspaces,
    );
    expect(groups.map((g) => [g.root, g.label, g.entries.map((e) => e.id)])).toEqual([
      ["/foo/bar", "bar", ["work"]],
      ["", "无 workspace", ["home", "legacy"]],
    ]);
  });

  it("has no groups when there are no sessions", () => {
    expect(groupByWorkspace([], workspaces)).toEqual([]);
  });
});
