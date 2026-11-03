import { EventEmitter } from "node:events";
import type { ConnectionInfo, ConnectionMode, ConnectionStatus, RemoteInput, SaveResult, TestResult } from "../shared/api";
import { launchGateway, readDiscovery, type Discovery } from "./gateway";
import type { BuildInfo } from "./paths";
import { loadStored, readSecrets, saveLocal, saveRemote, type StoredConnection } from "./store";
import {
  errorMessage,
  fetchHealth,
  normalizeBaseUrl,
  parseExtraHeaders,
  probe,
  request,
  type Health,
  type Target,
} from "./upstream";

/**
 * 当前连接：本机（读 runtime/gateway.json）或远程（地址 + 设备令牌 + 额外请求头）。
 * 渲染进程永远不知道令牌——它只对 app://komo 说话，主进程的协议处理器用 `target()` 转发。
 */
export class ConnectionManager extends EventEmitter<{ changed: [ConnectionInfo] }> {
  private stored: StoredConnection = loadStored();
  private discovery: Discovery | undefined;
  private state: Pick<ConnectionInfo, "status" | "message" | "gateway" | "older"> = { status: "connecting" };

  constructor(private readonly build: BuildInfo, private readonly appVersion: string) {
    super();
  }

  get mode(): ConnectionMode {
    return this.stored.mode;
  }

  info(): ConnectionInfo {
    const target = this.targetSync();
    const secrets = this.mode === "remote" ? readSecrets(this.stored) : undefined;
    return {
      mode: this.mode,
      ...this.state,
      baseUrl: target?.base,
      app: { version: this.appVersion, protocolVersion: this.build.protocolVersion },
      ...(this.stored.remote
        ? { remote: { url: this.stored.remote.url, headerNames: Object.keys(secrets?.headers ?? {}) } }
        : {}),
    };
  }

  private set(state: typeof this.state) {
    this.state = state;
    this.emit("changed", this.info());
  }

  /** 转发要用的目标；还没有（本机没发现文件、远程没配）就是 undefined。 */
  targetSync(): Target | undefined {
    if (this.stored.mode === "remote") {
      const secrets = readSecrets(this.stored);
      if (!this.stored.remote || !secrets) return undefined;
      return {
        base: this.stored.remote.url,
        // 额外请求头在前、Authorization 在后：写入时已经拒绝了保留名，这里再保证一次顺序。
        headers: { ...secrets.headers, authorization: `Bearer ${secrets.token}` },
      };
    }
    this.discovery ??= readDiscovery();
    if (!this.discovery) return undefined;
    return {
      base: this.discovery.base_url.replace(/\/+$/, ""),
      headers: this.discovery.token ? { authorization: `Bearer ${this.discovery.token}` } : {},
    };
  }

  /** 本机模式：重读发现文件（Gateway 重启换了令牌或端口）。返回目标是否变了。 */
  reloadLocal(): boolean {
    if (this.stored.mode !== "local") return false;
    const before = JSON.stringify(this.targetSync());
    this.discovery = readDiscovery();
    return JSON.stringify(this.targetSync()) !== before;
  }

  /**
   * 探测并（本机且没在跑时）拉起 Gateway，然后核对协议版本与令牌。结果落在 `info()`。
   * `launch: false` 用于只想看一眼现状（轮询里不再重复拉起）。
   */
  async connect(options: { launch?: boolean } = {}): Promise<ConnectionInfo> {
    const { launch = true } = options;
    this.set({ status: "connecting" });
    if (this.mode === "remote") return this.connectRemote();

    this.discovery = readDiscovery();
    let health = await this.healthOrUndefined();
    if (!health && launch) {
      this.set({ status: "connecting", message: "Gateway 没在跑，正在启动…" });
      const launched = await launchGateway();
      if (!launched.ok) return this.finish("unreachable", launched.message);
      for (let attempt = 0; attempt < 60 && !health; attempt++) {
        await new Promise((resolve) => setTimeout(resolve, 500));
        this.discovery = readDiscovery();
        health = await this.healthOrUndefined();
      }
    }
    if (!health) {
      return this.finish("unreachable", "连不上本机 Gateway。运行 `komo gateway status` 看看，或在连接设置里改用远程。");
    }
    return this.verify(health);
  }

  private async connectRemote(): Promise<ConnectionInfo> {
    const target = this.targetSync();
    if (!target) return this.finish("error", "远程连接的配置读不出来（钥匙串变了？），请在连接设置里重新填写。");
    let health: Health;
    try {
      health = await fetchHealth(target);
    } catch (error) {
      return this.finish("unreachable", `连不上远程 Gateway：${errorMessage(error)}`);
    }
    return this.verify(health);
  }

  private async healthOrUndefined(): Promise<Health | undefined> {
    const target = this.targetSync();
    if (!target) return undefined;
    try {
      return await fetchHealth(target);
    } catch {
      return undefined;
    }
  }

  private async verify(health: Health): Promise<ConnectionInfo> {
    const gateway = { version: health.version, protocolVersion: health.protocol_version, instanceId: health.instance_id };
    const app = this.build.protocolVersion;
    if (health.protocol_version !== app) {
      const older = health.protocol_version < app ? "gateway" : "app";
      const where = this.mode === "local" ? "本机" : "远程";
      return this.finish(
        "mismatch",
        older === "gateway"
          ? `${where} Gateway（${health.version}，协议 ${health.protocol_version}）比这个 App（${this.appVersion}，协议 ${app}）旧。` +
              (this.mode === "local" ? "在终端运行 `komo update`，再 `komo gateway restart`。" : "在 Gateway 所在的机器上运行 `komo update`，再 `komo gateway restart`。")
          : `这个 App（${this.appVersion}，协议 ${app}）比${where} Gateway（${health.version}，协议 ${health.protocol_version}）旧。` +
              "更新 App：菜单「komo → 检查更新…」，或到 GitHub Release 下载新版。",
        { gateway, older },
      );
    }
    const target = this.targetSync();
    try {
      const response = await request(target!, "/v1/sessions");
      if (response.status === 401 && this.mode === "local" && this.reloadLocal()) return this.verify(health);
      if (response.status === 401) {
        return this.finish(
          "unauthorized",
          this.mode === "local" ? "本机 Gateway 拒绝了发现文件里的令牌。" : "远程 Gateway 拒绝了设备令牌，请在连接设置里更新。",
          { gateway },
        );
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

  /** 只测不存（连接设置页的「测试」）。 */
  async test(input: RemoteInput): Promise<TestResult> {
    const { target } = this.prepare(input);
    return (await probe(target, this.build.protocolVersion)).result;
  }

  private prepare(input: RemoteInput) {
    const { url, warning } = normalizeBaseUrl(input.url);
    const token = input.token.trim();
    if (!token) throw new Error("请填设备令牌");
    const headers = parseExtraHeaders(input.headers);
    return { url, warning, token, headers, target: { base: url, headers: { ...headers, authorization: `Bearer ${token}` } } };
  }

  /** 保存前先测 HTTP 与 SSE 都通；没测通不保存。 */
  async saveRemote(input: RemoteInput): Promise<SaveResult> {
    let prepared;
    try {
      prepared = this.prepare(input);
    } catch (error) {
      return { ok: false, error: errorMessage(error) };
    }
    const { result } = await probe(prepared.target, this.build.protocolVersion);
    if (!result.ok) return { ok: false, error: "没测通，没有保存。", test: result };
    try {
      saveRemote(prepared.url, { token: prepared.token, headers: prepared.headers });
    } catch (error) {
      return { ok: false, error: errorMessage(error), test: result };
    }
    this.stored = loadStored();
    this.discovery = undefined;
    return { ok: true, test: result };
  }

  useLocal(): SaveResult {
    try {
      saveLocal(this.stored);
    } catch (error) {
      return { ok: false, error: errorMessage(error) };
    }
    this.stored = loadStored();
    this.discovery = undefined;
    return { ok: true };
  }
}
