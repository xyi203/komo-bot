import { Menu, app, ipcMain, type IpcMainInvokeEvent, type MenuItemConstructorOptions } from "electron";
import { channels } from "./channels";
import type { BrowserAction, BrowserRect, NotifyInput, SignInInput, WindowAction } from "../shared/api";
import { BrowserPane } from "./browser";
import { ConnectionManager } from "./connection";
import { parseCallback } from "./auth";
import { readBuildInfo } from "./paths";
import { installHandler, registerSchemes } from "./protocol";
import { SystemIntegration } from "./system";
import { checkForUpdatesInteractive, setupUpdater } from "./updater";
import { WindowManager, isAppUrl } from "./window";

// ---- 启动前要做的事（都必须在 ready 之前）----
registerSchemes();
// 本机冒烟用：指定 userData，免得和正在用的 App 共享连接配置。要在抢单实例锁之前：锁就放在 userData 里，
// 先抢锁的话会撞上正在用的那个 App，这个直接退出。
if (process.env.KOMO_DESKTOP_USER_DATA) app.setPath("userData", process.env.KOMO_DESKTOP_USER_DATA);
if (!app.requestSingleInstanceLock()) {
  app.quit();
  process.exit(0);
}

const build = readBuildInfo();
let pendingSession: string | undefined;
/** ready 之前（冷启动由深链拉起）收到的登录回跳：等主进程各部件就绪后再处理。 */
let pendingAuth: string | undefined;
let ready = false;

type DeepLink = { kind: "session"; id: string } | { kind: "auth" };

/** `komo://session/<id>` 与登录回跳 `komo://auth/callback`；其他形状一律不认。 */
function parseDeepLink(raw: string): DeepLink | undefined {
  if (parseCallback(raw)) return { kind: "auth" };
  try {
    const url = new URL(raw);
    if (url.protocol !== "komo:" || url.hostname !== "session") return undefined;
    const id = decodeURIComponent(url.pathname.replace(/^\/+/, ""));
    return /^[A-Za-z0-9._-]{1,128}$/.test(id) ? { kind: "session", id } : undefined;
  } catch {
    return undefined;
  }
}

app.on("open-url", (event, url) => {
  event.preventDefault();
  openDeepLink(url);
});

let windows: WindowManager;
let connection: ConnectionManager;
let system: SystemIntegration;
let browser: BrowserPane;

function openDeepLink(raw: string) {
  const link = parseDeepLink(raw);
  if (!link) return;
  if (link.kind === "auth") {
    if (ready) void finishLogin(raw);
    else pendingAuth = raw;
    return;
  }
  const id = link.id;
  if (!windows || connection?.info().status !== "ok") {
    pendingSession = id; // 还没连上：连上后打开
    windows?.show();
    return;
  }
  windows.openSession(id);
}

/** 登录回跳：换令牌；成功进已登录状态，失败停在连接设置页（原因在 info().notice）。 */
async function finishLogin(raw: string) {
  windows.show();
  const result = await connection.completeLogin(raw);
  if (result.ok) await connectAndShow();
  else windows.loadPage("settings");
}

/** 探测连接并把窗口带到该去的页面：通了进 SPA，否则停在对应的提示页。 */
async function connectAndShow() {
  windows.loadPage("starting");
  const info = await connection.connect();
  system.reset();
  if (info.status === "ok") {
    const session = pendingSession;
    pendingSession = undefined;
    windows.loadApp(session ? `/sessions/${encodeURIComponent(session)}` : "/");
  } else {
    windows.loadPage(info.status === "mismatch" ? "mismatch" : info.status === "signed-out" ? "settings" : "error");
  }
}

function trusted(event: IpcMainInvokeEvent): boolean {
  return isAppUrl(event.senderFrame?.url ?? "");
}

function registerIpc() {
  const guard = <Args extends unknown[], R>(handler: (...args: Args) => R | Promise<R>) =>
    (event: IpcMainInvokeEvent, ...args: Args) => {
      if (!trusted(event)) throw new Error("untrusted sender");
      return handler(...args);
    };

  ipcMain.handle(channels.connectionGet, guard(() => connection.info()));
  ipcMain.handle(channels.connectionTest, guard(async (input: SignInInput) => {
    try {
      return await connection.test(input);
    } catch (error) {
      return { ok: false, steps: [{ name: "health", ok: false, message: (error as Error).message }] };
    }
  }));
  ipcMain.handle(channels.connectionSignIn, guard((input: SignInInput) => connection.signIn(input)));
  ipcMain.handle(channels.connectionCancelSignIn, guard(() => connection.cancelSignIn()));
  ipcMain.handle(channels.connectionSignOut, guard(async () => {
    const result = await connection.signOut();
    system.reset();
    setImmediate(() => windows.loadPage("settings"));
    return result;
  }));
  ipcMain.handle(channels.connectionRetry, guard(() => {
    setImmediate(() => void connectAndShow());
  }));
  ipcMain.handle(channels.connectionOpenSettings, guard(() => windows.loadPage("settings")));
  ipcMain.handle(channels.notify, guard((input: NotifyInput) => {
    if (typeof input?.title === "string" && input.title) {
      system.notify(input.title.slice(0, 200), input.body?.slice(0, 500), input.session);
    }
  }));
  ipcMain.handle(channels.window, guard((action: WindowAction) => {
    const win = windows.window;
    if (!win) return;
    if (action === "minimize") win.minimize();
    else if (action === "toggle-maximize") {
      if (win.isMaximized()) win.unmaximize();
      else win.maximize();
    }
    else if (action === "toggle-fullscreen") win.setFullScreen(!win.isFullScreen());
    else if (action === "close") win.close();
  }));
  ipcMain.handle(channels.browserState, guard(() => browser.state()));
  ipcMain.handle(channels.browserBounds, guard((rect: BrowserRect | null) => browser.setBounds(validRect(rect))));
  ipcMain.handle(channels.browserNavigate, guard((input: string) =>
    typeof input === "string" ? browser.navigate(input.slice(0, 4096)) : { ok: false, error: "bad input" }));
  ipcMain.handle(channels.browserAct, guard((action: BrowserAction) => {
    if (["back", "forward", "reload", "stop"].includes(action)) browser.act(action);
  }));
  ipcMain.handle(channels.browserCapture, guard(() => browser.capture()));
  ipcMain.handle(channels.browserDetect, guard(() => browser.detectDevServers()));
  ipcMain.on(channels.openSettingsListening, (event) => {
    if (isAppUrl(event.senderFrame?.url ?? "")) windows.markSettingsListening();
  });
  ipcMain.on(channels.openSessionListening, (event) => {
    if (isAppUrl(event.senderFrame?.url ?? "")) windows.markSessionListening();
  });
}

function validRect(rect: unknown): BrowserRect | null {
  if (!rect || typeof rect !== "object") return null;
  const { x, y, width, height } = rect as Record<string, unknown>;
  const ok = [x, y, width, height].every((n) => typeof n === "number" && Number.isFinite(n));
  return ok ? { x: x as number, y: y as number, width: width as number, height: height as number } : null;
}

function buildMenu() {
  const template: MenuItemConstructorOptions[] = [
    {
      label: app.name,
      submenu: [
        { role: "about" },
        { label: "检查更新… / Check for Updates…", click: () => void checkForUpdatesInteractive() },
        { type: "separator" },
        { label: "设置… / Settings…", accelerator: "CommandOrControl+,", click: () => windows.openSettings() },
        { type: "separator" },
        { role: "hide" },
        { role: "hideOthers" },
        { role: "unhide" },
        { type: "separator" },
        { role: "quit" },
      ],
    },
    { role: "editMenu" },
    { role: "viewMenu" },
    { role: "windowMenu" },
  ];
  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}

app.on("second-instance", (_event, argv) => {
  windows?.show();
  const link = argv.find((arg) => arg.startsWith("komo://"));
  if (link) openDeepLink(link);
});

app.on("window-all-closed", () => {
  // 关窗口不退出：菜单栏图标还在，通知也照常来（macOS 惯例）。
});
app.on("activate", () => windows?.show());
app.on("before-quit", () => {
  windows?.markQuitting();
  system?.stop();
});

void app.whenReady().then(async () => {
  if (app.isPackaged) app.setAsDefaultProtocolClient("komo");
  connection = new ConnectionManager(build, app.getVersion());
  windows = new WindowManager(build, app.getVersion());
  browser = new BrowserPane(() => windows.window);
  system = new SystemIntegration(connection, windows);
  installHandler(connection);
  // 转发或轮询发现令牌失效：本地登录已清，回到登录界面。
  connection.on("signedOut", () => {
    system.reset();
    windows.loadPage("settings");
  });
  registerIpc();
  buildMenu();
  system.start(() => windows.openSettings("gateway"));
  setupUpdater(() => windows.markQuitting());

  // 冷启动带着 komo:// 参数的情况（Windows / Linux 在 argv 里；macOS 的 open-url 事件可能先于 ready，已经记进 pendingSession / pendingAuth）。
  const link = process.argv.find((arg) => arg.startsWith("komo://"));
  if (link) openDeepLink(link);
  ready = true;
  windows.ensure();
  const auth = pendingAuth;
  pendingAuth = undefined;
  if (auth) await finishLogin(auth);
  else await connectAndShow();
});
