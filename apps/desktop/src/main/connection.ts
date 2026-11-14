import { shell } from "electron";
import { EventEmitter } from "node:events";
import type {
  ConnectionInfo,
  ConnectionMode,
  ConnectionStatus,
  SignInInput,
  SignInResult,
  SignOutResult,
  TestResult,
} from "../shared/api";
import { beginLogin, parseCallback, redeemCode, revokeSession, sameState, type PendingLogin } from "./auth";
import { LocalDiscovery, discoveryFile, launchGateway } from "./local";
import { komoHome, type BuildInfo } from "./paths";
import {
  canEncrypt,
  loadStored,
  readSecrets,
  removeStored,
  saveMode,
  saveStored,
  type Secrets,
  type StoredConnection,
} from "./store";
import { errorMessage, fetchHealth, normalizeBaseUrl, parseExtraHeaders, probe, request, type Health, type Target } from "./upstream";

/** 拉起本机 Gateway 后最多等多久就绪（与命令行 `komo gateway` 的等待同量级）。 */
const LAUNCH_WAIT_MS = 30_000;
const LAUNCH_POLL_MS = 500;

/**
 * 当前连接，两种：
 * - **本机**（默认）：读 `$KOMO_HOME/runtime/gateway.json` 的地址与本机令牌，不登录；Gateway 没在跑就拉起。
 *   令牌每次 Gateway 启动都换，所以每次转发前按文件的 mtime 判断要不要重读（`LocalDiscovery`）。
 * - **远程**：地址 + 设备令牌（OIDC 设备登录换来的）+ 可选的额外请求头。
 * 渲染进程永远不知道令牌——它只对 app://komo 说话，主进程的协议处理器用 `targetSync()` 转发。
 * 远程登录只在主进程里发生：起（`signIn`）→ 系统浏览器 → 深链回来（`completeLogin`）。
 * 切到本机时远程的地址与登录原样保留，切回去不用重新登录。
 */
export class ConnectionManager extends EventEmitter<{ changed: [ConnectionInfo]; signedOut: []; lost: [] }> {
  private stored: StoredConnection = loadStored();
  /** 解密后缓存在内存：每次转发都解一次会反复碰钥匙串。 */
  private secrets: Secrets | undefined = readSecrets(this.stored);
  private readonly discovery = new LocalDiscovery();
  private launching: Promise<{ ok: true } | { ok: false; message: string }> | undefined;
  private pending: PendingLogin | undefined;
  private pendingTimer: ReturnType<typeof setTimeout> | undefined;
  private notice: ConnectionInfo["notice"];
  private state: Pick<ConnectionInfo, "status" | "message" | "gateway" | "older"> = { status: "connecting" };

  constructor(private readonly build: BuildInfo, private readonly appVersion: string) {
    super();
  }

  get mode(): ConnectionMode {
    return this.stored.mode;
  }

  info(): ConnectionInfo {
    const { mode, url, login } = this.stored;
    const baseUrl = mode === "local" ? this.localTarget()?.base : login ? url : undefined;
    return {
      mode,
      ...this.state,
      ...(baseUrl ? { baseUrl } : {}),
      ...(mode === "local" ? { local: { home: komoHome() } } : {}),
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

  /** 切换本机 / 远程。调用方（index.ts）随后重新探测并换页。 */
  setMode(mode: ConnectionMode) {
    if (mode !== "local" && mode !== "remote") throw new Error(`未知的连接方式：${String(mode)}`);
    if (mode === this.stored.mode) return;
    saveMode(mode);
    this.reload();
    this.notice = undefined;
    this.emit("changed", this.info());
  }

  /** 本机：发现文件里的地址与本机令牌。Gateway 没在跑（没有发现文件）就是 undefined。 */
  private localTarget(): Target | undefined {
    const found = this.discovery.current();
    if (!found) return undefined;
    return {
      base: found.base_url.replace(/\/+$/, ""),
      headers: found.token ? { authorization: `Bearer ${found.token}` } : {},
    };
  }

  /** 远程登录前也能用的目标（只有额外请求头）：探活、`/auth/me`。没填地址就是 undefined。 */
  private remoteBase(): Target | undefined {
    if (!this.stored.url) return undefined;
    return { base: this.stored.url, headers: { ...this.secrets?.headers } };
  }

  /** 远程已登录的目标；没登录（或令牌读不出来）就是 undefined。与当前模式无关（退出、换账号时要用）。 */
  private remoteTarget(): Target | undefined {
    const { url, login } = this.stored;
    if (!url || !login || !this.secrets?.token) return undefined;
    // 额外请求头在前、Authorization 在后：写入时已经拒绝了保留名，这里再保证一次顺序。
    return { base: url, headers: { ...this.secrets.headers, authorization: `Bearer ${this.secrets.token}` } };
  }

  /** 转发要用的目标（按当前模式）；还没有就是 undefined。 */
  targetSync(): Target | undefined {
    return this.stored.mode === "local" ? this.localTarget() : this.remoteTarget();
  }

  /**
   * 探测并核对协议版本与令牌，结果落在 `info()`。
   * 本机：没在跑就拉起再等就绪。远程：没填地址、没登录、登录失效都是 `signed-out`，窗口停在连接设置页。
   */
  async connect(): Promise<ConnectionInfo> {
    this.set({ status: "connecting" });
    return this.stored.mode === "local" ? this.connectLocal() : this.connectRemote();
  }

  private async connectLocal(): Promise<ConnectionInfo> {
    let health = await this.localHealth();
    if (!health) {
      this.set({ status: "connecting", message: "本机 Gateway 没在跑，正在启动…" });
      // 并发的 connect（重试按钮连点、切换模式）共用同一次拉起。
      this.launching ??= launchGateway().finally(() => {
        this.launching = undefined;
      });
      const launched = await this.launching;
      if (!launched.ok) return this.finish("unreachable", launched.message);
      for (let waited = 0; waited < LAUNCH_WAIT_MS && !health; waited += LAUNCH_POLL_MS) {
        await new Promise((resolve) => setTimeout(resolve, LAUNCH_POLL_MS));
        health = await this.localHealth();
      }
    }
    if (!health) {
      return this.finish(
        "unreachable",
        `连不上本机 Gateway（发现文件 ${discoveryFile()}）。在终端运行 \`komo gateway status\` 看看，或在连接设置里改用远程。`,
      );
    }
    const gateway = this.gatewayOf(health);
    const mismatch = this.checkProtocol(health);
    if (mismatch) return mismatch;
    const target = this.localTarget();
    if (!target) return this.finish("unreachable", "本机 Gateway 的发现文件刚刚不见了（Gateway 正在退出？），请重试。", { gateway });
    try {
      const response = await request(target, "/v1/sessions");
      void response.body?.cancel();
      if (response.status === 401) {
        return this.finish(
          "error",
          `本机 Gateway 不认 ${discoveryFile()} 里的令牌：多半是有两个数据目录（KOMO_HOME 不一致），或 Gateway 正在重启。请重试。`,
          { gateway },
        );
      }
      if (!response.ok) return this.finish("error", `/v1/sessions 返回 HTTP ${response.status}`, { gateway });
    } catch (error) {
      return this.finish("unreachable", errorMessage(error), { gateway });
    }
    return this.finish("ok", undefined, { gateway });
  }

  private async localHealth(): Promise<Health | undefined> {
    const target = this.localTarget();
    if (!target) return undefined;
    try {
      return await fetchHealth(target);
    } catch {
      return undefined;
    }
  }

  private async connectRemote(): Promise<ConnectionInfo> {
    const base = this.remoteBase();
    if (!base) return this.finish("signed-out", undefined);
    let health: Health;
    try {
      health = await fetchHealth(base);
    } catch (error) {
      return this.finish("unreachable", `连不上 Gateway：${errorMessage(error)}`);
    }
    const gateway = this.gatewayOf(health);
    const mismatch = this.checkProtocol(health);
    if (mismatch) return mismatch;
    if (!this.stored.login) return this.finish("signed-out", undefined, { gateway });
    const target = this.remoteTarget();
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

  private gatewayOf(health: Health) {
    return { version: health.version, protocolVersion: health.protocol_version, instanceId: health.instance_id };
  }

  /** 协议版本不一致就落成 `mismatch` 并返回；一致返回 undefined。 */
  private checkProtocol(health: Health): ConnectionInfo | undefined {
    const app = this.build.protocolVersion;
    if (health.protocol_version === app) return undefined;
    const local = this.stored.mode === "local";
    const older = health.protocol_version < app ? "gateway" : "app";
    const where = local ? "本机 Gateway" : "Gateway";
    return this.finish(
      "mismatch",
      older === "gateway"
        ? `${where}（${health.version}，协议 ${health.protocol_version}）比这个 App（${this.appVersion}，协议 ${app}）旧。` +
            (local ? "在终端运行 `komo update`，再 `komo gateway restart`。" : "在 Gateway 所在的机器上运行 `komo update`，再 `komo gateway restart`。")
        : `这个 App（${this.appVersion}，协议 ${app}）比${where}（${health.version}，协议 ${health.protocol_version}）旧。` +
            "更新 App：菜单「komo → 检查更新…」，或到 GitHub Release 下载新版。",
      { gateway: this.gatewayOf(health), older },
    );
  }

  private finish(status: ConnectionStatus, message: string | undefined, extra: Partial<typeof this.state> = {}): ConnectionInfo {
    this.set({ status, message, ...extra });
    return this.info();
  }

  // ---- 远程登录 ----

  private prepare(input: SignInInput) {
    const url = normalizeBaseUrl(input.url);
    // 请求头留空且地址没变：沿用已保存的（设置页只知道头的名字，不让人每次重填 service token）。
    const typed = parseExtraHeaders(input.headers);
    const reuse = Object.keys(typed).length === 0 && url === this.stored.url;
    const headers = reuse ? { ...this.secrets?.headers } : typed;
    return { url, headers, target: { base: url, headers } satisfies Target };
  }

  /** 只测不登录（远程）。 */
  async test(input: SignInInput): Promise<TestResult> {
    const { target } = this.prepare(input);
    // 已登录同一个地址就带上令牌，顺带测令牌与 SSE。
    const current = this.remoteTarget();
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
      return { ok: false, error: oidc ? "这台 Gateway 没有配置登录，没有开始登录。连本机的 Gateway 请改用「本机」。" : "没测通，没有开始登录。", test: result };
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
   * 处理 `komo://auth/callback?code&state`：校验 state 与有效期，换设备令牌并保存，连接方式随之切到远程。
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
    const previous = { target: this.remoteTarget(), sessionId: this.stored.login?.sessionId };
    try {
      saveStored(
        "remote",
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

  /** 清本地的远程登录状态，保留连接方式、地址与额外请求头（再登录不用重填）。 */
  private clearLogin(notice: string | undefined, error: boolean) {
    const { mode, url } = this.stored;
    const headers = this.secrets?.headers ?? {};
    if (url) {
      try {
        saveStored(mode, url, { token: "", headers });
      } catch {
        // 写不了（钥匙串不可用）：宁可删掉整个文件，也不留一个作废的令牌。
        try {
          removeStored();
        } catch {
          // 删不掉也只能清内存。
        }
      }
    }
    this.stored = url ? { mode, url } : { mode };
    this.secrets = { token: "", headers };
    this.setNotice(notice, error);
  }

  /**
   * 转发 / 轮询时拿到 401。`used` 是发出那条请求时的目标：期间令牌已经换过（重新登录、本机 Gateway 重启），
   * 旧请求的 401 不算数。
   * - 远程：这台设备的令牌失效了（在别处被吊销、Gateway 重置了登录表）→ 清登录，回连接设置页。
   * - 本机：发现文件里的令牌就是 Gateway 现在的令牌却仍被拒——不该发生，按连接出错处理，停在提示页可重试。
   */
  handleUnauthorized(used: Target) {
    const current = this.targetSync();
    if (!current || current.headers.authorization !== used.headers.authorization) return;
    if (this.stored.mode === "local") {
      // 并发的请求会一起拿到 401：只在第一条上换页。
      if (this.state.status !== "ok") return;
      this.set({ status: "error", message: `本机 Gateway 不认 ${discoveryFile()} 里的令牌（Gateway 正在重启？）。请重试。` });
      this.emit("lost");
      return;
    }
    this.clearLogin("登录已失效（Gateway 返回 401，这台设备的令牌可能在别处被吊销了），请重新登录。", true);
    this.set({ status: "signed-out" });
    this.emit("signedOut");
  }

  /**
   * 退出远程登录：吊销服务端的设备令牌，再清本地。网络失败也清本地，结果里说明服务端可能没吊销。
   * 本机模式下没有登录可退；保存着的远程登录照样可以在这里退掉。
   */
  async signOut(): Promise<SignOutResult> {
    const login = this.stored.login;
    const target = this.remoteTarget();
    if (!login) {
      this.cancelSignIn();
      return this.stored.mode === "local"
        ? { ok: true, revoked: false, warning: "本机连接不需要登录，没有可退出的账号。" }
        : { ok: true, revoked: false };
    }
    let warning: string | undefined;
    if (target) {
      const revoked = await revokeSession(target, login.sessionId);
      if (!revoked.ok) {
        warning = `没能通知 Gateway 吊销这台设备（${revoked.message}）：本地已退出登录，但服务端的令牌可能仍然有效。请在其他已登录的端（网页、komo 命令行）的设备清单里把这台设备移除。`;
      }
    }
    this.cancelSignIn();
    this.clearLogin(warning ?? "已退出登录，服务端已吊销这台设备的令牌。", Boolean(warning));
    if (this.stored.mode === "remote") this.set({ status: "signed-out" });
    return { ok: true, revoked: !warning, ...(warning ? { warning } : {}) };
  }
}
