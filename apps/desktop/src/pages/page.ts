// 壳自带的提示页：连接中、连接不上、版本不一致、连接设置。一个页面，按 ?kind= 切换。
// 只用 window.komoDesktop（preload 暴露），所有文字走 textContent。
import type { ConnectionInfo, KomoDesktopApi, TestResult } from "../shared/api";

declare global {
  interface Window {
    komoDesktop: KomoDesktopApi;
  }
}

const zh = navigator.language.toLowerCase().startsWith("zh");
const t = (cn: string, en: string) => (zh ? cn : en);
const api = window.komoDesktop;
const root = document.getElementById("root")!;
const kind = new URLSearchParams(location.search).get("kind") ?? "starting";
document.documentElement.lang = zh ? "zh-CN" : "en";

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Partial<HTMLElementTagNameMap[K]> & { class?: string } = {},
  ...children: Array<Node | string>
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  const { class: className, ...rest } = props;
  if (className) node.className = className;
  Object.assign(node, rest);
  node.append(...children);
  return node;
}

function button(label: string, onClick: () => void, primary = false) {
  return el("button", { class: primary ? "primary" : "", type: "button", onclick: onClick }, label);
}

function clear() {
  root.replaceChildren();
}

function renderStarting(info: ConnectionInfo) {
  clear();
  root.append(
    el("div", { class: "spinner" }),
    el("h1", {}, t("正在连接 Gateway…", "Connecting to the Gateway…")),
    el("p", { class: "muted" }, info.message ?? (info.mode === "local" ? t("本机模式", "Local mode") : info.baseUrl ?? "")),
  );
}

function renderProblem(info: ConnectionInfo) {
  clear();
  const title =
    info.status === "mismatch"
      ? info.older === "gateway"
        ? t("Gateway 版本比 App 旧", "The Gateway is older than this app")
        : t("App 版本比 Gateway 旧", "This app is older than the Gateway")
      : info.status === "unauthorized"
        ? t("令牌被拒绝", "Token rejected")
        : t("连不上 Gateway", "Can't reach the Gateway");
  root.append(el("h1", {}, title), el("p", {}, info.message ?? ""));
  if (info.baseUrl) root.append(el("p", { class: "muted" }, info.baseUrl));
  if (info.status === "mismatch" || info.gateway) {
    root.append(
      el(
        "p",
        { class: "muted" },
        `App ${info.app.version} · protocol ${info.app.protocolVersion}` +
          (info.gateway ? ` — Gateway ${info.gateway.version} · protocol ${info.gateway.protocolVersion}` : ""),
      ),
    );
  }
  const actions = el("div", { class: "row" }, button(t("重试", "Retry"), () => void api.connection.retry(), true));
  actions.append(button(t("连接设置", "Connection settings"), () => void api.connection.openSettings()));
  if (info.status === "mismatch" && info.older === "app") {
    actions.append(el("a", { class: "button", href: "https://github.com/xyi203/komo-bot/releases", target: "_blank" }, t("下载新版 App", "Download the new app")));
  }
  root.append(actions);
}

function renderSettings(info: ConnectionInfo) {
  clear();
  const remoteSaved = info.remote;
  const url = el("input", { type: "text", placeholder: "https://komo.example.com", value: remoteSaved?.url ?? "", spellcheck: false });
  const token = el("input", { type: "password", placeholder: t("设备令牌（每次保存都要重新填）", "Device token (re-enter on every save)"), autocomplete: "off" });
  const headers = el("textarea", {
    placeholder: remoteSaved?.headerNames.length
      ? t(`已保存的头：${remoteSaved.headerNames.join(", ")}（值不显示，需重新填写）`, `Saved headers: ${remoteSaved.headerNames.join(", ")} (values hidden; re-enter)`)
      : "CF-Access-Client-Id: …\nCF-Access-Client-Secret: …",
    spellcheck: false,
  });
  const result = el("div");
  const local = el("input", { type: "radio", name: "mode", checked: info.mode === "local" });
  const remote = el("input", { type: "radio", name: "mode", checked: info.mode === "remote" });
  const remoteForm = el(
    "div",
    {},
    el("label", {}, t("Gateway 地址", "Gateway URL")),
    url,
    el("label", {}, t("设备令牌", "Device token")),
    token,
    el("label", {}, t("额外请求头（可选，每行 名字: 值）", "Extra request headers (optional, one `Name: value` per line)")),
    headers,
    el("p", { class: "muted" }, t("不能设置 Authorization、Cookie、Host、Origin。令牌和请求头用系统钥匙串加密保存。", "Authorization, Cookie, Host and Origin can't be set. The token and headers are encrypted with the system keychain.")),
  );
  const sync = () => {
    remoteForm.hidden = !remote.checked;
    save.textContent = remote.checked ? t("测试并保存", "Test and save") : t("使用本机", "Use local");
    test.hidden = !remote.checked;
  };
  const test = button(t("只测试", "Test only"), async () => {
    result.replaceChildren(el("p", { class: "muted" }, t("测试中…", "Testing…")));
    showTest(await api.connection.test({ url: url.value, token: token.value, headers: headers.value }));
  });
  const save = button("", async () => {
    if (!remote.checked) {
      await api.connection.useLocal();
      return;
    }
    result.replaceChildren(el("p", { class: "muted" }, t("测试中…", "Testing…")));
    const saved = await api.connection.saveRemote({ url: url.value, token: token.value, headers: headers.value });
    if (!saved.ok) {
      showTest(saved.test, saved.error);
    }
  }, true);
  const showTest = (test: TestResult | undefined, error?: string) => {
    result.replaceChildren();
    if (error) result.append(el("p", { class: "bad" }, error));
    if (test) {
      const list = el("ul", { class: "steps" });
      for (const step of test.steps) {
        list.append(el("li", { class: step.ok ? "good" : "bad" }, `${step.ok ? "✓" : "✗"} ${step.message}`));
      }
      result.append(list);
    }
  };
  local.onchange = remote.onchange = sync;

  root.append(
    el("h1", {}, t("连接设置", "Connection settings")),
    el("p", { class: "muted" }, info.status === "ok" ? `${t("当前", "Current")}: ${info.baseUrl ?? ""}` : info.message ?? ""),
    el("div", { class: "mode" }, el("label", {}, local, t("本机", "Local")), el("label", {}, remote, t("远程", "Remote"))),
    remoteForm,
    el("div", { class: "row" }, save, test, button(t("返回", "Back"), () => void api.connection.retry())),
    result,
  );
  sync();
}

async function render() {
  const info = await api.connection.get();
  if (kind === "settings") return renderSettings(info);
  if (kind === "mismatch" || kind === "error") return renderProblem(info);
  renderStarting(info);
}

void render();
if (kind === "starting") {
  // 连接中：主进程探测完会自己换页；这里只刷新进度文字（"Gateway 没在跑，正在启动…"）。
  setInterval(() => void api.connection.get().then(renderStarting), 700);
}
