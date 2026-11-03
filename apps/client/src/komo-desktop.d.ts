// C2（Electron 壳）注入的 `window.komoDesktop`：没有桌面壳时不存在，所以每一项都是可选的。
interface KomoDesktop {
  /** 主进程的 `process.platform`（darwin / win32 / linux）。 */
  platform?: string;
  /** ⌘K 菜单命令；返回取消订阅的函数。 */
  onCommandPalette?: (listener: () => void) => () => void;
  connection?: {
    /** 打开桌面壳的"连接的 Gateway"设置（本机 / 远程）。 */
    openSettings(): void;
  };
}

interface Window {
  komoDesktop?: KomoDesktop;
}
