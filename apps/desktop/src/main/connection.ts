import { shell } from "electron";
import { EventEmitter } from "node:events";
import type { ConnectionInfo, ConnectionStatus, SignInInput, SignInResult, SignOutResult, TestResult } from "../shared/api";
import { beginLogin, parseCallback, redeemCode, revokeSession, sameState, type PendingLogin } from "./auth";
import type { BuildInfo } from "./paths";
import {
  canEncrypt,
  loadStored,
  readSecrets,
  removeStored,
  saveStored,
  type Secrets,
  type StoredConnection,
} from "./store";
import { errorMessage, fetchHealth, normalizeBaseUrl, parseExtraHeaders, probe, request, type Health, type Target } from "./upstream";

/**
 * 当前连接：远程 Gateway 的地址 + 设备令牌（OIDC 设备登录换来的）+ 可选的额外请求头。
 * 渲染进程永远不知道令牌——它只对 app://komo 说话，主进程的协议处理器用 `targetSync()` 转发。
 * 登录只在主进程里发生：起（`signIn`）→ 系统浏览器 → 深链回来（`completeLogin`）。
 */
export class ConnectionManager extends EventEmitter<{ changed: [ConnectionInfo]; signedOut: [] }> {
  private stored: StoredConnection = loadStored();
  /** 解密后缓存在内存：每次转发都解一次会反复碰钥匙串。 */
  private secrets: Secrets | undefined = readSecrets(this.stored);
  private pending: PendingLogin | undefined;
  private pendingTimer: ReturnType<typeof setTimeout> | undefined;
  private notice: ConnectionInfo["notice"];
  private state: Pick<ConnectionInfo, "status" | "message" | "gateway" | "older"> = { status: "connecting" };

  constructor(private readonly build: BuildInfo, private readonly appVersion: string) {
    super();
  }

  info(): ConnectionInfo {
    const { url, login } = this.stored;
    return {
      ...this.state,
      baseUrl: login ? url : undefined,
      app: { version: this.appVersion, protocolVersion: this.build.protocolVersion },
      ...(url ? { remote: { url, headerNames: Object.keys(this.secrets?.headers ?? {}) } } : {}),
      ...(login ? { account: { subject: login.subject, ...(login.email ? { email: login.email } : {}) } } : {}),
      ...(this.pending ? { signingIn: true } : {}),
      ...(this.notice ? { notice: this.notice } : {}),
    };
  }

  private set(state: typeof this.state) {
    this.state = state;
    this.emit("changed", this.info());
  }

  private setNotice(text: string | undefined, error = false) {
    this.notice = text ? { text, error } : undefined;
    this.emit("changed", this.info());
  }

  private reload() {
    this.stored = loadStored();
    this.secrets = readSecrets(this.stored);
  }

  /** 登录前也能用的目标（只有额外请求头）：探活、`/auth/me`。没填地址就是 undefined。 */
  private baseTarget(): Target | undefined {
    if (!this.stored.url) return undefined;
    return { base: this.stored.url, headers: { ...this.secrets?.headers } };
  }

  /** 转发要用的目标；没登录（或令牌读不出来）就是 undefined。 */
  targetSync(): Target | undefined {
    const { url, login } = this.stored;
    if (!url || !login || !this.secrets?.token) return undefined;
    // 额外请求头在前、Authorization 在后：写入时已经拒绝了保留名，这里再保证一次顺序。
    return { base: url, headers: { ...this.secrets.headers, authorization: `Bearer ${this.secrets.token}` } };
  }

  /**
   * 探测并核对协议版本与登录状态，结果落在 `info()`。
   * 没填地址、没登录、登录失效都是 `signed-out`：窗口停在连接设置页。
   */
  async connect(): Promise<ConnectionInfo> {
    this.set({ status: "connecting" });
    const base = this.baseTarget();
    if (!base) return this.finish("signed-out", undefined);
    let health: Health;
    try {
      health = await fetchHealth(base);
    } catch (error) {
      return this.finish("unreachable", `连不上 Gateway：${errorMessage(error)}`);
    }
    const gateway = { version: health.version, protocolVersion: health.protocol_version, instanceId: health.instance_id };
    const app = this.build.protocolVersion;
    if (health.protocol_version !== app) {
      const older = health.protocol_version < app ? "gateway" : "app";
      return this.finish(
        "mismatch",
        older === "gateway"
          ? `Gateway（${health.version}，协议 ${health.protocol_version}）比这个 App（${this.appVersion}，协议 ${app}）旧。` +
              "在 Gateway 所在的机器上运行 `komo update`，再 `komo gateway restart`。"
          : `这个 App（${this.appVersion}，协议 ${app}）比 Gateway（${health.version}，协议 ${health.protocol_version}）旧。` +
              "更新 App：菜单「komo → 检查更新…」，或到 GitHub Release 下载新版。",
        { gateway, older },
      );
    }
    if (!this.stored.login) return this.finish("signed-out", undefined, { gateway });
    const target = this.targetSync();
    if (!target) {
      this.clearLogin("已保存的登录读不出来（系统钥匙串变了？），请重新登录。", true);
      return this.finish("signed-out", undefined, { gateway });
    }
    try {
      const response = await request(target, "/v1/sessions");
      void response.body?.cancel();
      if (response.status === 401) {
        this.clearLogin("登录已失效（Gateway 不认这台设备的令牌，可能在别处被吊销了），请重新登录。", true);
        return this.finish("signed-out", undefined, { gateway });
      }
      if (!response.ok) return this.finish("error", `/v1/sessions 返回 HTTP ${response.status}`, { gateway });
    } catch (error) {
      return this.finish("unreachable", errorMessage(error), { gateway });
    }
    return this.finish("ok", undefined, { gateway });
  }

  private finish(status: ConnectionStatus, message: string | undefined, extra: Partial<typeof this.state> = {}): ConnectionInfo {
    this.set({ status, message, ...extra });
    return this.info();
  }

  // ---- 登录 ----

  private prepare(input: SignInInput) {
    const url = normalizeBaseUrl(input.url);
    // 请求头留空且地址没变：沿用已保存的（设置页只知道头的名字，不让人每次重填 service token）。
    const typed = parseExtraHeaders(input.headers);
    const reuse = Object.keys(typed).length === 0 && url === this.stored.url;
    const headers = reuse ? { ...this.secrets?.headers } : typed;
    return { url, headers, target: { base: url, headers } satisfies Target };
  }

  /** 只测不登录。 */
  async test(input: SignInInput): Promise<TestResult> {
    const { target } = this.prepare(input);
    // 已登录同一个地址就带上令牌，顺带测令牌与 SSE。
    const current = this.targetSync();
    const withToken = current && current.base === target.base ? { base: target.base, headers: { ...target.headers, authorization: current.headers.authorization! } } : target;
    return (await probe(withToken, this.build.protocolVersion)).result;
  }

  /** 先测这台 Gateway 能不能登录，再在系统浏览器里打开授权页。登录结果由 `completeLogin` 收。 */
  async signIn(input: SignInInput): Promise<SignInResult> {
    let prepared;
    try {
      prepared = this.prepare(input);
    } catch (error) {
      return { ok: false, error: errorMessage(error) };
    }
    // 加密不可用时拒绝保存令牌，所以先拒绝登录：等换到令牌再发现存不了，服务端就多了一条没人用的设备。
    if (!canEncrypt()) return { ok: false, error: "系统钥匙串不可用（safeStorage），没法安全保存登录令牌，没有开始登录。" };
    const { result, me } = await probe(prepared.target, this.build.protocolVersion);
    if (!result.ok || !me?.oidc_configured) {
      const oidc = result.steps.find((step) => step.name === "oidc" && !step.ok);
      return { ok: false, error: oidc ? "这台 Gateway 没有配置登录，没有开始登录。" : "没测通，没有开始登录。", test: result };
    }
    const { pending, url } = beginLogin(prepared.url, prepared.headers);
    try {
      await shell.openExternal(url);
    } catch (error) {
      return { ok: false, error: `没能打开系统浏览器：${errorMessage(error)}`, test: result };
    }
    this.setPending(pending);
    this.notice = undefined;
    this.emit("changed", this.info());
    return { ok: true, test: result };
  }

  cancelSignIn() {
    if (this.pending) {
      this.setPending(undefined);
      this.emit("changed", this.info());
    }
  }

  private setPending(pending: PendingLogin | undefined) {
    if (this.pendingTimer) clearTimeout(this.pendingTimer);
    this.pending = pending;
    this.pendingTimer = pending
      ? setTimeout(() => {
          if (this.pending !== pending) return;
          this.pending = undefined;
          this.setNotice("登录超时：5 分钟内没有从浏览器回到桌面端。请重新点「登录」。", true);
        }, Math.max(0, pending.expiresAt - Date.now()) + 500)
      : undefined;
  }

  /**
   * 处理 `komo://auth/callback?code&state`：校验 state 与有效期，换设备令牌并保存。
   * 失败的原因记在 `info().notice`（设置页显示），同时返回给调用方。
   */
  async completeLogin(raw: string): Promise<{ ok: true } | { ok: false; error: string }> {
    const fail = (error: string) => {
      this.setNotice(error, true);
      return { ok: false as const, error };
    };
    const callback = parseCallback(raw);
    if (!callback) return fail("回跳地址不是 komo://auth/callback，已忽略。");
    const pending = this.pending;
    if (!pending) {
      return fail("没有正在进行的登录：可能已超过 5 分钟，或登录途中 App 重启过（授权码只能在发起登录的那次运行里用）。请重新点「登录」。");
    }
    if (!callback.state || !sameState(callback.state, pending.state)) {
      // 不清 pending：可能是一个过期标签页的回跳，真正的那次还在等。
      return fail("回跳的 state 对不上这次登录，已放弃这条回跳（可能是过期的登录页，也可能不是你发起的）。如果你刚点过「登录」，请在新打开的浏览器页里完成。");
    }
    if (Date.now() > pending.expiresAt) {
      this.setPending(undefined);
      return fail("登录超时：超过 5 分钟才回到桌面端。请重新点「登录」。");
    }
    // 无论后面成败，这次的 state 与 verifier 都作废（授权码本来也只能用一次）。
    this.setPending(undefined);
    if (!callback.code) return fail("回跳里没有授权码（code），登录没有完成。请重新登录。");

    let issued;
    try {
      issued = await redeemCode(pending, callback.code);
    } catch (error) {
      return fail(`换取设备令牌失败：${errorMessage(error)}`);
    }
    const previous = { target: this.targetSync(), sessionId: this.stored.login?.sessionId };
    try {
      saveStored(
        pending.base,
        { token: issued.token, headers: pending.headers },
        { sessionId: issued.sessionId, subject: issued.subject, ...(issued.email ? { email: issued.email } : {}) },
      );
    } catch (error) {
      // 令牌已经发出去了，存不下就别让它留在服务端。
      void revokeSession({ base: pending.base, headers: { ...pending.headers, authorization: `Bearer ${issued.token}` } }, issued.sessionId);
      return fail(`登录成功但没能保存：${errorMessage(error)}。这次发的设备令牌已请求吊销。`);
    }
    this.reload();
    this.notice = undefined;
    // 换了账号 / 重新登录：旧的这台设备令牌不再有用，顺手吊销，免得设备清单里留一条。
    if (previous.target && previous.sessionId) void revokeSession(previous.target, previous.sessionId);
    return { ok: true };
  }

  // ---- 登录失效 / 退出 ----

  /** 清本地登录状态，保留地址与额外请求头（再登录不用重填）。 */
  private clearLogin(notice: string | undefined, error: boolean) {
    const url = this.stored.url;
    const headers = this.secrets?.headers ?? {};
    if (url) {
      try {
        saveStored(url, { token: "", headers });
      } catch {
        // 写不了（钥匙串不可用）：宁可删掉整个文件，也不留一个作废的令牌。
        try {
          removeStored();
        } catch {
          // 删不掉也只能清内存。
        }
      }
    }
    this.stored = url ? { url } : {};
    this.secrets = { token: "", headers };
    this.setNotice(notice, error);
  }

  /**
   * 转发 / 轮询时拿到 401：这台设备的令牌失效了（在别处被吊销、Gateway 重置了登录表）。
   * `used` 是发出那条请求时的目标：期间已经重新登录过的话，旧请求的 401 不该把新登录清掉。
   */
  handleUnauthorized(used: Target) {
    const current = this.targetSync();
    if (!current || current.headers.authorization !== used.headers.authorization) return;
    this.clearLogin("登录已失效（Gateway 返回 401，这台设备的令牌可能在别处被吊销了），请重新登录。", true);
    this.set({ status: "signed-out" });
    this.emit("signedOut");
  }

  /** 吊销服务端的设备令牌，再清本地。网络失败也清本地，结果里说明服务端可能没吊销。 */
  async signOut(): Promise<SignOutResult> {
    const login = this.stored.login;
    const target = this.targetSync();
    let warning: string | undefined;
    if (login && target) {
      const revoked = await revokeSession(target, login.sessionId);
      if (!revoked.ok) {
        warning = `没能通知 Gateway 吊销这台设备（${revoked.message}）：本地已退出登录，但服务端的令牌可能仍然有效。请在其他已登录的端（网页、komo 命令行）的设备清单里把这台设备移除。`;
      }
    }
    this.cancelSignIn();
    this.clearLogin(warning ?? "已退出登录，服务端已吊销这台设备的令牌。", Boolean(warning));
    this.set({ status: "signed-out" });
    return { ok: true, revoked: !warning, ...(warning ? { warning } : {}) };
  }
}
