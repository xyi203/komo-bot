// 提示页（连接设置、版本不一致…）的静态部分：pages/*.html、*.css 拷到 dist/pages，
// 脚本由 tsconfig.pages.json 编到同一个目录。主进程的 app:// 处理器从那里提供 /_desktop/*。
import { cpSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
mkdirSync(join(root, "dist", "pages"), { recursive: true });
cpSync(join(root, "pages"), join(root, "dist", "pages"), { recursive: true });
