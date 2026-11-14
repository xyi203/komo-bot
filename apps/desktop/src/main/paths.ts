import { app } from "electron";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import path from "node:path";

/** 本机数据目录：`KOMO_HOME`，默认 `~/.komo`（与 komo 命令一致）。从 Finder 启动的 App 通常没有 `KOMO_HOME`。 */
export const komoHome = () => process.env.KOMO_HOME || path.join(homedir(), ".komo");

export const defaultHome = () => path.join(homedir(), ".komo");

/** SPA 产物目录。打包后在 Resources/client；开发时指向 apps/client/dist/client（先 pnpm build）。 */
export function clientRoot(): string {
  if (process.env.KOMO_DESKTOP_CLIENT_DIR) return path.resolve(process.env.KOMO_DESKTOP_CLIENT_DIR);
  return app.isPackaged
    ? path.join(process.resourcesPath, "client")
    : path.resolve(__dirname, "../../../client/dist/client");
}

/** 壳自带的提示页（连接设置、版本不一致…），在 /_desktop/ 下。 */
export const pagesRoot = () => path.resolve(__dirname, "../pages");

/** 菜单栏图标等（打包时 assets/trayTemplate*.png 在 asar 里，位置相对 dist/main 不变）。 */
export const assetsRoot = () => path.resolve(__dirname, "../../assets");

export interface BuildInfo {
  protocolVersion: number;
  komoVersion: string;
}

/** scripts/build-info.mjs 在构建时写的：打包时 kernel 的 PROTOCOL_VERSION 与 komo 版本。 */
export function readBuildInfo(): BuildInfo {
  return JSON.parse(readFileSync(path.resolve(__dirname, "../build-info.json"), "utf8")) as BuildInfo;
}
