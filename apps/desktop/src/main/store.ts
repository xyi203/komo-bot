import { app, safeStorage } from "electron";
import { chmodSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import path from "node:path";

/** 远程连接里不能明文落盘的部分：设备令牌，以及可能带密钥的额外请求头（如 Cloudflare Access 的 service token）。 */
export interface RemoteSecrets {
  token: string;
  headers: Record<string, string>;
}

export interface StoredConnection {
  mode: "local" | "remote";
  remote?: { url: string; secret: string };
}

const file = () => path.join(app.getPath("userData"), "connection.json");

export function loadStored(): StoredConnection {
  try {
    const parsed = JSON.parse(readFileSync(file(), "utf8")) as Partial<StoredConnection>;
    const remote = parsed.remote?.url && parsed.remote.secret ? parsed.remote : undefined;
    // 模式是 remote 但配置坏了，也退回本机，不卡在一个连不上的远程上。
    return { mode: parsed.mode === "remote" && remote ? "remote" : "local", ...(remote ? { remote } : {}) };
  } catch {
    // 没有文件或读不出来：当作从没配过，用本机。
  }
  return { mode: "local" };
}

function write(value: StoredConnection) {
  const target = file();
  mkdirSync(path.dirname(target), { recursive: true });
  const temp = `${target}.tmp`;
  writeFileSync(temp, JSON.stringify(value), { mode: 0o600 });
  renameSync(temp, target);
  chmodSync(target, 0o600);
}

export function saveRemote(url: string, secrets: RemoteSecrets) {
  // 加密不可用时拒绝保存，不退化成明文。
  if (!safeStorage.isEncryptionAvailable()) throw new Error("系统钥匙串不可用（safeStorage），无法安全保存令牌");
  const secret = safeStorage.encryptString(JSON.stringify(secrets)).toString("base64");
  write({ mode: "remote", remote: { url, secret } });
}

export function saveLocal(previous: StoredConnection) {
  // 保留已存的远程配置，切回本机不丢它（设置页还能看到地址和头的名字）。
  write({ mode: "local", ...(previous.remote ? { remote: previous.remote } : {}) });
}

export function readSecrets(stored: StoredConnection): RemoteSecrets | undefined {
  if (!stored.remote) return undefined;
  try {
    const parsed = JSON.parse(safeStorage.decryptString(Buffer.from(stored.remote.secret, "base64"))) as RemoteSecrets;
    return { token: String(parsed.token ?? ""), headers: parsed.headers ?? {} };
  } catch {
    return undefined;
  }
}
