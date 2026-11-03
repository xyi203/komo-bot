import { app, dialog } from "electron";
import { autoUpdater } from "electron-updater";

/**
 * 自动更新：electron-updater 读 GitHub Release 里的 latest-mac.yml（与 `komo update` 同一个仓库，不同产物，
 * 见 electron-builder.yml 的 publish 与 artifactName）。只在打包后的 App 里启用。
 * macOS 的无人值守更新要求 App 已签名；未签名包检查会失败，错误只记日志、不打扰用户（手动检查时才弹窗）。
 */
const zh = () => app.getLocale().toLowerCase().startsWith("zh");
const SIX_HOURS = 6 * 60 * 60 * 1000;

/** `beforeInstall`：quitAndInstall 先关窗口再退出，不经过 before-quit，窗口得事先知道这次是真退出。 */
export function setupUpdater(beforeInstall: () => void) {
  if (!app.isPackaged || process.env.KOMO_DESKTOP_NO_UPDATE) return;
  autoUpdater.autoDownload = true;
  autoUpdater.autoInstallOnAppQuit = true;
  autoUpdater.on("error", (error) => console.warn("自动更新失败：", error.message));
  autoUpdater.on("update-downloaded", (info) => {
    void dialog
      .showMessageBox({
        message: zh() ? `komo ${info.version} 已下载` : `komo ${info.version} downloaded`,
        detail: zh() ? "重启即可使用新版本；也可以稍后，退出时会自动安装。" : "Restart to use it, or later — it installs on quit.",
        buttons: [zh() ? "现在重启" : "Restart now", zh() ? "稍后" : "Later"],
        defaultId: 0,
      })
      .then(({ response }) => {
        if (response === 0) {
          beforeInstall();
          autoUpdater.quitAndInstall();
        }
      });
  });
  void autoUpdater.checkForUpdates().catch(() => {});
  setInterval(() => void autoUpdater.checkForUpdates().catch(() => {}), SIX_HOURS);
}

/** 菜单「检查更新…」：手动检查，结果弹窗说明。 */
export async function checkForUpdatesInteractive() {
  if (!app.isPackaged) {
    await dialog.showMessageBox({ message: zh() ? "开发模式下不检查更新" : "Updates are disabled in development builds" });
    return;
  }
  try {
    const result = await autoUpdater.checkForUpdates();
    const latest = result?.updateInfo.version;
    if (!latest || latest === app.getVersion()) {
      await dialog.showMessageBox({ message: zh() ? "已经是最新版本" : "You are up to date", detail: app.getVersion() });
    }
  } catch (error) {
    await dialog.showMessageBox({
      type: "error",
      message: zh() ? "检查更新失败" : "Update check failed",
      detail: (error as Error).message,
    });
  }
}
