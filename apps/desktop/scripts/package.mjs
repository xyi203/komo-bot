// electron-builder 的入口：把版本号对齐到 [workspace.package].version，并按环境变量决定签名与公证。
//
//   node scripts/package.mjs --dir   只出未打包的 .app（本机冒烟，host 架构）
//   node scripts/package.mjs         出 arm64 + x64 的 .dmg 与 .zip，以及 latest-mac.yml（electron-updater 读）
//
// 签名与公证全由环境变量驱动，没有就跳过并照常出未签名包：
//   签名：CSC_LINK + CSC_KEY_PASSWORD（或钥匙串里的 CSC_NAME）；没设时关掉身份自动发现，免得本机钥匙串里
//         碰巧有的证书把"未签名包"签了。
//   公证：APPLE_ID + APPLE_APP_SPECIFIC_PASSWORD + APPLE_TEAM_ID，或 APPLE_API_KEY + APPLE_API_KEY_ID +
//         APPLE_API_ISSUER；只在已签名时才有意义。
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { readBuildInfo } from "./build-info.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dirOnly = process.argv.includes("--dir");
const env = { ...process.env };

const signing = Boolean(env.CSC_LINK || env.CSC_NAME);
if (!signing) env.CSC_IDENTITY_AUTO_DISCOVERY = "false";
const notarize =
  signing &&
  Boolean(
    (env.APPLE_ID && env.APPLE_APP_SPECIFIC_PASSWORD && env.APPLE_TEAM_ID) ||
      (env.APPLE_API_KEY && env.APPLE_API_KEY_ID && env.APPLE_API_ISSUER),
  );
console.log(`签名：${signing ? "是" : "否（未设 CSC_LINK / CSC_NAME，出未签名包）"}；公证：${notarize ? "是" : "否"}`);

const { komoVersion } = readBuildInfo();
const args = [
  "electron-builder",
  "--mac",
  ...(dirOnly ? ["--dir", "--arm64"] : ["--arm64", "--x64"]),
  "--publish",
  "never",
  `--config.extraMetadata.version=${komoVersion}`,
  `--config.mac.notarize=${notarize}`,
];
const result = spawnSync("pnpm", ["exec", ...args], { cwd: root, env, stdio: "inherit" });
process.exit(result.status ?? 1);
