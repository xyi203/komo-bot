# 客户端：网页、macOS、手机

> 状态：部分实施。草案（2026-11-01）；**C0 已实现（2026-11-02，§6.1）、C1 已实现（2026-11-03，§6.2）、C2 已实现（§6.3，未签名）**，C3（远程访问、设备配对、PWA 与 Web Push）、C4 未做。已由操作者拍板的有三条：前端栈用 TanStack Start、macOS 用 Electron、网页由 Gateway 托管
> （借鉴 Hermes Agent 与 DeepSeek Harness，见 §7）；2026-11-02 补拍板：新版界面要多语言，语言偏好只存在各端（D9）；
> 同日操作者认可按 §2 的建议做 D1–D6、D9（文案表用 Paraglide JS）。D7、D8 仍是建议。
> 取代 [`roadmap-muse.md`](roadmap-muse.md) §4.4"手机端先用飞书，不做原生 App"。界面规格在 [`web.md`](web.md)（C1 起是新版的规格），
> 视觉 token 在 [`DESIGN.md`](../DESIGN.md)。不带文件名的章节号指 [`komo_bot.md`](komo_bot.md)。

## 1. 要解决什么

`apps/web` 曾是 W0 原型（C1 已删除）：约 3500 行原生 JS/CSS，没有构建、没有类型，靠 `serve.mjs` 代理并替浏览器带令牌。要再往外长出
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
| D4 | 仓库结构 | 仓库根加 pnpm workspace：`apps/client`（Start SPA）、`apps/desktop`（Electron）、`packages/protocol`（生成的类型）、`packages/api`（HTTP + SSE 客户端与 Query hooks）；以后加 `apps/mobile` | 界面、协议、各端的壳各管一摊。原型 `apps/web` 留到新版对齐后再删（C1 已删） |
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
  每搬完一节，在 web.md 里标出新版的位置。全部对齐后删掉 `apps/web` 和 `serve.mjs`，web.md 改成新版的规格（已做，§6.2）。
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
  - **不影响的地方**：TUI、CLI 和渠道回执不受这个偏好影响，仍然由 Gateway 决定。原型 `apps/web` 保持只有中文，不补多语言（C1 已删除）。

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
| C0（**已做**，§6.1） | workspace、协议类型生成、`packages/api`、Start SPA 壳与 token、多语言骨架（文案表、语言切换、lint）、Gateway 提供静态文件与 cookie 鉴权 | `komo web` 打开 7777 上的新壳，能列会话、能收 SSE，能在中英文之间切换 |
| C1（**已做**，§6.2） | 按 §3 的顺序逐节迁移，对齐原型；每节的文案同时进中英文两份 | web.md 每一节都有新版对应，且两种语言下都检查过；`apps/web` 与 `serve.mjs` 删除 |
| C2（**已做**，§6.3） | Electron 壳（§4） | 签名后的 `.app` 能连本机 Gateway，通知与托盘可用 |
| C3 | 远程访问（Tailscale 或 Tunnel，`public_url` / `trusted_hosts`）、设备配对、PWA 与 Web Push（§5 的 1–3） | iPhone 主屏幕上的 PWA 能聊天、收确认卡推送 |
| C4 | Capacitor 原生壳（§5 的 4） | 按需 |

### 6.1 C0 实际做了什么（2026-11-02）

| 项 | 位置 | 状态 |
|---|---|---|
| pnpm workspace（D4） | 根 `package.json` / `pnpm-workspace.yaml`；`apps/client`、`packages/protocol`、`packages/api`；`apps/desktop` 不在 C0，原型 `apps/web` 不动 | 完成。`pnpm install` / `build` / `lint` / `typecheck` 都过 |
| 协议类型（D3） | kernel 的 `ts` feature（默认关）；`protocol/ts.rs` 导出到 `packages/protocol/src/generated/`；`cargo test -p komo-kernel --features ts` 核对生成物是最新的，`KOMO_UPDATE_TS=1` 重新生成；§13.4 已登记 `ts-rs` | 完成，**导出的是最小集合**：`ErrorBody`（`ApiError`、`ErrorCode`）、`HealthResponse`、`SessionListResponse`（`SessionSummary`、`RunState`、`WaitReason`…）、`EventQuery`、`EventPage`、`SseFrame`、配对的两个类型 |
| `packages/api` | `fetch` 封装（同源相对路径、cookie、失败统一成 `ApiError`）、`subscribeSession`（SSE 带游标续传）、按资源组织的 Query key、`useSessions` / `useHealth` / `useSessionStream`、`invalidateOnFrame` | 完成 |
| SPA 壳 | `apps/client`：TanStack Start SPA 模式 + Tailwind v4（DESIGN.md token 在 `src/styles.css` 的 `@theme`）+ shadcn/ui 初始化（`components.json`、`cn`、一个 Button）；左栏会话列表、选中后显示 SSE 帧、语言切换 | 完成，界面只到"列会话、看事件流"，其余留给 C1 |
| 多语言（D9） | Paraglide JS（`messages/{zh-CN,en}.json`）；偏好存 `localStorage`，没选过按系统语言（`zh*` 中文，其余英文）；日期、相对时间、数字走 `Intl`；`pnpm lint` 里 `scripts/lint-i18n.mjs` 在 JSX 里发现中文字面量就报错 | 完成 |
| Gateway 静态文件（D5） | `[web] dir`（没配退到二进制旁的 `web/`）；`/v1/*`、`/healthz` 照旧，其余先找文件、无扩展名回落壳 html；目录不存在只是没有网页；`komo config check` 对不存在的目录给警告 | 完成；`[web] dir` 与 `trusted_hosts` 是**启动期键**（改了重载会报"需要 `komo gateway restart`"） |
| 鉴权（D6） | `komo web [--no-open]`；`POST /v1/web/pair-code`（只认 Bearer）与 `POST /v1/web/session`；`auth` 层认 Bearer 或 cookie，cookie 的写请求核对 Origin；所有路由先过 Host 校验；Rust 单测与 `tests/web` 集成测试 | 完成，见下面的偏离 |
| 开发流程 | `apps/client/vite-gateway-proxy.ts`：`pnpm dev` 把 `/v1`、`/healthz` 转给本机 Gateway，令牌每次请求现读 `$KOMO_HOME/runtime/gateway.json` | 完成；开发模式下令牌由代理带，**不需要配对** |
| 发布约定（§13.6） | 包里多 `web/`；`release.yml` 构建并打进归档，`install.sh`、`update.rs` 装到二进制旁（`web` 是指向 `komo-web/<代次>/` 的符号链接，一次 `rename` 换上） | 完成，三处一起改；`update.rs` 有单测，`release.yml` / `install.sh` 没法本地跑，只对过路径 |

**与上文规格的偏离与取舍**

- **构建产物在 `apps/client/dist/client/`，不是 `dist/`**：Start 的 client 环境这么输出（旁边的 `dist/server` 用不到）。开发时 `[web] dir` 指 `apps/client/dist/client`，`release.yml` 拷的也是这个目录。壳 html 是 `_shell.html`，Gateway 回落时先找它，再找 `index.html`。
- **SSE 自己重连，不用 `EventSource` 的自动重连**：浏览器只记最后一个 `id:`，而 `run_status` / `assistant_delta` 这类派生帧与源事件同号，拿它当游标可能跳过还没送到的源事件。`subscribeSession` 记的是原始事件（`event` 帧）的 seq，`error` 来了就关掉自动重连，按退避自己重开并带 `?from=<seq>`。
- **`Event` 没有 derive 能描述的形状**（自定义序列化），线上给 `WireEvent` 这个裸信封（`type` + `data`），`data` 是 `unknown`，由界面按 `type` 收窄；`SseEvent::NestedCalls.calls` 同样是 `unknown[]`。线上 `skip_serializing_if` 的 `Vec`（如 `ApiError.keys`）为空时不出现，TS 里仍是必填，`ApiError` 读的时候补成空数组。
- **配对码两分钟失效、最多同时 64 个；会话 cookie 30 天、最多 64 个，Gateway 重启就全部失效**（重新 `komo web`）。`Secure` 靠请求的 `X-Forwarded-Proto: https` 判断（Gateway 自己不终止 TLS）；`public_url` 与二维码留给 C3。
- **Host 校验只比主机名，端口不参与**；监听地址 `0.0.0.0` 时 `0.0.0.0` 本身也放行（本机客户端用发现文件里的 `base_url` 连）。cookie 写请求缺 `Origin` 按不同源处理（浏览器发 POST 一定带）；`POST /v1/web/session` 没有 cookie，所以只在带了 `Origin` 时核对同源，curl 照常能换。
- **Paraglide 构建时从 jsDelivr 取 inlang 的 message-format 插件**（`project.inlang/settings.json` 的 `modules`），缓存在 `project.inlang/cache`（已忽略）；第一次构建需要联网。
- **TypeScript 7**（原生 `tsc`）；`pnpm lint` = 自写的 i18n 检查脚本（oxc-parser 解析 JSX）+ oxlint 默认规则（不带配置文件，按 `.gitignore` 跳过生成物）。ESLint 与 typescript-eslint 已移除——后者只支持 TypeScript <6.1。
- pnpm 11 的"发布满一定天数才装"策略把三个新版本拦下，已按提示加进 `pnpm-workspace.yaml` 的 `minimumReleaseAgeExclude`。

**留给 C1 的事**：按 §3 的顺序迁移界面（会话树与对话流先行）；`SessionSummary` 以外的协议类型按用到的补 `ts-rs` 派生并列进 `export_all`；`WireEvent.data` 的按 `type` 收窄类型；错误 `code` 的完整译文表；`public_url` / 二维码 / 设备令牌（C3）；移动端布局（C0 的侧栏在窄屏下直接隐藏，没有会话切换入口）；`/pair` 页面的样式与失败重试体验；`apps/web` 与 `serve.mjs` 的删除。

### 6.2 C1 实际做了什么（2026-11-03）

| 段 | 内容 | 状态 |
|---|---|---|
| 第一段 | 会话列表、对话流与轨迹、改动卡片与改动页（Gateway 端 web.md §16）、输入框、右栏、⌘K | 完成 |
| 第二段 | 待处理页、公共页面部件（`components/page.tsx`、`ui/*`） | 完成 |
| 第三段 | 设置弹窗（模型、配置、环境变量、记忆索引、MCP、Gateway、渠道）、记忆页、事项页、定时页；两处前段遗留（回答 Markdown 的嵌套与链接、上下文卡无窗口时的 `—`）；删除原型 | 完成 |

- **类型来源**：记忆、事项、定时与设置的线上类型都由 kernel 的 `ts` feature 生成到 `packages/protocol`（`protocol/ts.rs` 的 `export_all` 又列了几十个类型，连带 `MemoryItem`、`Matter`、`CronJob`、`ConfigModelsResponse` 等的嵌套类型；`CronJob` 里的 `ModelConfig`、记忆提取元数据里的 `EffortSetting` 这类界面不用的深层类型按 `unknown` 导出）。线上 `skip_serializing_if` 为空时不出现的 `Vec` 字段，在 TS 里按可选导出（`ts(as = "Option<Vec<T>>")`），用时要 `?? []`；没有 `skip_serializing_if` 的 `Option` 在线上是 `null`（`since`、`ModelWindow.window`），按 `T | null` 导出。`packages/api/src/types.ts` 里只剩没法生成的 pi 树投影（`SessionTree`、`TreeEntry`）。
- **偏离与取舍**：见 `web.md` §17。要点：不搬原型"改连的 Gateway"（换 Gateway 就是换地址，Electron 经 `window.komoDesktop.connection.openSettings()`）；页面路由用路径而不是 hash；设置入口 `/settings` 打开弹窗后换回 `/`。
- **原型删除**：`apps/web`（含 `serve.mjs`）删除；演示与截图工具挪到 `apps/client/scripts/`（`replay-model.mjs` 回放真实会话造演示对话，`shot.mjs` 按真实视口截图）。`AGENTS.md`、`web.md`、`DESIGN.md`、`PRODUCT.md`、`roadmap-muse.md` 里指向原型的说法已改；`komo_bot.md` 里的历史记录（2026-10-30 的实现流水）保持原文。
- **遗留**：定时的新建 / 修改仍走对话或 `komo cron`；右栏"连接"卡还没接渠道与 MCP 状态；`WireEvent.data` 按 `type` 收窄的类型与错误 `code` 的完整译文表仍是按需补；移动端的会话切换入口、`/pair` 页面的失败重试体验（C0 遗留）；C2 的 `komoDesktop.connection.openSettings` 只有类型声明（`apps/client/src/komo-desktop.d.ts`），壳还没做。

### 6.3 C2 实际做了什么（2026-11-02）

位置 `apps/desktop`（`@komo/desktop`，TypeScript，Electron 45.2.1 + electron-builder 26.18.0 + electron-updater 6.8.11；
均为 devDependencies，仅 electron-updater 是运行时依赖。前端依赖不进 §13.4 的 Rust 表，清单以 `apps/desktop/package.json` 为准）。

| 项 | 实现 | 状态 |
|---|---|---|
| 加载 | 特权协议 `app://komo`（standard/secure/stream），`src/main/protocol.ts`：无扩展名回落 `_shell.html`，目录穿越拒绝；开发读 `apps/client/dist/client`，打包读 `Resources/client`（`KOMO_DESKTOP_CLIENT_DIR` 可覆盖）；壳 html 的 CSP 按内联脚本内容算 sha256（不开 `unsafe-inline` 脚本） | 完成，冒烟通过 |
| 转发 | `/v1/*`、`/healthz` 由协议处理器转给当前 Gateway，加 `Authorization` 与额外请求头；响应以流转发，渲染进程取消读取时上游随之断开；3xx（访问代理要登录）转成 502 的 `ErrorBody` | 完成；SSE 冒烟通过（24 帧）。**断开传播只按代码路径写，没有单独测** |
| 本机连接 | 读 `$KOMO_HOME/runtime/gateway.json`（默认 `~/.komo`）；401 或连不上时重读一次；没在跑就拉起，找 `komo` 的顺序：`KOMO_BIN` → PATH → `~/.local/bin` → `/opt/homebrew/bin` → `/usr/local/bin`。默认数据目录执行 `komo gateway`；**非默认 `KOMO_HOME` 改用分离的 `komo gateway --foreground`**（komo 自己拒绝让非默认目录碰全局服务单元，`units::owns_unit`） | 完成；非默认目录的拉起分支用临时目录实测通过，默认目录的 `komo gateway` 分支**未实测**（会动真实 launchd 服务） |
| 远程连接 | 地址 + 设备令牌 + 额外请求头（禁止 Authorization/Cookie/Host/Origin 及分帧类头）；保存前测 health、协议版本、令牌、SSE（没有会话时 SSE 无从实测，如实标注）；令牌与请求头值一起用 `safeStorage` 加密存 `userData/connection.json`（0600），加密不可用就拒绝保存、不退化成明文；渲染进程只能看到头的名字 | 完成；`test` 的各失败分支冒烟通过。**`saveRemote` 的落盘与 safeStorage 未实测**（会改 userData；需要钥匙串授权） |
| 切换连接 | 保存或切回后主进程让窗口整页重载到 `app://komo/`（清 Query 缓存与前端状态），托盘通知基线同时作废 | 完成（代码路径），未端到端实测 |
| 版本 | 构建时 `scripts/build-info.mjs` 从 `crates/komo-kernel/src/protocol/mod.rs` 的 `PROTOCOL_VERSION` 与根 `Cargo.toml` 读出，写进 `dist/build-info.json`（读不到就构建失败）；连接时与 `/healthz` 比，不一致停在提示页并说明哪边旧、怎么更新；`version` 不同而协议相同照常用 | 完成；提示页文案与布局做了，**不一致的真实场景没造出来**（只有一个 Gateway 版本） |
| 托盘 / 通知 | **轮询**每 5 秒：`GET /v1/interventions`（托盘标题显示数字；新出现的条目发通知，覆盖 confirm/verify/blocked）与 `GET /v1/sessions`（`kind=task` 的会话由未终态变 `completed`/`failed` 发通知）。选轮询是因为 Gateway 的 SSE 只有每会话一条、没有全局流；第一轮只记基线；窗口在前台时不弹；点击通知打开对应会话 | 完成，**未实测**（没有真实的待处理与任务结束事件，也没点过托盘） |
| 快捷键 | 全局 `CommandOrControl+Shift+K`（`KOMO_DESKTOP_SHORTCUT` 可改）唤起窗口并让界面开 ⌘K | 已注册，**未实测**（自动按键被系统权限挡住） |
| 深链 | `komo://session/<id>`（id 只收 `[A-Za-z0-9._-]`），打包后注册协议；冷启动先记下，连上后打开 | 完成，未实测（需要已安装的 .app） |
| 安全 | `contextIsolation` 开、`nodeIntegration` 关、`sandbox` 开；preload 只 `require("electron")`；导航只允许 `app://`，外链（http/https/mailto）交给系统浏览器；权限请求一律拒绝；非 `app:/devtools:/data:/blob:` 的请求在会话层取消；IPC 校验发送方 frame 在 `app://komo`；CSP | 冒烟确认：`window` 上、cookie、storage、DOM 里都找不到令牌，`require/process` 不存在，跨源 fetch 被挡 |
| 打包 | `electron-builder.yml`：macOS arm64 + x64 的 dmg 与 zip，`latest-mac.yml`，`komo://` 协议；`scripts/package.mjs` 把版本对齐 `[workspace.package].version`，签名与公证由环境变量驱动（`CSC_LINK`/`CSC_NAME`、`APPLE_ID`+`APPLE_APP_SPECIFIC_PASSWORD`+`APPLE_TEAM_ID` 或 `APPLE_API_*`），没有就关掉身份自动发现并出未签名包 | 未签名包本机出成（四个产物 + latest-mac.yml） |
| 发布 | `release.yml` 新增 `desktop` job（macos-latest，同 tag），`release` 等它；产物名 `Komo-<版本>-<架构>.*` 与 `komo-*.tar.gz` / `SHA256SUMS` 不重名 | **CI 没法本地跑**；只核对了产物名与 `sha256sum komo-*.tar.gz` 的通配不会误收 |
| 自动更新 | `electron-updater` 读同一 GitHub 仓库的 `latest-mac.yml`，每 6 小时与启动时检查，下载后弹窗询问重启；菜单「检查更新…」手动检查 | **无法本地验证**（需要已签名的包与新 Release；macOS 无人值守更新要求签名） |

**与上文规格的偏离与取舍**

- 通知与角标用轮询而非 SSE（理由见上表）；等 Gateway 有全局事件流再换。
- `window` 控制 API 已暴露，但主窗口用系统标题栏，没做无边框；C1 若要自绘标题栏，改 `window.ts` 的 `titleBarStyle` 并在界面里留拖拽区。
- 令牌保存时必须重填（渲染进程拿不到已存的令牌，设置页只显示地址与头的名字）。
- `komo` 的查找路径多加了 Homebrew 两个目录（Finder 启动的 App 的 PATH 很短）。
- `pnpm-workspace.yaml` 新增 `allowBuilds`：`electron: true`（postinstall 下载二进制）、`electron-winstaller: false`（只给 Windows 用）。没有新增 `minimumReleaseAgeExclude`（Electron 等本次没被拦）。
- `pnpm build` 会同时编译 desktop 的 TS，但不下载 Electron 以外的东西；`pnpm desktop:build` 才出 `.app`。
- 图标是 `scripts/make-icons.mjs` 生成的占位图，等设计稿。

**界面接口约定**（C1 已接 ⌘K 事件与设置里的连接按钮）（类型在 `apps/desktop/src/shared/api.ts`，界面里按 `window.komoDesktop?` 判断是否在 Electron 里；浏览器里没有这个对象）

- `window.komoDesktop`：`apiVersion: 1`、`platform`、`appVersion`、`protocolVersion`；
  `connection.get()`（`ConnectionInfo`，不含令牌）、`connection.test(input)`、`connection.saveRemote(input)`、`connection.useLocal()`、`connection.retry()`、`connection.openSettings()`；
  `notify({ title, body?, session? })`；`window.do("minimize" | "toggle-maximize" | "toggle-fullscreen" | "close")`；
  `onCommandPalette(handler)` 与 `onOpenSession(handler)`，都返回取消订阅函数。
- **⌘K**：全局快捷键触发后，主进程在窗口加载完的 SPA 上发事件。界面二选一：`window.komoDesktop.onCommandPalette(() => openPalette())`，或监听 DOM 事件 `komo-desktop:command-palette`（无 detail）。
- **打开会话**：`onOpenSession(id => router.navigate(...))`。**登记过才会收到**；没登记时主进程退回整页跳转到 `app://komo/sessions/<id>`（现有 C0 路由），所以不接也能用，只是会整页重载。
- **连接设置入口**：连接设置页是壳自带的 `app://komo/_desktop/page.html?kind=settings`（菜单 ⌘, 与托盘也能进）。界面的设置页放个按钮调 `komoDesktop.connection.openSettings()` 即可，别再自己做一份；Gateway 地址与令牌不进 SPA。
- **版本不一致/连不上**：主进程在进入 SPA 之前就拦在 `_desktop/page.html?kind=mismatch|error`，SPA 不需要处理；SPA 里 Gateway 掉线后的错误提示照常走 `ErrorBody`（转发失败会得到 502 + `error.code = "internal"`）。设置页可以用 `connection.get()` 显示两边版本。
- 请求照旧用同源相对路径 `/v1/*`；Electron 里不会有 cookie，也不要走 `/pair`。
- 开发：`pnpm desktop:dev` 用的是 SPA 的构建产物，没有热更新；改界面仍用 `pnpm dev` 在浏览器里调。

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
