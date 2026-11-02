# 客户端：网页、macOS、手机

> 状态：草案（2026-11-01）。已由操作者拍板的有三条：前端栈用 TanStack Start、macOS 用 Electron、网页由 Gateway 托管
> （借鉴 Hermes Agent 与 DeepSeek Harness，见 §7）；2026-11-02 补拍板：新版界面要多语言，语言偏好只存在各端（D9）。
> §2 其余各条是建议，待认可。
> 取代 [`roadmap-muse.md`](roadmap-muse.md) §4.4"手机端先用飞书，不做原生 App"。现有原型的界面规格仍在 [`web.md`](web.md)，
> 视觉 token 在 [`DESIGN.md`](../DESIGN.md)。不带文件名的章节号指 [`komo_bot.md`](komo_bot.md)。

## 1. 要解决什么

`apps/web` 是 W0 原型：约 3500 行原生 JS/CSS，没有构建、没有类型，靠 `serve.mjs` 代理并替浏览器带令牌。要再往外长出
macOS 应用和手机端，需要一套标准的前端栈，以及一份所有端共用的界面代码。

一句话的边界：**所有端共用一份界面代码，都是 Gateway 的 HTTP / SSE 客户端，不持有状态；各端的壳只补系统能力
（托盘、通知、推送、钥匙串），不复制界面。** Gateway 仍是唯一打开状态的进程（§3）。

不做的：

- 不用 SSR，也不用 server functions / server routes。生产路径上没有 Node 进程，数据全走 Gateway 的 `/v1/*`。
- 不在各平台重写界面（React Native、Flutter、SwiftUI / Compose 都不用）。
- 本机场景下，令牌不进渲染进程，也不进浏览器 JS（同 `serve.mjs` 的现行做法）。

## 2. 决定

| # | 问题 | 建议 | 理由 |
|---|---|---|---|
| D1 | 前端框架 | **React + TypeScript + Vite + TanStack Start（SPA 模式）**，路由用 TanStack Router 的文件路由，服务端数据用 TanStack Query（已拍板） | SPA 模式构建时只预渲染一个壳 `/_shell.html`，产物全是静态文件。Gateway、Electron、Capacitor 都能直接装，不需要 Node 服务端 |
| D2 | 组件与样式 | Tailwind CSS v4 + shadcn/ui（Radix 底座），⌘K 用 `cmdk`；DESIGN.md 的 token 原样搬进 `@theme` 的 CSS 变量 | 弹窗、菜单、焦点管理、键盘交互交给 Radix，不再手写；token 不变，视觉不漂移 |
| D3 | 协议类型 | kernel `protocol::http` 生成 TypeScript 类型，提交进仓库，加一条测试确认生成物是最新的 | 前后端字段对不上的错误在编译期暴露。要给 kernel 加 `ts-rs`（放在 feature 后面），需先补 §13.4 的依赖表 |
| D4 | 仓库结构 | 仓库根加 pnpm workspace：`apps/client`（Start SPA）、`apps/desktop`（Electron）、`packages/protocol`（生成的类型）、`packages/api`（HTTP + SSE 客户端与 Query hooks）；以后加 `apps/mobile` | 界面、协议、各端的壳各管一摊。原型 `apps/web` 留到新版对齐后再删 |
| D5 | 网页由谁提供 | Gateway 同源提供 SPA 产物：`/v1/*` 是 API，其余路径先找静态文件，找不到回落 `_shell.html`（已拍板，同 Hermes） | 去掉 `serve.mjs`；浏览器、PWA、手机都只认 Gateway 一个地址（web.md §9 第 1 条）。网页跟 Gateway 一起发布，协议不会错位；同源时 cookie 鉴权可用。不放到 Cloudflare Worker：浏览器反正要连到 Gateway，放在 Worker 上只多一处部署，还会跨域、版本错位 |
| D6 | 浏览器怎么鉴权 | 配对换 cookie：`komo web` 打开 `http://127.0.0.1:7777/pair#code=<一次性码>`，Gateway 换成 `HttpOnly; SameSite=Strict` 的会话 cookie；`auth` 层同时接受 Bearer 与这个 cookie；会改东西的请求另外核对 `Origin`。另加 **Host 校验**：只接受回环地址、监听地址和 `[gateway] trusted_hosts` 里列出的 Host，其余一律 403（同 DeepSeek Harness） | `EventSource` 带不了请求头，cookie 能带；`HttpOnly` 让 JS 拿不到；一次性码用完作废，不进日志。Host 校验挡的是 DNS 重绑定：恶意网页把自己的域名改解析到 `127.0.0.1`，就能以"同源"身份读 Gateway 的响应。cookie 按主机名隔离，带不过去，但不需要鉴权的路由（静态文件、`/healthz`、配对换 cookie）照样暴露。校验只负责放行，鉴权仍靠令牌和 cookie |
| D7 | macOS | **Electron**（已拍板），用 electron-builder 打包和自动更新；连接分本机与远程两种模式 | 跟网页同一套代码。见 §4 |
| D8 | 手机 | 先做 PWA；PWA 不够用时再用 Capacitor 套壳 | 见 §5 |
| D9 | 多语言 | 新版从第一行界面代码起就走文案表，先做 `zh-CN` 与 `en`；文案表用 Paraglide JS（编译期生成带类型的消息函数）。语言偏好**只存在各端**（已拍板）：浏览器与 PWA 存 `localStorage`，Electron 存在自己的设置里，Capacitor 存在 App 偏好里；没选过就跟系统语言（`zh*` 用中文，其余用英文） | 原型里几百处写死的中文不再搬第二次：C1 迁移一节就抽一节的文案。消息函数带类型，漏翻、键名写错在编译期报错，没用到的文案不进产物。偏好不进 Gateway：语言是看界面的人的事，同一个 Gateway 可以有人用中文、有人用英文；代价是换设备要重选一次 |

## 3. 共用部分

- **`packages/api`**：基于 `fetch` 的请求封装，加上 SSE 订阅。SSE 带提交游标，断开后从游标续上，补齐缺的事件
  （§8 的 SSE 只推已提交的东西，游标就是提交序号）。错误统一成 `ErrorBody`。Query 的 key 按资源组织，SSE 事件到达时
  让对应的 key 失效或直接写进缓存。
- **界面迁移**：按 web.md 的章节逐节搬，行为以 web.md 为准，搬的过程中只修 bug、不加功能。顺序：会话列表与对话流 →
  输入框与右栏 → ⌘K → 设置（模型、配置、环境变量、MCP、渠道、Gateway）→ 待处理 → 记忆、事项、定时。
  每搬完一节，在 web.md 里标出新版的位置。全部对齐后删掉 `apps/web` 和 `serve.mjs`，web.md 改成新版的规格。
- **构建**：`pnpm build` 产出 `apps/client/dist`。Rust 的冷构建不依赖 Node：Gateway 运行时从一个目录读静态文件
  （发布包里的 `web/`，开发时由 `[web] dir` 指过去），不嵌进二进制。发布包多出 `web/` 目录，会碰到 §13.6 的发布约定
  （`release.yml`、`install.sh`、`update.rs` 三处一起改）。

- **多语言**（D9）：
  - **翻译范围**：只翻界面自己的文字，包括导航、按钮、提示、空状态、错误说明，以及界面按 key 渲染的东西（介入的答复选项、
    常用设置的说明、状态名）。不翻的有三类：模型的回复、记忆与事项的正文这类用户内容，机器的名字（模型 id、路径、配置键），
    以及 Gateway 原样返回的 `ErrorBody.message`。错误按 `code` 映射成译文，不认识的 `code` 再回落到原文。
  - **术语**：中英对照表跟设计文档走（约定 convention、线索 reference、目标 goal、领域 area、任务会话 task session、
    插话 interjection、待处理 inbox……）。英文取代码里的名字，两种语言里的同一个概念各只有一种说法。
  - **格式**：日期、时间、相对时间、数字和复数都交给 `Intl`，按当前语言渲染，不手拼字符串。
  - **布局**：英文通常比中文长 30%–50%。按钮、标签页、表头不能按中文定宽；迁移每一节都要在两种语言下各看一遍。
  - **检查**：加一条 lint，JSX 里出现中文字面量就报错，防止新写的界面绕过文案表。
  - **不影响的地方**：TUI、CLI 和渠道回执不受这个偏好影响，仍然由 Gateway 决定。原型 `apps/web` 保持只有中文，不补多语言。

## 4. macOS：Electron

- **加载**：用自定义协议 `app://` 加载打进包里的 SPA 产物，不用 `file://`。打开即用，不依赖 Gateway 先起来。
- **两种连接**（同 Hermes Desktop 的本机 / 远程）：
  - **本机**：主进程读 `<数据目录>/runtime/gateway.json` 拿地址和令牌。Gateway 重启换了令牌，就重读一次。Gateway 没在跑时
    调用 `komo gateway` 拉起它（与 `komo` 命令的"需要时启动"同一个行为）。
  - **远程**：填 Gateway 地址和设备令牌（§5），可选再加几个额外请求头，给挡在前面的访问代理用（比如 Cloudflare Access 的
    service token）。保存前先测一次 HTTP 和 SSE 都通。令牌用 Electron 的 `safeStorage` 加密保存。远程模式下工具都跑在
    Gateway 那台机器上，不在显示界面的这台。
  - 两种模式下，界面都把 API 请求发到 `app://` 自己的 `/v1/*`，由主进程的协议处理器转发给当前 Gateway，并在转发时加上
    `Authorization` 和额外请求头（同 DeepSeek Harness）。界面看来一直是同源，不需要 CORS；渲染进程看不到令牌。SSE 也是
    HTTP，转发时原样把响应流接过去。额外请求头不能覆盖 `Authorization`、`Cookie`、`Host`、`Origin` 这几个。
  - 切换连接时，清掉跟上一个 Gateway 绑定的前端状态，让所有查询缓存失效，避免旧 Gateway 的会话混进新的界面。
- **系统能力**：菜单栏图标（有待处理时带数字）、原生通知（确认卡、任务结束，来源是 SSE）、全局快捷键唤起 ⌘K、
  `komo://` 深链（打开某个会话）。
- **安全**：`contextIsolation` 开、`nodeIntegration` 关，preload 只暴露通知和窗口控制这几个函数；导航只允许 `app://`。
- **分发**：需要 Apple Developer ID 签名和公证（操作者动作）。自动更新走 GitHub Release，跟 `komo update` 用同一个仓库，
  但更新的是另一个产物。
- **同版本发布**（同 DeepSeek Harness：界面、壳、后端只作为一个组合来测）：Electron 应用和 `komo` 用同一个 tag、在同一个
  发布流程里出，版本号相同。跟 dsh 不同的是，komo 的 Gateway 是 launchd 常驻服务，App 关掉后渠道和定时任务也得接着跑，
  所以 App 只连接 Gateway、不持有它，两边可能各自更新而错开。为此连接时读 `/healthz` 的 `protocol_version`：
  跟 App 打包时的不一致就停在一个提示页，说清楚哪一边旧、怎么更新（`komo update` 或 App 自动更新），不带着不匹配的
  协议往下跑。`version` 不同但协议相同时照常使用，只在设置页显示两边的版本。

## 5. 手机

前提是手机能安全连上 Gateway：

1. **连接**：Gateway 只管监听哪个地址、认哪些令牌，不关心中间怎么走。两种走法都支持：
   - **Tailscale**：Gateway 额外监听 tailnet 地址（新增配置项，默认关）。端到端加密，但每台设备都要装 Tailscale。
   - **Cloudflare Tunnel + Access**：Mac 上跑 `cloudflared`，把本机 Gateway 挂到自己的域名下，由 Access 负责登录。
     不用开端口，任何浏览器都能打开；代价是 Cloudflare 终止 TLS，能看到明文。
   网页由 Gateway 托管，所以经隧道打开时仍然同源，D6 的 cookie 照样能用。

   经反向代理或隧道访问时，Gateway 看不到浏览器实际用的地址，需要两项配置（同 DeepSeek Harness 的 `--public-url` /
   `--trusted-host`）：`[gateway] public_url` 告诉 Gateway 对外地址，用在 `komo web` 打印的链接和配对二维码里；
   `trusted_hosts` 把这个域名加进 D6 的 Host 校验。代理要原样转发 `Host`，不要改写成 `127.0.0.1:7777`。浏览器那一段
   必须是 HTTPS，否则配对码和 cookie 都是明文；cookie 在 HTTPS 下加 `Secure`。
2. **设备配对**：电脑上出二维码，手机扫码后换到一个只属于这台设备的令牌，可以单独吊销。设备清单的存放位置待定：
   按 §11.2"身份在配置里"的先例，倾向于放进 `config.toml`，令牌的哈希放进 `.env`。
3. **PWA**：manifest、图标、service worker。推送用 Web Push：Gateway 拿 VAPID 密钥直接推，不需要自建推送服务。
   iOS 要先"添加到主屏幕"才能收推送（16.4 起支持）。
4. **原生壳（按需）**：碰到 PWA 的硬限制再做，比如 iOS 推送不够稳，要分享扩展、小组件、快捷指令，或者要读位置、
   健康数据（roadmap-muse P5）。用 Capacitor 套同一份 SPA 产物，令牌存钥匙串，原生能力写成插件。

## 6. 阶段

| 阶段 | 内容 | 完成的标志 |
|---|---|---|
| C0 | workspace、协议类型生成、`packages/api`、Start SPA 壳与 token、多语言骨架（文案表、语言切换、lint）、Gateway 提供静态文件与 cookie 鉴权 | `komo web` 打开 7777 上的新壳，能列会话、能收 SSE，能在中英文之间切换 |
| C1 | 按 §3 的顺序逐节迁移，对齐原型；每节的文案同时进中英文两份 | web.md 每一节都有新版对应，且两种语言下都检查过；`apps/web` 与 `serve.mjs` 删除 |
| C2 | Electron 壳（§4） | 签名后的 `.app` 能连本机 Gateway，通知与托盘可用 |
| C3 | 远程访问（Tailscale 或 Tunnel，`public_url` / `trusted_hosts`）、设备配对、PWA 与 Web Push（§5 的 1–3） | iPhone 主屏幕上的 PWA 能聊天、收确认卡推送 |
| C4 | Capacitor 原生壳（§5 的 4） | 按需 |

## 7. 参考：Hermes Agent 与 DeepSeek Harness

2026-11-01 读的 `NousResearch/hermes-agent` 仓库（`web/README.md`、`apps/desktop/README.md`）：

- **网页面板**（`web/`）：Vite + React 19 + TypeScript + Tailwind v4 + shadcn 风格组件。构建到 `hermes_cli/web_dist/`，
  打进 Python 包，由自家后端（FastAPI，`hermes dashboard`，本机 9119 端口）当静态 SPA 提供；开发时用 Vite 把 `/api`
  代理过去。没有部署到 Cloudflare 这类托管平台。
- **桌面版**（`apps/desktop`）：Electron。渲染进程和浏览器面板共用 `apps/shared` 这层传输代码。连接分三种：本机托管的
  后端、手动填写的远程 Gateway、Hermes Cloud。连远程时先探测鉴权方式（token 或 OAuth），HTTP 和 WebSocket 都测通了
  才保存，令牌用 `safeStorage` 加密；每个连接可以配额外请求头，给访问代理用，但 `Authorization`、`Cookie`、`Host`、
  `Origin` 这类请求头不让覆盖。切换连接时清掉绑定在旧 Gateway 上的状态，让查询缓存失效。macOS 用 electron-updater 更新。

komo 照搬的：网页由 Gateway 托管、跟后端一起发布（D5）；界面与传输层各端共用（D4 的 `packages/api`）；Electron 的
本机 / 远程两种连接、额外请求头和切换时清状态（§4）。不照搬的：Hermes 用 WebSocket JSON-RPC，komo 沿用现有的
HTTP + SSE。

DeepSeek Harness（`deepseek-ai/deepseek-harness`，同日读的公开部署文档与 `apps/desktop/README.md`）：

- **网页**：`dsh web` 在 `127.0.0.1:3080` 上同时提供界面和 API，实时消息走 WebSocket。启动时打印带启动令牌的网址，
  访问 `/` 时把令牌换成签名的会话 cookie。Host 校验只接受回环地址、监听地址和 `--trusted-host` 列出的域名，其余 API
  请求 403。公开访问交给反向代理或隧道，`--public-url` 告诉它浏览器用的地址，也可以用 `--tls-cert` / `--tls-key`
  直接开 HTTPS。
- **桌面版**：Electron + electron-builder。Electron 起子进程跑后端，监听 `127.0.0.1` 上系统分配的端口；界面用自定义协议
  `dsh-app://app/` 加载打包好的网页，请求由主进程转发给后端，凭据只附加在自己的窗口上，渲染进程拿不到令牌。Electron
  壳与 dsh 永远同一个版本号，后端升级就是一次桌面发布。

komo 照搬的：启动令牌换 cookie、Host 校验（D6）、`public_url` / `trusted_hosts`（§5）、同版本
发布（§4）。不照搬的：dsh 桌面版自己持有后端进程，komo 的 App 只连接常驻的 Gateway。

## 8. 待定

- D2–D4、D6、D8 待认可；D9 里用 Paraglide JS 做文案表也待认可（备选 i18next，运行时加载、生态更大，但键名没有类型检查）。
- 第三种语言要不要做、谁来维护译文（C1 前）。
- D3 的新依赖（`ts-rs`）要先补 §13.4 的依赖表；不加的话，退路是用 `schemars` 出 JSON Schema 再转 TypeScript，依赖一样要登记。
- 包管理器用 pnpm；Node 版本跟 Electron 内置的版本对齐。
- Apple Developer 账号与签名证书（C2 前）。
- 设备清单与令牌放在哪（C3 前）。
- 远程访问先用 Tailscale 还是 Cloudflare Tunnel + Access（C3 前；Tunnel 连的是本机地址，Gateway 不用改，Tailscale 要加监听配置）。
