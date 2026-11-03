export interface DiffRow {
  /** `@` 是块头，`+` 加、`-` 删、空格 上下文。 */
  t: "@" | "+" | "-" | " ";
  a?: number;
  b?: number;
  s: string;
  /** 行内改了的那一段 `[起, 止)`：只有配上对的删 / 加行才有（见 `markInline`）。 */
  hl?: [number, number];
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
  markInline(rows);
  return rows;
}

const INLINE_MAX = 2000; // 超过这么长的行不做行内对比

/**
 * 行内高亮：一段连续的删行紧跟一段加行时，按顺序两两配对，去掉共同的开头与结尾，中间那段就是改了的。
 * 只在改动不超过整行一半、且两行有共同部分时才标；整行都换了就不标，免得整行都是深底。
 */
function markInline(rows: DiffRow[]) {
  for (let i = 0; i < rows.length; ) {
    if (rows[i]!.t !== "-") {
      i++;
      continue;
    }
    let j = i;
    while (j < rows.length && rows[j]!.t === "-") j++;
    let k = j;
    while (k < rows.length && rows[k]!.t === "+") k++;
    const pairs = Math.min(j - i, k - j);
    for (let p = 0; p < pairs; p++) {
      const del = rows[i + p]!;
      const add = rows[j + p]!;
      if (del.s.length > INLINE_MAX || add.s.length > INLINE_MAX) continue;
      const max = Math.min(del.s.length, add.s.length);
      let head = 0;
      while (head < max && del.s[head] === add.s[head]) head++;
      let tail = 0;
      while (tail < max - head && del.s[del.s.length - 1 - tail] === add.s[add.s.length - 1 - tail]) tail++;
      const common = head + tail;
      if (common === 0 || common * 2 < Math.max(del.s.length, add.s.length)) continue;
      if (del.s.length - common > 0) del.hl = [head, del.s.length - tail];
      if (add.s.length - common > 0) add.hl = [head, add.s.length - tail];
    }
    i = k;
  }
}

export const splitDisplay = (display: string): [string, string] => {
  const k = display.lastIndexOf("/");
  return [display.slice(0, k + 1), display.slice(k + 1)];
};

/** 家目录缩成 `~`。 */
export const homeDir = (path: string) => path.replace(/^\/(?:Users|home)\/[^/]+(?=\/|$)/, "~");
