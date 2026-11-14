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
    el("p", { class: "muted" }, info.message ?? info.remote?.url ?? ""),
  );
}

function renderProblem(info: ConnectionInfo) {
  clear();
  const title =
    info.status === "mismatch"
      ? info.older === "gateway"
        ? t("Gateway 版本比 App 旧", "The Gateway is older than this app")
        : t("App 版本比 Gateway 旧", "This app is older than the Gateway")
      : t("连不上 Gateway", "Can't reach the Gateway");
  root.append(el("h1", {}, title), el("p", {}, info.message ?? ""));
  if (info.remote?.url) root.append(el("p", { class: "muted" }, info.remote.url));
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
  const url = el("input", { type: "text", placeholder: "https://komo.example.com", value: info.remote?.url ?? "", spellcheck: false });
  const names = info.remote?.headerNames ?? [];
  const headers = el("textarea", {
    placeholder: names.length
      ? t(`已保存的头：${names.join(", ")}（值不显示；留空且地址不变则沿用）`, `Saved headers: ${names.join(", ")} (values hidden; leave empty to keep them while the URL is unchanged)`)
      : "CF-Access-Client-Id: …\nCF-Access-Client-Secret: …",
    spellcheck: false,
  });
  const result = el("div");
  let busy = false;
  const sync = () => {
    signIn.disabled = busy || Boolean(info.signingIn);
    test.disabled = busy;
  };
  // 请求进行中按钮置灰：连点不会并发出几个请求、结果也不会被来回覆盖
  const run = async (work: () => Promise<void>) => {
    if (busy) return;
    busy = true;
    sync();
    try {
      await work();
    } finally {
      busy = false;
      sync();
    }
  };
  const input = () => ({ url: url.value, headers: headers.value });
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
  const test = button(t("只测试", "Test only"), () => void run(async () => {
    result.replaceChildren(el("p", { class: "muted" }, t("测试中…", "Testing…")));
    showTest(await api.connection.test(input()));
  }));
  const signIn = button(info.account ? t("重新登录", "Sign in again") : t("登录", "Sign in"), () => void run(async () => {
    result.replaceChildren(el("p", { class: "muted" }, t("测试中…", "Testing…")));
    const started = await api.connection.signIn(input());
    if (!started.ok) return showTest(started.test, started.error);
    // 浏览器已打开：换成等待态，回跳后主进程会整页换掉这里；超时 / 取消由下面的轮询发现。
    renderSettings(await api.connection.get());
  }), true);

  root.append(el("h1", {}, t("连接设置", "Connection settings")));
  if (info.account) {
    const who = info.account.email ?? info.account.subject;
    root.append(
      el("p", { class: "muted" }, `${t("已登录", "Signed in")}: ${who} · ${info.baseUrl ?? ""}`),
      el("div", { class: "row" }, button(t("退出登录", "Sign out"), () => void api.connection.signOut())),
    );
  } else {
    root.append(el("p", { class: "muted" }, info.message ?? t("还没有登录。填 Gateway 地址，点「登录」，在系统浏览器里完成登录。", "Not signed in. Enter the Gateway URL and sign in from your browser.")));
  }
  if (info.notice) root.append(el("p", { class: info.notice.error ? "bad" : "good" }, info.notice.text));
  if (info.signingIn) {
    root.append(
      el("p", {}, t("已在系统浏览器里打开登录页。登录完成后会自动回到这里（最长等 5 分钟）。", "The sign-in page is open in your browser. You'll be brought back here when it's done (waits up to 5 minutes).")),
      el("div", { class: "row" }, button(t("取消", "Cancel"), () => void api.connection.cancelSignIn().then(async () => renderSettings(await api.connection.get())))),
    );
  }
  root.append(
    el("label", {}, t("Gateway 地址", "Gateway URL")),
    url,
    el("p", { class: "muted hint" }, t("https；开发时连本机可以写 http://127.0.0.1:端口。", "https only; http://127.0.0.1:port is allowed for local development.")),
    el(
      "details",
      {},
      el("summary", {}, t("额外请求头（可选）", "Extra request headers (optional)")),
      el("label", {}, t("每行 名字: 值", "One `Name: value` per line")),
      headers,
      el("p", { class: "muted hint" }, t("Gateway 前面有访问代理（如 Cloudflare Access 的 service token）才需要。不能设置 Authorization、Cookie、Host、Origin。登录令牌和请求头用系统钥匙串加密保存。", "Only needed behind an access proxy (e.g. a Cloudflare Access service token). Authorization, Cookie, Host and Origin can't be set. The sign-in token and headers are encrypted with the system keychain.")),
    ),
    el("div", { class: "row" }, signIn, test, button(t("返回", "Back"), () => void api.connection.retry())),
    result,
  );
  sync();
  if (info.signingIn) waitForSignIn();
}

let waiting: ReturnType<typeof setInterval> | undefined;
/** 登录等待中：每 700ms 看一眼主进程。超时 / 取消后（signingIn 没了）重画；成功时主进程会换页，不用管。 */
function waitForSignIn() {
  if (waiting) clearInterval(waiting);
  waiting = setInterval(() => {
    void api.connection.get().then((info) => {
      if (info.signingIn) return;
      clearInterval(waiting);
      waiting = undefined;
      renderSettings(info);
    });
  }, 700);
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
