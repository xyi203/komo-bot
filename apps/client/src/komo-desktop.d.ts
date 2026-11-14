// C2（Electron 壳）注入的 `window.komoDesktop`：没有桌面壳时不存在，所以每一项都是可选的。
interface KomoDesktop {
  /** 主进程的 `process.platform`（darwin / win32 / linux）。 */
  platform?: string;
  /** ⌘K 菜单命令；返回取消订阅的函数。 */
  onCommandPalette?: (listener: () => void) => () => void;
  /** ⌘, 或菜单栏图标的"设置…"：打开设置弹窗，section 是要停的那一节。返回取消订阅的函数。 */
  onOpenSettings?: (listener: (section?: string) => void) => () => void;
  connection?: {
    /** 跳到壳自带的连接设置页（离开 SPA）：地址、额外请求头、登录。未登录时网页只提示一句，登录在那里做。 */
    openSettings(): Promise<void> | void;
    get?(): Promise<KomoConnectionInfo>;
    /** 切换本机 / 远程（远程的地址与登录切走时保留）；切完壳会重新探测并整页重载。 */
    setMode?(mode: "local" | "remote"): Promise<void>;
    /** 只测不登录。 */
    test?(input: KomoSignInInput): Promise<KomoTestResult>;
    signIn?(input: KomoSignInInput): Promise<{ ok: boolean; error?: string; test?: KomoTestResult }>;
    cancelSignIn?(): Promise<void>;
    /** 吊销设备令牌、清本地登录状态并回到连接设置页（桌面壳里退出走它，不走 `POST /auth/logout`：那条只认 cookie）。 */
    signOut?(): Promise<{ ok: boolean; revoked: boolean; warning?: string }>;
    retry?(): Promise<void>;
  };
  /** 右栏浏览器（apps/desktop/src/main/browser.ts）：网页是一块盖在占位框上的原生视图，界面只画地址栏与按钮。 */
  browser?: {
    state(): Promise<KomoBrowserState>;
    /** 占位框的位置（CSS 像素，相对窗口内容区）；null 藏起来。 */
    setBounds(rect: { x: number; y: number; width: number; height: number } | null): Promise<void>;
    navigate(input: string): Promise<{ ok: boolean; error?: string }>;
    act(action: "back" | "forward" | "reload" | "stop"): Promise<void>;
    capture(): Promise<{ png: Uint8Array; width: number; height: number; url: string; title: string }>;
    detectDevServers(): Promise<{ url: string; port: number; title: string }[]>;
    onState(listener: (state: KomoBrowserState) => void): () => void;
  };
}

interface KomoConnectionInfo {
  /** 本机：用发现文件里的本机令牌连这台电脑上的 Gateway，不登录；远程：OIDC 设备登录。老版本的壳没有这一项（只有远程）。 */
  mode?: "local" | "remote";
  /** 本机数据目录（`KOMO_HOME`）。 */
  local?: { home: string };
  /** `signed-out`：还没登录或登录失效，桌面端停在连接设置页。 */
  status: "connecting" | "ok" | "signed-out" | "unreachable" | "mismatch" | "error";
  baseUrl?: string;
  message?: string;
  /** 已填的地址与额外请求头的名字；请求头的值永远不给。 */
  remote?: { url: string; headerNames: string[] };
  /** 已登录的账号；令牌不给界面。 */
  account?: { subject: string; email?: string };
  /** 系统浏览器里的登录还在等。 */
  signingIn?: boolean;
  /** 最近一次登录 / 退出 / 登录失效的结果。 */
  notice?: { text: string; error: boolean };
}

interface KomoSignInInput {
  url: string;
  /** 每行 `Name: value`。 */
  headers: string;
}

interface KomoTestResult {
  ok: boolean;
  steps: { name: "health" | "protocol" | "oidc" | "auth" | "sse"; ok: boolean; message: string }[];
}

interface KomoBrowserState {
  url: string;
  title: string;
  canGoBack: boolean;
  canGoForward: boolean;
  loading: boolean;
  error?: string;
}

interface Window {
  komoDesktop?: KomoDesktop;
}
