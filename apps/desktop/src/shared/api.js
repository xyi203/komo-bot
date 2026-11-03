// 主进程、preload 与界面共用的类型（只有类型，没有运行时代码）。
// `window.komoDesktop` 的形状就是 KomoDesktopApi；给 apps/client 的约定见 docs/clients.md §6.3。
/** IPC 通道名。 */
export const channels = {
    connectionGet: "komo:connection:get",
    connectionTest: "komo:connection:test",
    connectionSaveRemote: "komo:connection:save-remote",
    connectionUseLocal: "komo:connection:use-local",
    connectionRetry: "komo:connection:retry",
    connectionOpenSettings: "komo:connection:open-settings",
    notify: "komo:notify",
    window: "komo:window",
    commandPalette: "komo:command-palette",
    openSession: "komo:open-session",
    openSessionListening: "komo:open-session-listening",
};
/** 界面里 ⌘K 事件的 DOM 事件名（preload 在 window 上派发，没有 detail）。 */
export const COMMAND_PALETTE_EVENT = "komo-desktop:command-palette";
