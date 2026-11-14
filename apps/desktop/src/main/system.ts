import { Menu, Notification, Tray, app, globalShortcut, nativeImage } from "electron";
import path from "node:path";
import type { ConnectionManager } from "./connection";
import { assetsRoot } from "./paths";
import { request } from "./upstream";
import type { WindowManager } from "./window";

/**
 * 系统能力：菜单栏 Tray（待处理数）、原生通知、全局快捷键。
 *
 * 数据来源选的是**轮询**，不是 SSE：Gateway 的 SSE 只有"每个会话一条"（`/v1/sessions/{id}/events`），没有全局事件流；
 * 为通知和角标给每个会话各开一条长连接不划算。`GET /v1/interventions`（待处理）与 `GET /v1/sessions`（任务会话的
 * 运行状态）都是便宜的只读查询，每 5 秒一次足够。等 Gateway 有全局事件流再换成订阅。
 */
const POLL_MS = 5000;
const SHORTCUT = process.env.KOMO_DESKTOP_SHORTCUT || "CommandOrControl+Shift+K";

const zh = () => app.getLocale().toLowerCase().startsWith("zh");
const text = {
  open: () => (zh() ? "打开 komo" : "Open komo"),
  pending: (n: number) => (zh() ? `待处理 ${n} 项` : `${n} pending`),
  nonePending: () => (zh() ? "没有待处理" : "Nothing pending"),
  settings: () => (zh() ? "设置…" : "Settings…"),
  quit: () => (zh() ? "退出 komo" : "Quit komo"),
  kind: (kind: string) =>
    kind === "confirm"
      ? zh() ? "需要你确认" : "Needs your confirmation"
      : kind === "verify"
        ? zh() ? "结果不明，请核对" : "Outcome unclear, please verify"
        : zh() ? "任务被阻塞" : "Task blocked",
  taskDone: () => (zh() ? "任务完成" : "Task finished"),
  taskFailed: () => (zh() ? "任务失败" : "Task failed"),
};

interface Intervention {
  handle: string;
  kind: string;
  session: string;
  question: string;
}
interface SessionRow {
  session: string;
  title: string;
  kind?: string;
  current_state?: string;
}
const TERMINAL = new Set(["completed", "failed", "cancelled", "abandoned", "absorbed"]);

export class SystemIntegration {
  private tray: Tray | undefined;
  private timer: ReturnType<typeof setInterval> | undefined;
  private seenInterventions: Set<string> | undefined;
  private runStates: Map<string, string | undefined> | undefined;
  private pending = 0;
  private polling = false;
  private readonly live = new Set<Notification>();
  private openSettings: () => void = () => {};

  constructor(private readonly connection: ConnectionManager, private readonly windows: WindowManager) {}

  start(openSettings: () => void) {
    this.openSettings = openSettings;
    const icon = nativeImage.createFromPath(path.join(assetsRoot(), "trayTemplate.png"));
    icon.setTemplateImage(true);
    this.tray = new Tray(icon);
    this.tray.setToolTip("komo");
    this.tray.on("click", () => this.windows.toggle());
    this.renderTray();

    if (!globalShortcut.register(SHORTCUT, () => this.windows.sendCommandPalette())) {
      console.warn(`全局快捷键 ${SHORTCUT} 注册失败（被别的程序占了？可用 KOMO_DESKTOP_SHORTCUT 改）`);
    }
    this.timer = setInterval(() => void this.poll(), POLL_MS);
    void this.poll();
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    globalShortcut.unregisterAll();
    this.tray?.destroy();
  }

  /** 切换连接后：基线作废（旧 Gateway 的会话不该变成"新通知"）。 */
  reset() {
    this.seenInterventions = undefined;
    this.runStates = undefined;
    this.pending = 0;
    this.renderTray();
    void this.poll();
  }

  /** 渲染进程经 `komoDesktop.notify` 发来的通知。 */
  notify(title: string, body?: string, session?: string) {
    if (!Notification.isSupported()) return;
    const notification = new Notification({ title, body });
    this.live.add(notification);
    notification.on("click", () => {
      this.windows.show();
      if (session) this.windows.openSession(session);
    });
    notification.on("close", () => this.live.delete(notification));
    notification.show();
  }

  private renderTray() {
    if (!this.tray) return;
    // macOS 上 title 显示在图标右侧：有待处理时是个数字，没有就空。
    this.tray.setTitle(this.pending > 0 ? String(this.pending) : "");
    this.tray.setContextMenu(
      Menu.buildFromTemplate([
        { label: text.open(), click: () => this.windows.show() },
        { label: this.pending > 0 ? text.pending(this.pending) : text.nonePending(), enabled: false },
        { type: "separator" },
        { label: text.settings(), click: () => this.openSettings() },
        { type: "separator" },
        { label: text.quit(), click: () => app.quit() },
      ]),
    );
  }

  private async poll() {
    if (this.polling) return;
    const target = this.connection.targetSync();
    if (!target || this.connection.info().status !== "ok") return;
    this.polling = true;
    try {
      const get = async <T>(url: string): Promise<T> => {
        const response = await request(target, url, { timeoutMs: 4000 });
        if (response.status === 401) this.connection.handleUnauthorized(target);
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return (await response.json()) as T;
      };
      const [interventions, sessions] = await Promise.all([
        get<{ interventions: Intervention[] }>("/v1/interventions"),
        get<{ sessions: SessionRow[] }>("/v1/sessions"),
      ]);
      this.apply(interventions.interventions, sessions.sessions);
    } catch {
      // 这一轮没取到（Gateway 重启中、网络断了）：保持现状，下一轮再来。
    } finally {
      this.polling = false;
    }
  }

  private apply(interventions: Intervention[], sessions: SessionRow[]) {
    if (interventions.length !== this.pending) {
      this.pending = interventions.length;
      this.renderTray();
    }
    const quiet = this.windows.window?.isFocused() ?? false;

    // 第一轮只记基线，不通知：打开 App 时已经在那儿的东西不算"新来的"。
    if (this.seenInterventions) {
      const fresh = interventions.filter((item) => !this.seenInterventions!.has(item.handle));
      if (!quiet) for (const item of fresh.slice(0, 3)) this.notify(text.kind(item.kind), item.question, item.session);
    }
    this.seenInterventions = new Set(interventions.map((item) => item.handle));

    const states = new Map(sessions.map((row) => [row.session, row.current_state] as const));
    if (this.runStates && !quiet) {
      for (const row of sessions) {
        if (row.kind !== "task" || !this.runStates.has(row.session)) continue;
        const before = this.runStates.get(row.session);
        const now = row.current_state;
        const wasRunning = before !== undefined && !TERMINAL.has(before);
        if (wasRunning && (now === "completed" || now === "failed")) {
          this.notify(now === "completed" ? text.taskDone() : text.taskFailed(), row.title, row.session);
        }
      }
    }
    this.runStates = states;
  }
}
