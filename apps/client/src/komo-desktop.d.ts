// C2（Electron 壳）注入的 `window.komoDesktop`：没有桌面壳时不存在，所以每一项都是可选的。
interface KomoDesktop {
  /** 主进程的 `process.platform`（darwin / win32 / linux）。 */
  platform?: string;
  /** ⌘K 菜单命令；返回取消订阅的函数。 */
  onCommandPalette?: (listener: () => void) => () => void;
  /** ⌘, 或菜单栏图标的"设置…"：打开设置弹窗，section 是要停的那一节。返回取消订阅的函数。 */
  onOpenSettings?: (listener: (section?: string) => void) => () => void;
  connection?: {
    /** 打开桌面壳的"连接的 Gateway"设置（本机 / 远程）。连不上时的提示页还在用；设置弹窗里直接用下面几项。 */
    openSettings(): void;
    get?(): Promise<KomoConnectionInfo>;
    test?(input: KomoRemoteInput): Promise<KomoTestResult>;
    /** 测通才保存；成功后主进程整页重载，收不到结果。 */
    saveRemote?(input: KomoRemoteInput): Promise<{ ok: boolean; error?: string; test?: KomoTestResult }>;
    useLocal?(): Promise<{ ok: boolean; error?: string }>;
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
  mode: "local" | "remote";
  status: string;
  baseUrl?: string;
  message?: string;
  /** 令牌与请求头的值永远不给，只有名字。 */
  remote?: { url: string; headerNames: string[] };
}

interface KomoRemoteInput {
  url: string;
  token: string;
  /** 每行 `Name: value`。 */
  headers: string;
}

interface KomoTestResult {
  ok: boolean;
  steps: { name: string; ok: boolean; message: string }[];
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
