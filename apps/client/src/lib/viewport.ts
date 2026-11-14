import { useEffect } from "react";

/**
 * 手机上软键盘弹出时，把可见区域的高度与偏移写进 `--app-height` / `--app-top`：窄屏外壳（app-shell）按它定高，
 * 输入框因此始终在键盘上方。用 visualViewport 而不是 dvh：iOS 的 dvh 不随键盘变，而 visualViewport 在 iOS 与 Android 上都准。
 * 不支持 visualViewport 时变量没设，外壳回落到 `100dvh`。
 */
export function useViewportVars(active: boolean) {
  useEffect(() => {
    const viewport = window.visualViewport;
    if (!active || !viewport) return;
    const root = document.documentElement;
    const apply = () => {
      root.style.setProperty("--app-height", `${viewport.height}px`);
      root.style.setProperty("--app-top", `${viewport.offsetTop}px`);
    };
    apply();
    viewport.addEventListener("resize", apply);
    viewport.addEventListener("scroll", apply);
    return () => {
      viewport.removeEventListener("resize", apply);
      viewport.removeEventListener("scroll", apply);
      root.style.removeProperty("--app-height");
      root.style.removeProperty("--app-top");
    };
  }, [active]);
}
