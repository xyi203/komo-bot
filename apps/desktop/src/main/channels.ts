// IPC 通道名与界面事件名（运行时值，所以不放在只有类型的 shared/api.ts 里；preload 照 typeof 抄一份）。

/** IPC 通道名。 */
export const channels = {
  connectionGet: "komo:connection:get",
  connectionSetMode: "komo:connection:set-mode",
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
} as const;

/** 界面里 ⌘K 事件的 DOM 事件名（preload 在 window 上派发，没有 detail）。 */
export const COMMAND_PALETTE_EVENT = "komo-desktop:command-palette";
