// 主进程、preload 与界面共用的类型（只有类型，没有运行时代码）。
// `window.komoDesktop` 的形状就是 KomoDesktopApi；给 apps/client 的约定见 docs/clients.md §6.3。

export type ConnectionMode = "local" | "remote";

/** connecting：正在探测或拉起；ok：可用；其余都停在提示页。 */
export type ConnectionStatus = "connecting" | "ok" | "unreachable" | "unauthorized" | "mismatch" | "error";

export interface ConnectionInfo {
  mode: ConnectionMode;
  status: ConnectionStatus;
  /** 当前 Gateway 的地址（不含令牌）。 */
  baseUrl?: string;
  /** 给人看的说明：为什么停在这里、下一步做什么。 */
  message?: string;
  gateway?: { version: string; protocolVersion: number; instanceId: string };
  app: { version: string; protocolVersion: number };
  /** status 为 mismatch 时，哪一边旧。 */
  older?: "gateway" | "app";
  /** 远程配置的现状。令牌与额外请求头的值永远不会出现在这里，只给名字。 */
  remote?: { url: string; headerNames: string[] };
}

export interface RemoteInput {
  url: string;
  token: string;
  /** 额外请求头，每行 `Name: value`。 */
  headers: string;
}

export interface TestStep {
  name: "health" | "protocol" | "auth" | "sse";
  ok: boolean;
  message: string;
}

export interface TestResult {
  ok: boolean;
  steps: TestStep[];
}

export interface SaveResult {
  ok: boolean;
  /** 保存失败（或没测通）的原因。成功时界面会被主进程整页重载，收不到这个结果。 */
  error?: string;
  test?: TestResult;
}

export interface NotifyInput {
  title: string;
  body?: string;
  /** 点击通知后打开哪个会话。 */
  session?: string;
}

export type WindowAction = "minimize" | "toggle-maximize" | "toggle-fullscreen" | "close";

export interface KomoDesktopApi {
  /** 接口版本；以后有不兼容改动再加。 */
  readonly apiVersion: 1;
  readonly platform: string;
  readonly appVersion: string;
  /** 打包时 kernel 的 PROTOCOL_VERSION。 */
  readonly protocolVersion: number;
  connection: {
    get(): Promise<ConnectionInfo>;
    /** 只测不存：HTTP、协议版本、令牌、SSE。 */
    test(input: RemoteInput): Promise<TestResult>;
    /** 测通后保存并切换到远程；成功则整页重载。 */
    saveRemote(input: RemoteInput): Promise<SaveResult>;
    /** 切回本机并整页重载。 */
    useLocal(): Promise<SaveResult>;
    /** 重新探测（本机模式会再试着拉起 Gateway）并整页重载。 */
    retry(): Promise<void>;
    /** 跳到壳自带的连接设置页（离开 SPA）。 */
    openSettings(): Promise<void>;
  };
  /** 发一条系统通知（不依赖渲染进程的 Notification 权限）。 */
  notify(input: NotifyInput): Promise<void>;
  window: {
    do(action: WindowAction): Promise<void>;
  };
  /** 全局快捷键被按下：界面该打开 ⌘K。返回取消订阅函数。 */
  onCommandPalette(handler: () => void): () => void;
  /** 深链或通知点击要打开某个会话。登记之后主进程才改用它而不是整页跳转。 */
  onOpenSession(handler: (sessionId: string) => void): () => void;
}
