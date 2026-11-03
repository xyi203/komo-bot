// 界面文字必须走文案表（docs/clients.md D9）：JSX 里出现中文字面量就报错。
// 查的是 src/ 下 .tsx 的 JSX 文本、JSX 属性与 `{…}` 表达式里的字符串字面量（含模板字符串）；
// 注释不查，messages/*.json 是文案表本身，自然不查。生成物（paraglide、routeTree）跳过。
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { parseSync } from "oxc-parser";

const root = fileURLToPath(new URL("..", import.meta.url));
const SKIP = new Set(["paraglide", "routeTree.gen.ts"]);
const CJK = new RegExp("[\\u2e80-\\u9fff\\uf900-\\ufaff\\uff00-\\uffef\\u3000-\\u303f]", "u");

function* files(dir) {
  for (const name of readdirSync(dir)) {
    if (SKIP.has(name)) continue;
    const path = join(dir, name);
    if (statSync(path).isDirectory()) yield* files(path);
    else if (name.endsWith(".tsx")) yield path;
  }
}

/** AST 节点的直接子节点（oxc-parser 出的是 ESTree：带 type 的普通对象）。 */
function* children(node) {
  for (const [key, value] of Object.entries(node)) {
    if (key === "parent") continue;
    for (const child of Array.isArray(value) ? value : [value]) {
      if (child && typeof child.type === "string") yield child;
    }
  }
}

/** 节点的文字和报告位置：JSX 文本跳过开头空白，模板片段的位置本就从前面的 ` 或 } 算起。 */
function textOf(node) {
  if (node.type === "JSXText") {
    const lead = node.value.length - node.value.trimStart().length;
    return { text: node.value, at: node.start + lead };
  }
  if (node.type === "Literal" && typeof node.value === "string") return { text: node.value, at: node.start };
  if (node.type === "TemplateElement") return { text: node.value.cooked ?? node.value.raw, at: node.start };
  return null;
}

/** 一个 JSX 子树里所有带中文的文本 / 字符串节点。 */
function cjkIn(node, out) {
  const found = textOf(node);
  if (found && CJK.test(found.text)) out.push({ at: found.at, text: found.text.trim() });
  for (const child of children(node)) cjkIn(child, out);
}

function lineCol(code, offset) {
  const before = code.slice(0, offset);
  const line = before.split("\n").length;
  return { line, column: offset - before.lastIndexOf("\n") };
}

let failures = 0;
for (const file of files(join(root, "src"))) {
  const code = readFileSync(file, "utf8");
  const { program, errors } = parseSync(file, code, { lang: "tsx" });
  if (errors.length > 0) {
    console.error(`${relative(root, file)}  解析失败：${errors[0].message}`);
    failures += 1;
    continue;
  }
  const found = [];
  const visit = (node) => {
    if (node.type === "JSXElement" || node.type === "JSXFragment") {
      // 只从最外层的 JSX 节点往下找，里面的嵌套交给 cjkIn 一并走完
      cjkIn(node, found);
      return;
    }
    for (const child of children(node)) visit(child);
  };
  visit(program);
  for (const { at, text } of found.sort((a, b) => a.at - b.at)) {
    const { line, column } = lineCol(code, at);
    console.error(`${relative(root, file)}:${line}:${column}  JSX 里不能写中文字面量，放进 messages/*.json：「${text.slice(0, 40)}」`);
    failures += 1;
  }
}
if (failures > 0) {
  console.error(`\n${failures} 处中文字面量绕过了文案表。`);
  process.exit(1);
}
