// 本机连接：读本机 Gateway 的发现文件（`$KOMO_HOME/runtime/gateway.json`）拿地址与本机令牌，没在跑就拉起来。
// 本机令牌与机器上的命令行用的是同一个（crates/komo-gateway/src/auth.rs 的 `Credential::Local`）：
// 能读这个 0600 文件的人本来就能用 komo，所以本机连接不经 OIDC。令牌只在主进程里，不落 App 自己的存储。
import { execFile, spawn } from "node:child_process";
import { accessSync, constants, readFileSync, statSync } from "node:fs";
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

export const discoveryFile = () => path.join(komoHome(), "runtime", "gateway.json");

function readDiscovery(): Discovery | undefined {
  try {
    const parsed = JSON.parse(readFileSync(discoveryFile(), "utf8")) as Discovery;
    return typeof parsed.base_url === "string" && parsed.base_url ? parsed : undefined;
  } catch {
    return undefined;
  }
}

/**
 * 发现文件的缓存：按 mtime 与大小判断要不要重读。Gateway 每次启动都重写这个文件（端口、令牌都可能变），
 * 所以每次转发前 stat 一下——比等到 401 再重读省掉一次失败的请求，也让 `/auth/me` 这类不回 401 的接口拿到新令牌。
 */
export class LocalDiscovery {
  private cached: { mtimeMs: number; size: number; value: Discovery | undefined } | undefined;

  current(): Discovery | undefined {
    let stat;
    try {
      stat = statSync(discoveryFile());
    } catch {
      this.cached = undefined;
      return undefined;
    }
    if (!this.cached || this.cached.mtimeMs !== stat.mtimeMs || this.cached.size !== stat.size) {
      this.cached = { mtimeMs: stat.mtimeMs, size: stat.size, value: readDiscovery() };
    }
    return this.cached.value;
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
 * - 默认数据目录：`komo gateway`（装 / 启动系统服务，等就绪后返回），与命令行"需要时启动"同一个行为。
 * - 非默认的 `KOMO_HOME`：komo 自己拒绝碰全局服务单元，这里改成分离的 `komo gateway --foreground` 子进程；
 *   App 退出它也继续跑。
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
