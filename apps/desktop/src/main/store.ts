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

/** 本机：读 `$KOMO_HOME/runtime/gateway.json` 的本机令牌，不登录；远程：地址 + OIDC 设备令牌。 */
export type StoredMode = "local" | "remote";

/**
 * `userData/connection.json`：连接方式、远程 Gateway 地址与登录状态。`secret` 是 safeStorage 加密后的 Secrets（base64）；
 * 有 `login` 才算登录过，没有就是"只填了地址"。本机模式下远程那几项照样保留，切回远程不用重填、不用重新登录。
 */
export interface StoredConnection {
  mode: StoredMode;
  url?: string;
  secret?: string;
  login?: StoredLogin;
}

const file = () => path.join(app.getPath("userData"), "connection.json");

export function loadStored(): StoredConnection {
  try {
    const parsed = JSON.parse(readFileSync(file(), "utf8")) as Partial<StoredConnection> & { remote?: { url?: string; secret?: string } };
    // 旧版（手填令牌）写的是 { mode, remote: { url, secret } }：地址与额外请求头沿用，令牌不再认——没有 login 就得重新登录。
    const raw = parsed.url ?? parsed.remote?.url;
    const url = typeof raw === "string" && raw ? raw : undefined;
    // 没写 mode 的文件来自只有远程的那一版：有地址就还是远程（已登录的人升级后不被切走），否则默认本机。
    const mode: StoredMode = parsed.mode === "local" || parsed.mode === "remote" ? parsed.mode : url ? "remote" : "local";
    if (!url) return { mode };
    const secret = parsed.secret ?? parsed.remote?.secret;
    const login = parsed.login;
    const valid = login && typeof login.sessionId === "string" && typeof login.subject === "string";
    return {
      mode,
      url,
      ...(typeof secret === "string" && secret ? { secret } : {}),
      ...(valid ? { login: { sessionId: login.sessionId, subject: login.subject, ...(login.email ? { email: login.email } : {}) } } : {}),
    };
  } catch {
    // 没有文件或读不出来：当作从没配过，默认连本机。
    return { mode: "local" };
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
export function saveStored(mode: StoredMode, url: string, secrets: Secrets, login?: StoredLogin) {
  const needsSecret = Boolean(secrets.token) || Object.keys(secrets.headers).length > 0;
  if (!needsSecret) return write({ mode, url });
  if (!canEncrypt()) throw new Error("系统钥匙串不可用（safeStorage），无法安全保存登录令牌");
  const secret = safeStorage.encryptString(JSON.stringify(secrets)).toString("base64");
  write({ mode, url, secret, ...(login ? { login } : {}) });
}

/** 只改连接方式，远程的地址、密文与登录状态原样留着（不解密、不重新加密）。 */
export function saveMode(mode: StoredMode) {
  write({ ...loadStored(), mode });
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
