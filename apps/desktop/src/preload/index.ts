// preload：渲染进程能碰到主进程的唯一入口。sandbox 开着，preload 里只能 require("electron")，
// 所以这里不 import 共享模块的运行时代码，通道名照 `channels` 的类型原样抄一份（写错编译不过）。
// 只暴露四类函数：通知、窗口控制、连接设置、右栏浏览器，外加几个"主进程通知界面"的订阅。令牌从不经过这里。
import { contextBridge, ipcRenderer } from "electron";
import type { channels as ChannelNames } from "../main/channels";
import type { KomoDesktopApi } from "../shared/api";

const channels: typeof ChannelNames = {
  connectionGet: "komo:connection:get",
  connectionTest: "komo:connection:test",
  connectionSignIn: "komo:connection:sign-in",
  connectionCancelSignIn: "komo:connection:cancel-sign-in",
  connectionSignOut: "komo:connection:sign-out",
  connectionRetry: "komo:connection:retry",
  connectionOpenSettings: "komo:connection:open-settings",
  notify: "komo:notify",
  window: "komo:window",
  commandPalette: "komo:command-palette",
  openSettings: "komo:open-settings",
  openSettingsListening: "komo:open-settings-listening",
  openSession: "komo:open-session",
  openSessionListening: "komo:open-session-listening",
  browserState: "komo:browser:state",
  browserBounds: "komo:browser:bounds",
  browserNavigate: "komo:browser:navigate",
  browserAct: "komo:browser:act",
  browserCapture: "komo:browser:capture",
  browserDetect: "komo:browser:detect",
  browserStateChanged: "komo:browser:state-changed",
};
const COMMAND_PALETTE_EVENT = "komo-desktop:command-palette";

// 这两个参数由主进程在 webPreferences.additionalArguments 里给：`--komo-app-version=…`。
const arg = (name: string) => process.argv.find((item) => item.startsWith(`--${name}=`))?.slice(name.length + 3) ?? "";

function subscribe<T extends unknown[]>(channel: string, handler: (...args: T) => void): () => void {
  const listener = (_event: unknown, ...args: unknown[]) => handler(...(args as T));
  ipcRenderer.on(channel, listener);
  return () => ipcRenderer.removeListener(channel, listener);
}

// ⌘K：除了回调，还在 window 上派发一个无 detail 的 DOM 事件，界面用哪种都行。
ipcRenderer.on(channels.commandPalette, () => window.dispatchEvent(new Event(COMMAND_PALETTE_EVENT)));

const api: KomoDesktopApi = {
  apiVersion: 1,
  platform: process.platform,
  appVersion: arg("komo-app-version"),
  protocolVersion: Number(arg("komo-protocol-version")),
  connection: {
    get: () => ipcRenderer.invoke(channels.connectionGet),
    test: (input) => ipcRenderer.invoke(channels.connectionTest, input),
    signIn: (input) => ipcRenderer.invoke(channels.connectionSignIn, input),
    cancelSignIn: () => ipcRenderer.invoke(channels.connectionCancelSignIn),
    signOut: () => ipcRenderer.invoke(channels.connectionSignOut),
    retry: () => ipcRenderer.invoke(channels.connectionRetry),
    openSettings: () => ipcRenderer.invoke(channels.connectionOpenSettings),
  },
  notify: (input) => ipcRenderer.invoke(channels.notify, input),
  window: { do: (action) => ipcRenderer.invoke(channels.window, action) },
  browser: {
    state: () => ipcRenderer.invoke(channels.browserState),
    setBounds: (rect) => ipcRenderer.invoke(channels.browserBounds, rect),
    navigate: (input) => ipcRenderer.invoke(channels.browserNavigate, input),
    act: (action) => ipcRenderer.invoke(channels.browserAct, action),
    capture: () => ipcRenderer.invoke(channels.browserCapture),
    detectDevServers: () => ipcRenderer.invoke(channels.browserDetect),
    onState: (handler) => subscribe(channels.browserStateChanged, handler),
  },
  onCommandPalette: (handler) => subscribe(channels.commandPalette, () => handler()),
  onOpenSettings: (handler) => {
    ipcRenderer.send(channels.openSettingsListening);
    return subscribe(channels.openSettings, (section?: string) => handler(section));
  },
  onOpenSession: (handler) => {
    ipcRenderer.send(channels.openSessionListening);
    return subscribe(channels.openSession, (id: string) => handler(id));
  },
};

contextBridge.exposeInMainWorld("komoDesktop", api);
