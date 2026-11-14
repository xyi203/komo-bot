// 主进程、preload 与界面共用的类型（只有类型，没有运行时代码）。
// `window.komoDesktop` 的形状就是 KomoDesktopApi；给 apps/client 的约定见 docs/clients.md §6.3。

/**
 * connecting：正在探测；ok：可用；signed-out：还没登录或登录失效，停在连接设置页；其余停在提示页。
 */
export type ConnectionStatus = "connecting" | "ok" | "signed-out" | "unreachable" | "mismatch" | "error";

export interface ConnectionInfo {
  status: ConnectionStatus;
  /** 当前 Gateway 的地址（已登录时才有，不含令牌）。 */
  baseUrl?: string;
  /** 给人看的说明：为什么停在这里、下一步做什么。 */
  message?: string;
  gateway?: { version: string; protocolVersion: number; instanceId: string };
  app: { version: string; protocolVersion: number };
  /** status 为 mismatch 时，哪一边旧。 */
  older?: "gateway" | "app";
  /** 已填的 Gateway 地址与额外请求头的名字（值永远不会出现在这里）。 */
  remote?: { url: string; headerNames: string[] };
  /** 已登录的账号；令牌与 session_id 不给界面。 */
  account?: { subject: string; email?: string };
  /** 系统浏览器里的登录还在等（最长 5 分钟）。 */
  signingIn?: boolean;
  /** 最近一次登录 / 退出 / 登录失效的结果，设置页显示；error 为真时按错误样式。 */
  notice?: { text: string; error: boolean };
}

export interface SignInInput {
  /** Gateway 地址：https，回环地址（127.0.0.1 / localhost / [::1]）允许 http。 */
  url: string;
  /** 额外请求头，每行 `Name: value`。留空且地址没变时沿用已保存的。 */
  headers: string;
}

export interface TestStep {
  name: "health" | "protocol" | "oidc" | "auth" | "sse";
  ok: boolean;
  message: string;
}

export interface TestResult {
  ok: boolean;
  steps: TestStep[];
}

export interface SignInResult {
  ok: boolean;
  /** 没能开始登录的原因。成功只表示浏览器已打开；登录结果之后由主进程整页重载通知。 */
  error?: string;
  test?: TestResult;
}

export interface SignOutResult {
  ok: boolean;
  /** 服务端确认吊销了这台设备的令牌。 */
  revoked: boolean;
  /** 没吊销成功时的说明（本地登录状态照样清了）。 */
  warning?: string;
}

export interface NotifyInput {
  title: string;
  body?: string;
  /** 点击通知后打开哪个会话。 */
  session?: string;
}

export type WindowAction = "minimize" | "toggle-maximize" | "toggle-fullscreen" | "close";

/** 右栏浏览器当前页。没打开过网页时 url 为空。 */
export interface BrowserState {
  url: string;
  title: string;
  canGoBack: boolean;
  canGoForward: boolean;
  loading: boolean;
  /** 主框架没加载成功的原因。 */
  error?: string;
}

/** 界面上浏览器占位框的位置（CSS 像素，相对窗口内容区左上角）。 */
export interface BrowserRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export type BrowserAction = "back" | "forward" | "reload" | "stop";

export interface BrowserCapture {
  png: Uint8Array;
  width: number;
  height: number;
  url: string;
  title: string;
}

export interface DevServer {
  url: string;
  port: number;
  /** 首页的 <title>；不是 HTML 或没有就是空串。 */
  title: string;
}

export interface KomoDesktopApi {
  /** 接口版本；以后有不兼容改动再加。 */
  readonly apiVersion: 1;
  readonly platform: string;
  readonly appVersion: string;
  /** 打包时 kernel 的 PROTOCOL_VERSION。 */
  readonly protocolVersion: number;
  connection: {
    get(): Promise<ConnectionInfo>;
    /** 只测不登录：HTTP、协议版本、这台 Gateway 配没配登录（已登录时还测令牌与 SSE）。 */
    test(input: SignInInput): Promise<TestResult>;
    /** 测通后在系统浏览器里打开登录页；完成登录后系统会唤回桌面端（komo://auth/callback），主进程整页重载。 */
    signIn(input: SignInInput): Promise<SignInResult>;
    /** 放弃正在等的登录。 */
    cancelSignIn(): Promise<void>;
    /** 吊销设备令牌并清本地登录状态，然后回到连接设置页（网络失败也清，结果里有 warning）。 */
    signOut(): Promise<SignOutResult>;
    /** 重新探测并整页重载。 */
    retry(): Promise<void>;
    /** 跳到壳自带的连接设置页（离开 SPA）。 */
    openSettings(): Promise<void>;
  };
  /** 发一条系统通知（不依赖渲染进程的 Notification 权限）。 */
  notify(input: NotifyInput): Promise<void>;
  window: {
    do(action: WindowAction): Promise<void>;
  };
  /** 右栏浏览器：网页是一块盖在占位框上的原生视图，与界面隔离（单独 session、无 preload）。 */
  browser: {
    state(): Promise<BrowserState>;
    /** 报占位框的位置；null 藏起来。 */
    setBounds(rect: BrowserRect | null): Promise<void>;
    /** 地址栏输入；没写协议的自动补，只认 http(s)。 */
    navigate(input: string): Promise<{ ok: boolean; error?: string }>;
    act(action: BrowserAction): Promise<void>;
    /** 截当前可见的一屏（PNG）。 */
    capture(): Promise<BrowserCapture>;
    /** 扫本机常用端口上的 dev server。 */
    detectDevServers(): Promise<DevServer[]>;
    /** 地址、标题、加载状态变了。返回取消订阅函数。 */
    onState(handler: (state: BrowserState) => void): () => void;
  };
  /** 全局快捷键被按下：界面该打开 ⌘K。返回取消订阅函数。 */
  onCommandPalette(handler: () => void): () => void;
  /** ⌘, 或菜单栏图标的"设置…"：界面该打开设置弹窗（section 是要停的那一节，如 gateway）。返回取消订阅函数。 */
  onOpenSettings(handler: (section?: string) => void): () => void;
  /** 深链或通知点击要打开某个会话。登记之后主进程才改用它而不是整页跳转。 */
  onOpenSession(handler: (sessionId: string) => void): () => void;
}
