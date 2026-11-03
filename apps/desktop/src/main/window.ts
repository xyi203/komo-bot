import { BrowserWindow, session, shell } from "electron";
import path from "node:path";
import { channels } from "./channels";
import { APP_ORIGIN } from "./protocol";
import type { BuildInfo } from "./paths";

export type PageKind = "starting" | "settings" | "mismatch" | "error";

/** 主窗口：只加载 app://komo/*。导航、弹窗、权限都在这里收紧。 */
export class WindowManager {
  private win: BrowserWindow | undefined;
  /** 界面是否登记了 onOpenSession：登记了就用事件打开会话（不重载页面），否则整页跳转。 */
  private sessionListening = false;
  private paletteQueued = false;
  /** 真要退出了（菜单退出、⌘Q、重启装更新）：这时关窗口才真的销毁它。 */
  private quitting = false;

  constructor(private readonly build: BuildInfo, private readonly appVersion: string) {
    this.lockDownSession();
  }

  get window(): BrowserWindow | undefined {
    return this.win && !this.win.isDestroyed() ? this.win : undefined;
  }

  private lockDownSession() {
    const ses = session.defaultSession;
    // 渲染进程不需要任何浏览器权限（通知由主进程发）。
    ses.setPermissionRequestHandler((_wc, _permission, callback) => callback(false));
    ses.setPermissionCheckHandler(() => false);
    // 除了 app://、DevTools 与内联资源，渲染进程发不出任何网络请求（CSP 之外再加一道）。
    ses.webRequest.onBeforeRequest((details, callback) => {
      const scheme = details.url.slice(0, details.url.indexOf(":") + 1);
      callback({ cancel: !["app:", "devtools:", "data:", "blob:", "chrome-extension:"].includes(scheme) });
    });
  }

  ensure(): BrowserWindow {
    const existing = this.window;
    if (existing) return existing;
    const win = new BrowserWindow({
      width: 1280,
      height: 820,
      minWidth: 640,
      minHeight: 480,
      show: false,
      backgroundColor: "#ffffff",
      // macOS 不要系统标题栏：红绿灯嵌进界面 48px 高的顶栏，界面自己留位置、自己标拖动区（apps/client styles.css）
      ...(process.platform === "darwin" ? { titleBarStyle: "hidden" as const, trafficLightPosition: { x: 18, y: 17 } } : {}),
      webPreferences: {
        preload: path.resolve(__dirname, "../preload/index.js"),
        contextIsolation: true,
        nodeIntegration: false,
        sandbox: true,
        webSecurity: true,
        allowRunningInsecureContent: false,
        additionalArguments: [
          `--komo-app-version=${this.appVersion}`,
          `--komo-protocol-version=${this.build.protocolVersion}`,
        ],
      },
    });
    this.win = win;
    win.once("ready-to-show", () => win.show());
    // 关窗口只是藏起来：App 还在菜单栏里，再打开（Dock、启动台、托盘）就是原来的页面。
    // 真销毁的话，show() 里新建的窗口什么都没加载，是一片空白。
    win.on("close", (event) => {
      if (this.quitting) return;
      event.preventDefault();
      if (win.isFullScreen()) {
        // 全屏下直接 hide 会留下一块黑屏的桌面空间：先退出全屏再藏
        win.once("leave-full-screen", () => win.hide());
        win.setFullScreen(false);
      } else {
        win.hide();
      }
    });
    win.on("closed", () => {
      this.win = undefined;
      this.sessionListening = false;
    });

    const wc = win.webContents;
    // 导航只允许 app://komo；其他地址（外链）交给系统浏览器，页面本身不跳。
    wc.on("will-navigate", (event, url) => {
      if (isAppUrl(url)) return;
      event.preventDefault();
      openExternal(url);
    });
    wc.on("will-redirect", (event, url) => {
      if (!isAppUrl(url)) event.preventDefault();
    });
    wc.setWindowOpenHandler(({ url }) => {
      openExternal(url);
      return { action: "deny" };
    });
    wc.on("will-attach-webview", (event) => event.preventDefault());
    wc.on("did-start-navigation", (_event, _url, isInPlace, isMainFrame) => {
      // 整页换了才要让界面重新登记；SPA 内部路由跳转（isInPlace）不算。
      if (isMainFrame && !isInPlace) this.sessionListening = false;
    });
    wc.on("did-finish-load", () => {
      if (this.paletteQueued && this.onSpa()) {
        this.paletteQueued = false;
        wc.send(channels.commandPalette);
      }
    });
    return win;
  }

  /** 当前显示的是 SPA（而不是壳自带的提示页）。 */
  onSpa(): boolean {
    const url = this.window?.webContents.getURL() ?? "";
    return url.startsWith(`${APP_ORIGIN}/`) && !url.startsWith(`${APP_ORIGIN}/_desktop/`);
  }

  markSessionListening() {
    this.sessionListening = true;
  }

  markQuitting() {
    this.quitting = true;
  }

  show() {
    const win = this.ensure();
    if (win.isMinimized()) win.restore();
    win.show();
    win.focus();
  }

  toggle() {
    const win = this.window;
    if (win?.isVisible() && win.isFocused()) win.hide();
    else this.show();
  }

  loadPage(kind: PageKind) {
    void this.ensure().loadURL(`${APP_ORIGIN}/_desktop/page.html?kind=${kind}`);
  }

  loadApp(pathname = "/") {
    void this.ensure().loadURL(`${APP_ORIGIN}${pathname}`);
  }

  /** 整页重载到 SPA 根：切换连接后用它清掉 Query 缓存与全部前端状态。 */
  reloadApp() {
    this.loadApp("/");
  }

  openSession(id: string) {
    this.show();
    const wc = this.ensure().webContents;
    if (this.onSpa() && this.sessionListening && !wc.isLoading()) wc.send(channels.openSession, id);
    else this.loadApp(`/sessions/${encodeURIComponent(id)}`);
  }

  sendCommandPalette() {
    this.show();
    const wc = this.ensure().webContents;
    if (this.onSpa() && !wc.isLoading()) wc.send(channels.commandPalette);
    else this.paletteQueued = true;
  }
}

export function isAppUrl(url: string): boolean {
  return url.startsWith(`${APP_ORIGIN}/`) || url === APP_ORIGIN;
}

/** 外链只放 http(s) 与 mailto；其他协议（file:、自定义协议）不交给系统。 */
export function openExternal(url: string) {
  try {
    const parsed = new URL(url);
    if (["http:", "https:", "mailto:"].includes(parsed.protocol)) void shell.openExternal(url);
  } catch {
    // 不是 URL：忽略。
  }
}
