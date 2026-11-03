export interface DiffRow {
  /** `@` 是块头，`+` 加、`-` 删、空格 上下文。 */
  t: "@" | "+" | "-" | " ";
  a?: number;
  b?: number;
  s: string;
}

/** unified diff → 行：只看 @@ 之后的部分（文件头可能是副本的临时路径）。 */
export function parsePatch(patch: string): DiffRow[] {
  const rows: DiffRow[] = [];
  let a = 0;
  let b = 0;
  let inHunk = false;
  for (const line of patch.split("\n")) {
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$/.exec(line);
    if (hunk) {
      a = Number(hunk[1]);
      b = Number(hunk[2]);
      inHunk = true;
      rows.push({ t: "@", s: line });
      continue;
    }
    if (!inHunk || line.startsWith("\\")) continue;
    if (line.startsWith("+")) rows.push({ t: "+", b: b++, s: line.slice(1) });
    else if (line.startsWith("-")) rows.push({ t: "-", a: a++, s: line.slice(1) });
    else if (line.startsWith(" ")) rows.push({ t: " ", a: a++, b: b++, s: line.slice(1) });
  }
  return rows;
}

export const splitDisplay = (display: string): [string, string] => {
  const k = display.lastIndexOf("/");
  return [display.slice(0, k + 1), display.slice(k + 1)];
};

/** 家目录缩成 `~`。 */
export const homeDir = (path: string) => path.replace(/^\/(?:Users|home)\/[^/]+(?=\/|$)/, "~");
