// 把"这个 App 是跟哪一版 komo 一起打的"写成 dist/build-info.json，主进程启动时读：
//   - protocolVersion：crates/komo-kernel/src/protocol/mod.rs 的 `PROTOCOL_VERSION`，连接时拿它跟 /healthz 比；
//   - komoVersion：根 Cargo.toml `[workspace.package].version`，打包时同步成 App 版本（scripts/package.mjs）。
// 不另建一份常量：协议版本只有 kernel 那一处，这里在构建时现读，读不到就让构建失败。
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

export function readBuildInfo() {
  const protocolSource = readFileSync(join(root, "crates/komo-kernel/src/protocol/mod.rs"), "utf8");
  const protocol = /pub const PROTOCOL_VERSION: u32 = (\d+);/.exec(protocolSource);
  if (!protocol) throw new Error("读不到 PROTOCOL_VERSION（crates/komo-kernel/src/protocol/mod.rs）");

  const cargo = readFileSync(join(root, "Cargo.toml"), "utf8");
  const section = /\[workspace\.package\]([\s\S]*?)(?:\n\[|$)/.exec(cargo);
  const version = section && /^version\s*=\s*"([^"]+)"/m.exec(section[1]);
  if (!version) throw new Error("读不到 [workspace.package].version（Cargo.toml）");

  return { protocolVersion: Number(protocol[1]), komoVersion: version[1] };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const out = join(dirname(fileURLToPath(import.meta.url)), "..", "dist", "build-info.json");
  mkdirSync(dirname(out), { recursive: true });
  const info = readBuildInfo();
  writeFileSync(out, `${JSON.stringify(info, null, 2)}\n`);
  console.log(`build-info: protocol ${info.protocolVersion}, komo ${info.komoVersion}`);
}
