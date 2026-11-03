import { Menu, app, ipcMain, type IpcMainInvokeEvent, type MenuItemConstructorOptions } from "electron";
import { channels } from "./channels";
import type { NotifyInput, RemoteInput, WindowAction } from "../shared/api";
import { ConnectionManager } from "./connection";
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

/** `komo://session/<id>` → 会话 id；其他形状一律不认。 */
function parseDeepLink(raw: string): string | undefined {
  try {
    const url = new URL(raw);
    if (url.protocol !== "komo:" || url.hostname !== "session") return undefined;
    const id = decodeURIComponent(url.pathname.replace(/^\/+/, ""));
    return /^[A-Za-z0-9._-]{1,128}$/.test(id) ? id : undefined;
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

function openDeepLink(raw: string) {
  const id = parseDeepLink(raw);
  if (!id) return;
  if (!windows || connection?.info().status !== "ok") {
    pendingSession = id; // 还没连上：连上后打开
    windows?.show();
    return;
  }
  windows.openSession(id);
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
    windows.loadPage(info.status === "mismatch" ? "mismatch" : "error");
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
  ipcMain.handle(channels.connectionTest, guard(async (input: RemoteInput) => {
    try {
      return await connection.test(input);
    } catch (error) {
      return { ok: false, steps: [{ name: "health", ok: false, message: (error as Error).message }] };
    }
  }));
  ipcMain.handle(channels.connectionSaveRemote, guard(async (input: RemoteInput) => {
    const result = await connection.saveRemote(input);
    // 成功就整页重载：旧 Gateway 的 Query 缓存与前端状态一起丢掉。
    if (result.ok) setImmediate(() => void connectAndShow());
    return result;
  }));
  ipcMain.handle(channels.connectionUseLocal, guard(() => {
    const result = connection.useLocal();
    if (result.ok) setImmediate(() => void connectAndShow());
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
  ipcMain.on(channels.openSessionListening, (event) => {
    if (isAppUrl(event.senderFrame?.url ?? "")) windows.markSessionListening();
  });
}

function buildMenu() {
  const settings = () => windows.loadPage("settings");
  const template: MenuItemConstructorOptions[] = [
    {
      label: app.name,
      submenu: [
        { role: "about" },
        { label: "检查更新… / Check for Updates…", click: () => void checkForUpdatesInteractive() },
        { type: "separator" },
        { label: "连接设置… / Connection Settings…", accelerator: "CommandOrControl+,", click: () => { windows.show(); settings(); } },
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
  system = new SystemIntegration(connection, windows);
  installHandler(connection);
  registerIpc();
  buildMenu();
  system.start(() => {
    windows.show();
    windows.loadPage("settings");
  });
  setupUpdater(() => windows.markQuitting());

  // 冷启动带着 komo:// 参数的情况（open-url 事件可能先于 ready，已经记进 pendingSession）。
  const link = process.argv.find((arg) => arg.startsWith("komo://"));
  if (link) pendingSession = parseDeepLink(link);
  windows.ensure();
  await connectAndShow();
});
