// 右栏浏览器：一个 WebContentsView 盖在界面画的占位框上（地址栏、前进后退由界面画，网页本身是这块原生视图）。
// 网页是不可信内容，和界面完全隔开：单独的 session（partition），没有 preload、碰不到 komo 的 IPC，
// 权限一律拒绝，只走 http(s) / ws(s)，下载取消，新窗口改在原地打开。用的是 Electron 自带的 Chromium，不另带浏览器。
import { WebContentsView, session, type BrowserWindow, type Session } from "electron";
import { channels } from "./channels";
import type { BrowserAction, BrowserCapture, BrowserRect, BrowserState, DevServer } from "../shared/api";

const PARTITION = "persist:komo-browser";
/** "检测 dev server" 扫的本机端口：Vite、Next / CRA、Vite preview、Astro、Angular 与几个常见后端。 */
const DEV_PORTS = [5173, 5174, 5175, 3000, 3001, 4173, 4321, 4200, 8080, 8000, 8888];

export class BrowserPane {
  private view: WebContentsView | undefined;
  private host: BrowserWindow | undefined;
  private lockedDown = false;
  private error: string | undefined;

  constructor(private readonly window: () => BrowserWindow | undefined) {}

  state(): BrowserState {
    const wc = this.live()?.webContents;
    if (!wc) return { url: "", title: "", canGoBack: false, canGoForward: false, loading: false };
    return {
      url: wc.getURL(),
      title: wc.getTitle(),
      canGoBack: wc.navigationHistory.canGoBack(),
      canGoForward: wc.navigationHistory.canGoForward(),
      loading: wc.isLoading(),
      ...(this.error ? { error: this.error } : {}),
    };
  }

  /** 界面上占位框的位置（CSS 像素，相对窗口内容区）；null 表示藏起来（关了右栏、开着弹窗、还没打开网页）。 */
  setBounds(rect: BrowserRect | null) {
    const win = this.window();
    const view = this.live();
    if (!win || !view) return;
    if (!rect || rect.width < 1 || rect.height < 1) {
      view.setVisible(false);
      return;
    }
    // 界面可能被 ⌘+ / ⌘- 缩放过：CSS 像素乘上缩放比才是窗口坐标
    const zoom = win.webContents.getZoomFactor();
    view.setBounds({
      x: Math.round(rect.x * zoom),
      y: Math.round(rect.y * zoom),
      width: Math.round(rect.width * zoom),
      height: Math.round(rect.height * zoom),
    });
    view.setVisible(true);
  }

  /** 地址栏的输入：没写协议的，本机地址补 http://，其余补 https://；只认 http(s)。 */
  navigate(input: string): { ok: boolean; error?: string } {
    const url = normalizeUrl(input);
    if (!url) return { ok: false, error: `不是能打开的网址：${input.slice(0, 200)}` };
    const view = this.ensure();
    if (!view) return { ok: false, error: "窗口还没准备好" };
    this.error = undefined;
    view.webContents.loadURL(url).catch(() => {
      // 失败原因由 did-fail-load 记进 state，这里不重复报
    });
    return { ok: true };
  }

  act(action: BrowserAction) {
    const wc = this.live()?.webContents;
    if (!wc) return;
    if (action === "back") wc.navigationHistory.goBack();
    else if (action === "forward") wc.navigationHistory.goForward();
    else if (action === "reload") {
      this.error = undefined;
      wc.reload();
    } else if (action === "stop") wc.stop();
  }

  /** 截当前可见的那一屏。视图藏着时截出来是空的，所以只在显示时截。 */
  async capture(): Promise<BrowserCapture> {
    const view = this.live();
    if (!view || !view.getVisible() || !view.webContents.getURL()) throw new Error("浏览器里没有打开的网页");
    const image = await view.webContents.capturePage();
    if (image.isEmpty()) throw new Error("截到的是空图（网页还没画出来？）");
    const size = image.getSize();
    return { png: image.toPNG(), width: size.width, height: size.height, url: view.webContents.getURL(), title: view.webContents.getTitle() };
  }

  /** 扫本机常用端口，能回 HTTP 的就算一个 dev server；HTML 的顺手取 <title>。 */
  async detectDevServers(): Promise<DevServer[]> {
    const found = await Promise.all(
      DEV_PORTS.map(async (port): Promise<DevServer | undefined> => {
        const url = `http://localhost:${port}/`;
        try {
          const response = await fetch(url, { redirect: "manual", signal: AbortSignal.timeout(800) });
          let title = "";
          if ((response.headers.get("content-type") ?? "").includes("text/html")) {
            title = /<title[^>]*>([^<]{0,120})<\/title>/i.exec(await response.text())?.[1]?.trim() ?? "";
          } else {
            await response.body?.cancel();
          }
          return { url, port, title };
        } catch {
          return undefined;
        }
      }),
    );
    return found.filter((item): item is DevServer => !!item);
  }

  private live(): WebContentsView | undefined {
    return this.view && !this.view.webContents.isDestroyed() ? this.view : undefined;
  }

  private ensure(): WebContentsView | undefined {
    const win = this.window();
    if (!win) return undefined;
    let view = this.live();
    if (!view) {
      view = this.create();
      this.view = view;
      this.host = undefined;
    }
    if (this.host !== win) {
      win.contentView.addChildView(view);
      view.setVisible(false);
      // 主窗口整页换了（重连、去提示页）：浏览器先藏起来，回到 SPA 后界面会重新报位置
      win.webContents.on("did-start-navigation", (_event, _url, isInPlace, isMainFrame) => {
        if (isMainFrame && !isInPlace) this.live()?.setVisible(false);
      });
      this.host = win;
    }
    return view;
  }

  private create(): WebContentsView {
    const view = new WebContentsView({
      webPreferences: {
        session: this.session(),
        contextIsolation: true,
        nodeIntegration: false,
        sandbox: true,
        webSecurity: true,
        allowRunningInsecureContent: false,
      },
    });
    view.setBackgroundColor("#ffffff");
    const wc = view.webContents;
    wc.setWindowOpenHandler(({ url }) => {
      if (isWebUrl(url)) void wc.loadURL(url).catch(() => {});
      return { action: "deny" };
    });
    wc.on("will-navigate", (event, url) => {
      if (!isWebUrl(url)) event.preventDefault();
    });
    wc.on("will-redirect", (event, url) => {
      if (!isWebUrl(url)) event.preventDefault();
    });
    wc.on("will-attach-webview", (event) => event.preventDefault());
    wc.on("did-fail-load", (_event, code, description, url, isMainFrame) => {
      // -3（ERR_ABORTED）是换页或用户停止，不算失败
      if (!isMainFrame || code === -3) return;
      this.error = `${description || code}（${url}）`;
      this.emit();
    });
    wc.on("did-start-navigation", (_event, _url, isInPlace, isMainFrame) => {
      if (isMainFrame && !isInPlace) this.error = undefined;
    });
    const emit = () => this.emit();
    wc.on("did-start-loading", emit);
    wc.on("did-stop-loading", emit);
    wc.on("did-navigate", emit);
    wc.on("did-navigate-in-page", emit);
    wc.on("page-title-updated", emit);
    return view;
  }

  private session(): Session {
    const ses = session.fromPartition(PARTITION);
    if (this.lockedDown) return ses;
    this.lockedDown = true;
    ses.setPermissionRequestHandler((_wc, _permission, callback) => callback(false));
    ses.setPermissionCheckHandler(() => false);
    ses.webRequest.onBeforeRequest((details, callback) => {
      const scheme = details.url.slice(0, details.url.indexOf(":") + 1);
      callback({ cancel: !["http:", "https:", "ws:", "wss:", "data:", "blob:", "about:"].includes(scheme) });
    });
    ses.on("will-download", (event) => event.preventDefault());
    return ses;
  }

  private emit() {
    const wc = this.window()?.webContents;
    if (wc && !wc.isDestroyed()) wc.send(channels.browserStateChanged, this.state());
  }
}

function isWebUrl(url: string): boolean {
  return url.startsWith("http://") || url.startsWith("https://");
}

export function normalizeUrl(input: string): string | undefined {
  const text = input.trim();
  if (!text || /\s/.test(text)) return undefined;
  const local = /^(localhost|127(?:\.\d{1,3}){3}|\[::1\]|0\.0\.0\.0)(:\d+)?(\/|$)/i.test(text);
  const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(text) ? text : `${local ? "http" : "https"}://${text}`;
  try {
    const url = new URL(withScheme);
    return url.protocol === "http:" || url.protocol === "https:" ? url.toString() : undefined;
  } catch {
    return undefined;
  }
}
