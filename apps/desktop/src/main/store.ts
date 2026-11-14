import { app, safeStorage } from "electron";
import { chmodSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";

/** 不能明文落盘的部分：设备令牌，以及可能带密钥的额外请求头（如 Cloudflare Access 的 service token）。登录前 token 为空串。 */
export interface Secrets {
  token: string;
  headers: Record<string, string>;
}

/** 登录结果里不敏感的部分（会显示在界面上，吊销时要用 sessionId）。 */
export interface StoredLogin {
  sessionId: string;
  subject: string;
  email?: string;
}

/**
 * `userData/connection.json`：Gateway 地址与登录状态。`secret` 是 safeStorage 加密后的 Secrets（base64）；
 * 有 `login` 才算登录过，没有就是"只填了地址"。
 */
export interface StoredConnection {
  url?: string;
  secret?: string;
  login?: StoredLogin;
}

const file = () => path.join(app.getPath("userData"), "connection.json");

export function loadStored(): StoredConnection {
  try {
    const parsed = JSON.parse(readFileSync(file(), "utf8")) as StoredConnection & { remote?: { url?: string; secret?: string } };
    // 旧版（手填令牌）写的是 { mode, remote: { url, secret } }：地址与额外请求头沿用，令牌不再认——没有 login 就得重新登录。
    const url = parsed.url ?? parsed.remote?.url;
    if (typeof url !== "string" || !url) return {};
    const secret = parsed.secret ?? parsed.remote?.secret;
    const login = parsed.login;
    const valid = login && typeof login.sessionId === "string" && typeof login.subject === "string";
    return {
      url,
      ...(typeof secret === "string" && secret ? { secret } : {}),
      ...(valid ? { login: { sessionId: login.sessionId, subject: login.subject, ...(login.email ? { email: login.email } : {}) } } : {}),
    };
  } catch {
    // 没有文件或读不出来：当作从没配过。
    return {};
  }
}

/**
 * 能不能把令牌安全地存下来。Linux 上没有钥匙串（gnome-libsecret / kwallet）时 Chromium 退到
 * 写死口令的 basic_text，`isEncryptionAvailable()` 仍然返回 true，但那等于明文，这里按不可用处理。
 */
export function canEncrypt(): boolean {
  if (!safeStorage.isEncryptionAvailable()) return false;
  return !(process.platform === "linux" && safeStorage.getSelectedStorageBackend() === "basic_text");
}

function write(value: StoredConnection) {
  const target = file();
  mkdirSync(path.dirname(target), { recursive: true });
  const temp = `${target}.tmp`;
  writeFileSync(temp, JSON.stringify(value), { mode: 0o600 });
  renameSync(temp, target);
  chmodSync(target, 0o600);
}

/**
 * 保存地址（与登录状态）。令牌或额外请求头要落盘时，加密不可用就抛，不退化成明文；
 * 只有地址（没有令牌、没有请求头）不需要加密。
 */
export function saveStored(url: string, secrets: Secrets, login?: StoredLogin) {
  const needsSecret = Boolean(secrets.token) || Object.keys(secrets.headers).length > 0;
  if (!needsSecret) return write({ url });
  if (!canEncrypt()) throw new Error("系统钥匙串不可用（safeStorage），无法安全保存登录令牌");
  const secret = safeStorage.encryptString(JSON.stringify(secrets)).toString("base64");
  write({ url, secret, ...(login ? { login } : {}) });
}

/** 写不进去时的兜底：直接删文件，免得留着一个已经作废的令牌。 */
export function removeStored() {
  rmSync(file(), { force: true });
}

export function readSecrets(stored: StoredConnection): Secrets | undefined {
  if (!stored.secret) return undefined;
  try {
    const parsed = JSON.parse(safeStorage.decryptString(Buffer.from(stored.secret, "base64"))) as Partial<Secrets>;
    return { token: String(parsed.token ?? ""), headers: parsed.headers ?? {} };
  } catch {
    return undefined;
  }
}
