import { execFile, spawn } from "node:child_process";
import { accessSync, constants, readFileSync } from "node:fs";
import { homedir } from "node:os";
import path from "node:path";
import { defaultHome, komoHome } from "./paths";

/** `runtime/gateway.json` 里本壳用到的字段（crates/komo-gateway/src/lock.rs 写、komo-client discovery 读）。 */
export interface Discovery {
  base_url: string;
  token?: string;
  instance_id?: string;
  protocol_version?: number;
  version?: string;
}

export function readDiscovery(): Discovery | undefined {
  try {
    const parsed = JSON.parse(readFileSync(path.join(komoHome(), "runtime", "gateway.json"), "utf8")) as Discovery;
    return typeof parsed.base_url === "string" ? parsed : undefined;
  } catch {
    return undefined;
  }
}

/**
 * 找 `komo` 可执行文件，顺序：`KOMO_BIN` 环境变量 → PATH → `~/.local/bin`（install.sh 装的地方）→
 * Homebrew 的两个常见目录。从 Finder 启动的 App 拿到的 PATH 很短，所以后几项不能省。
 */
export function findKomo(): string | undefined {
  const dirs = [
    ...(process.env.PATH ?? "").split(path.delimiter).filter(Boolean),
    path.join(homedir(), ".local", "bin"),
    "/opt/homebrew/bin",
    "/usr/local/bin",
  ];
  const candidates = [process.env.KOMO_BIN, ...dirs.map((dir) => path.join(dir, "komo"))].filter(Boolean) as string[];
  return candidates.find((candidate) => {
    try {
      accessSync(candidate, constants.X_OK);
      return true;
    } catch {
      return false;
    }
  });
}

/**
 * 拉起本机 Gateway。
 * - 默认数据目录：`komo gateway`（装 / 启动系统服务，等就绪后返回），与 CLI 的"需要时启动"同一个行为。
 * - 非默认的 `KOMO_HOME`：komo 自己拒绝碰全局服务单元（units::owns_unit，免得把现役服务改写成指向别处），
 *   这里改成分离的 `komo gateway --foreground` 子进程；App 退出它也继续跑。
 * 不等就绪——调用方自己轮询 /healthz。
 */
export function launchGateway(): Promise<{ ok: true } | { ok: false; message: string }> {
  const komo = findKomo();
  if (!komo) {
    return Promise.resolve({
      ok: false,
      message: "找不到 komo 命令（查过 KOMO_BIN、PATH、~/.local/bin、/opt/homebrew/bin、/usr/local/bin）。先安装 komo，或改用远程连接。",
    });
  }
  const env = { ...process.env, KOMO_HOME: komoHome() };
  if (path.resolve(komoHome()) !== path.resolve(defaultHome())) {
    const child = spawn(komo, ["gateway", "--foreground"], { env, detached: true, stdio: "ignore" });
    child.on("error", () => {});
    child.unref();
    return Promise.resolve({ ok: true });
  }
  return new Promise((resolve) => {
    execFile(komo, ["gateway"], { env, timeout: 90_000 }, (error, _stdout, stderr) => {
      if (error) resolve({ ok: false, message: `komo gateway 失败：${(stderr || error.message).trim().slice(0, 400)}` });
      else resolve({ ok: true });
    });
  });
}
