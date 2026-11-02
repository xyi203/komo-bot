// 界面文字必须走文案表（docs/clients.md D9）：JSX 里出现中文字面量就报错。
// 查的是 src/ 下 .tsx 的 JSX 文本、JSX 属性与 `{…}` 表达式里的字符串字面量（含模板字符串）；
// 注释不查，messages/*.json 是文案表本身，自然不查。生成物（paraglide、routeTree）跳过。
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

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

/** 一个 JSX 子树里所有带中文的文本 / 字符串节点。 */
function cjkIn(node, source, out) {
  const textual =
    ts.isJsxText(node) || ts.isStringLiteralLike(node) || ts.isTemplateHead(node) || ts.isTemplateMiddle(node) || ts.isTemplateTail(node);
  if (textual && CJK.test(node.text ?? "")) {
    out.push({ node, text: node.text.trim() });
  }
  ts.forEachChild(node, (child) => cjkIn(child, source, out));
}

let failures = 0;
for (const file of files(join(root, "src"))) {
  const source = ts.createSourceFile(file, readFileSync(file, "utf8"), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const found = [];
  const visit = (node) => {
    if (ts.isJsxElement(node) || ts.isJsxSelfClosingElement(node) || ts.isJsxFragment(node)) {
      // 只从最外层的 JSX 节点往下找，里面的嵌套交给 cjkIn 一并走完
      cjkIn(node, source, found);
      return;
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  for (const { node, text } of found) {
    const { line, character } = source.getLineAndCharacterOfPosition(node.getStart());
    console.error(`${relative(root, file)}:${line + 1}:${character + 1}  JSX 里不能写中文字面量，放进 messages/*.json：「${text.slice(0, 40)}」`);
    failures += 1;
  }
}
if (failures > 0) {
  console.error(`\n${failures} 处中文字面量绕过了文案表。`);
  process.exit(1);
}
