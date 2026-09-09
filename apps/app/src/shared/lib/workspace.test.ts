import { describe, expect, it } from "vitest";

import type { WorkspaceInfo } from "@/shared/types";
import {
  decodeFolderPath,
  encodeFolder,
  rootLabel,
  workspaceIdForRoot,
  workspaceLabel,
  workspacePath,
} from "./workspace";

const catalog: WorkspaceInfo[] = [{ id: "__default__", name: ".komo", path: "/Users/x/.komo" }];

// base64url of "/Users/xyi/01-code/komo-bot" — the shape `encodeFolder`
// produces and the composer sends in `X-Komo-Workspace`.
const AGENT = "folder:L1VzZXJzL3h5aS8wMS1jb2RlL2tvbW8tYm90";

describe("decodeFolderPath", () => {
  it("decodes a base64url folder id back to its path", () => {
    expect(decodeFolderPath(AGENT)).toBe("/Users/xyi/01-code/komo-bot");
  });

  it("decodes non-ASCII paths", () => {
    expect(decodeFolderPath("folder:L1VzZXJzL3h5aS_mlofmoaM")).toBe("/Users/xyi/文档");
  });

  it("is null for an id that is not a folder id", () => {
    expect(decodeFolderPath("__default__")).toBeNull();
    expect(decodeFolderPath("notes")).toBeNull();
  });

  it("is null rather than throwing on a malformed payload", () => {
    // Every branch here reaches a render path, so none may throw: bad base64,
    // bytes that are not UTF-8, and a decode that isn't an absolute path.
    expect(decodeFolderPath("folder:not base64!")).toBeNull();
    expect(decodeFolderPath("folder:_w")).toBeNull();
    expect(decodeFolderPath("folder:Zm9v")).toBeNull();
    expect(decodeFolderPath("folder:")).toBeNull();
  });
});

describe("workspaceLabel", () => {
  it("prefers the catalog name", () => {
    expect(workspaceLabel("__default__", catalog)).toBe(".komo");
  });

  it("names the default workspace even with an empty catalog", () => {
    expect(workspaceLabel("__default__", [])).toBe("默认 workspace");
  });

  it("names an unlisted folder by its last path segment", () => {
    expect(workspaceLabel(AGENT, [])).toBe("komo-bot");
  });

  it("falls back to the id only when nothing else is knowable", () => {
    expect(workspaceLabel("notes", [])).toBe("notes");
  });
});

describe("workspacePath", () => {
  it("uses the catalog path when there is one", () => {
    expect(workspacePath("__default__", catalog)).toBe("/Users/x/.komo");
  });

  it("recovers the path of an unlisted folder from its id", () => {
    expect(workspacePath(AGENT, [])).toBe("/Users/xyi/01-code/komo-bot");
  });

  it("is null for a catalog id this client has not loaded", () => {
    expect(workspacePath("notes", [])).toBeNull();
  });
});

describe("encodeFolder", () => {
  it("round-trips a path through the folder id", () => {
    expect(encodeFolder("/Users/xyi/01-code/komo-bot")).toBe(AGENT);
    expect(decodeFolderPath(encodeFolder("/Users/xyi/文档"))).toBe("/Users/xyi/文档");
  });
});

describe("rootLabel", () => {
  it("prefers the catalog name for that directory", () => {
    expect(rootLabel("/Users/x/.komo", catalog)).toBe(".komo");
  });

  it("names an unlisted directory by its last segment", () => {
    expect(rootLabel("/Users/xyi/01-code/komo-bot", catalog)).toBe("komo-bot");
    expect(rootLabel("/Users/xyi/01-code/komo-bot/", catalog)).toBe("komo-bot");
  });
});

describe("workspaceIdForRoot", () => {
  it("uses the catalog id when the directory is one the gateway lists", () => {
    expect(workspaceIdForRoot("/Users/x/.komo", catalog)).toBe("__default__");
  });

  it("encodes an unlisted directory as a folder id", () => {
    expect(workspaceIdForRoot("/Users/xyi/01-code/komo-bot", catalog)).toBe(AGENT);
  });
});
