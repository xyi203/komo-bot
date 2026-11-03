import { Fragment, memo, type ReactNode } from "react";
import { cn } from "@/lib/utils";

// 回复正文：代码块、标题、列表、表格、行内代码、粗体、链接，够看就行（与原型同一份规则）。
// 解析成 React 节点，不拼 HTML 字符串，所以不需要转义，也没有 dangerouslySetInnerHTML。

// 行内：代码 > 粗体 > 链接 > 裸链接，粗体与链接文字里可以再套行内代码等（原型是按顺序整串替换，嵌套自然成立）。
// 裸链接只吃 ASCII：中文、全角括号标点紧跟在后面也能断开。
const INLINE = /`([^`]+)`|\*\*(.+?)\*\*|\[([^\]]+)\]\(([^)\s]+)\)|(https?:\/\/[^\s<>"'`\u0080-\uffff]+)/;

/** 裸链接末尾的句点逗号等不算链接；`)` 只在没配对时去掉（保留 wiki 那种带括号的地址）。 */
function trimUrl(url: string): string {
  for (;;) {
    const last = url.at(-1);
    if (last && ".,;:!?]}".includes(last)) url = url.slice(0, -1);
    else if (last === ")" && url.split("(").length < url.split(")").length) url = url.slice(0, -1);
    else return url;
  }
}

/** 只放行 http(s)、mailto 与没有协议的路径；`javascript:` 之类不渲染成链接。 */
const safeTarget = (target: string) => /^(https?:|mailto:)/i.test(target) || !/^[a-z][a-z0-9+.-]*:/i.test(target);

function inline(text: string): ReactNode[] {
  const out: ReactNode[] = [];
  let rest = text;
  for (let key = 0; rest; key++) {
    const hit = INLINE.exec(rest);
    if (!hit) {
      out.push(rest);
      break;
    }
    if (hit.index > 0) out.push(rest.slice(0, hit.index));
    const [whole, code, bold, label, target, bare] = hit;
    if (bare !== undefined) {
      const url = trimUrl(bare);
      out.push(
        <a key={key} href={url} target="_blank" rel="noreferrer">
          {url}
        </a>,
      );
      rest = rest.slice(hit.index + url.length);
      continue;
    }
    if (code !== undefined) out.push(<code key={key}>{code}</code>);
    else if (bold !== undefined) out.push(<strong key={key}>{inline(bold)}</strong>);
    else if (target && safeTarget(target)) {
      // 打开方式照原型：新标签页
      out.push(
        <a key={key} href={target} target="_blank" rel="noreferrer">
          {inline(label!)}
        </a>,
      );
    } else out.push(whole);
    rest = rest.slice(hit.index + whole.length);
  }
  return out;
}

type Block =
  | { kind: "code"; text: string }
  | { kind: "table"; head: string[]; rows: string[][] }
  | { kind: "list"; ordered: boolean; items: string[] }
  | { kind: "heading"; text: string }
  | { kind: "paragraph"; lines: string[] };

const LIST_ITEM = /^\s*([-*]|\d+\.)\s+/;

export function parseMarkdown(source: string): Block[] {
  const blocks: Block[] = [];
  const lines = source.replace(/\r/g, "").split("\n");
  const cells = (line: string) => line.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
  for (let i = 0; i < lines.length; ) {
    const line = lines[i]!;
    if (/^```/.test(line)) {
      const buffer: string[] = [];
      for (i++; i < lines.length && !/^```/.test(lines[i]!); i++) buffer.push(lines[i]!);
      i++;
      blocks.push({ kind: "code", text: buffer.join("\n") });
    } else if (/^\s*\|.*\|\s*$/.test(line) && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1] ?? "")) {
      const head = cells(line);
      i += 2;
      const rows: string[][] = [];
      for (; i < lines.length && /^\s*\|/.test(lines[i]!); i++) rows.push(cells(lines[i]!));
      blocks.push({ kind: "table", head, rows });
    } else if (LIST_ITEM.test(line)) {
      const ordered = /^\s*\d+\./.test(line);
      const items: string[] = [];
      for (; i < lines.length && LIST_ITEM.test(lines[i]!); i++) items.push(lines[i]!.replace(LIST_ITEM, ""));
      blocks.push({ kind: "list", ordered, items });
    } else if (/^#{1,6}\s/.test(line)) {
      blocks.push({ kind: "heading", text: line.replace(/^#+\s/, "") });
      i++;
    } else if (!line.trim()) {
      i++;
    } else {
      const buffer: string[] = [];
      for (; i < lines.length && lines[i]!.trim() && !/^(```|#{1,6}\s|\s*([-*]|\d+\.)\s|\s*\|)/.test(lines[i]!); i++) buffer.push(lines[i]!);
      blocks.push({ kind: "paragraph", lines: buffer });
    }
  }
  return blocks;
}

function renderBlock(block: Block, i: number): ReactNode {
  switch (block.kind) {
    case "code":
      return (
        <pre key={i}>
          <code>{block.text}</code>
        </pre>
      );
    case "table":
      return (
        <div key={i} className="table-wrap">
          <table>
            <thead>
              <tr>{block.head.map((c, j) => <th key={j}>{inline(c)}</th>)}</tr>
            </thead>
            <tbody>
              {block.rows.map((row, j) => (
                <tr key={j}>{row.map((c, k) => <td key={k}>{inline(c)}</td>)}</tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    case "list": {
      const Tag = block.ordered ? "ol" : "ul";
      return <Tag key={i}>{block.items.map((item, j) => <li key={j}>{inline(item)}</li>)}</Tag>;
    }
    case "heading":
      return <p key={i} className="md-h">{inline(block.text)}</p>;
    case "paragraph":
      return (
        <p key={i}>
          {block.lines.map((line, j) => (
            <Fragment key={j}>
              {j > 0 && <br />}
              {inline(line)}
            </Fragment>
          ))}
        </p>
      );
  }
}

/**
 * 正文。样式在 styles.css 的 `.prose-komo`：阅读字号 15 / 行高 26，段间 14；
 * `compact` 是详情栏、思考里用的 14 / 22。
 */
export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  return <div className={cn("prose-komo", className)}>{parseMarkdown(text).map(renderBlock)}</div>;
});
