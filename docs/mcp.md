# MCP

> 状态：已落地（2026-10-08）。与 `docs/komo_bot.md` 冲突处以本文为准。codemode 见
> `docs/codemode.md`。2026-10-14 起 MCP 工具默认只在 codemode 里调（§3，照 pi 的 exposure）。

## 1. 形状

MCP 服务器是**操作者接进来的工具来源**，不是第七个基础工具。每个 MCP 工具注册成一个普通的
`Tool`（`komo-runtime/src/tools/mcp.rs`），与内置工具走同一条路：`prepare` 出计划 → 能力面核对 →
`start_call` → 执行 → 落账（2026-10-11 起没有 Policy 与审批，见 `docs/komo_bot.md` §7）。没有第二条执行路径。

```text
config.toml [mcp.servers.<name>]
   │  Gateway 启动时（build_tools）每台后台连，不等握手（§2.4）；30s 连不上算失败，后台隔一阵再连（§2.2）
   ▼
rmcp 客户端（stdio 子进程 / Streamable HTTP）── initialize（取 instructions）+ tools/list
   ▼
McpTool × N，名字 mcp__<server>__<tool>，挂进工具目录（exposure = hidden 的不挂）
   ▼
Profile 的 tools 照常挑（省略 = 全部，含 MCP）；Run 受理时冻结进能力面
   ▼
exposure = codemode（默认）：不进交给模型的 schema，脚本里调；direct：照常声明
```

## 2. 配置

```toml
[mcp.servers.github]
command   = "npx"
args      = ["-y", "@modelcontextprotocol/server-github"]
env       = ["GITHUB_TOKEN"]          # 从 .env 带进子进程的变量名
read_only = ["search_issues", "get_issue"]

[mcp.servers.docs]
url              = "https://mcp.example.com/mcp"
bearer_token_env = "DOCS_TOKEN"       # .env 里的变量名
tools            = ["lookup"]          # 白名单；省略 = 服务器列出的全部
description      = "内部文档检索"      # 系统提示里这台服务器的那一行（§3）
exposure         = "codemode"          # 默认；direct / hidden（§3）
tool_exposure    = { lookup = "direct", "delete_*" = "hidden" }
outward          = ["publish_*"]       # 对外工具：无人值守的 Run 调它先确认（docs/confirm.md）
```

- `command` 与 `url` 恰好写一个；`args` / `env` 只配 `command`，`bearer_token_env` 只配 `url`。
- 凭证只在 `.env`。**stdio 子进程不继承 Gateway 的环境**：先清空，只放回 `PATH`、`HOME`、`USER`、
  `LANG`、`TMPDIR`、`SHELL`，再加 `env` 列出的变量。HTTP 的 token 同理。
- 整段**热重载即生效**（`ConfigSnapshot.mcp`，2026-10-17 起；之前在 `start_only` 里、要重启）：
  增删改服务器不用 `komo gateway restart`，见 §2.3。
- 校验：服务器名只能是字母、数字、`_`、`-`（它要进工具名）；`read_only` / `outward` 写了 `tools`
  白名单之外的名字是警告；声明了 `outward` 却没有渠道配 `home_chat` 也是警告（确认卡投不到人）。
- `outward`（2026-10-29）：原名或带 `*` 的模式，任一匹配就算对外。同 `read_only`，只信操作者的声明，
  不信服务器自报。codemode 脚本里在无人值守的 Run 中调它直接被拒，要到顶层调；所以无人值守的 Run 把
  `codemode` 暴露的对外工具也声明给模型（`docs/confirm.md` §7）。

### 2.1 OAuth（2026-10-14）

```toml
[mcp.servers.devops-mcp-prod]
url   = "https://devops-mcp-mdw.shoplazza.site/mcp"
oauth = true                           # 与 bearer_token_env 只能写一个
```

- `komo auth mcp login <name>`：**不经 Gateway**（同 `komo auth codex login`），在本机发现授权
  服务器（RFC 9728 / 8414）→ 动态注册客户端（RFC 7591，`client_name = "komo"`）→ 打开浏览器走
  授权码 + PKCE → 回调只听 `127.0.0.1` 的一个随机端口（5 分钟超时，`/callback` 以外的路径回 404）
  → 换 token。`client_id` 与 token 写进 `<数据目录>/mcp/<name>/credentials.json`（临时文件 +
  rename，0600），不进 `.env`。不用重启 Gateway（§2.2）。
- `komo auth mcp status`：每个 `oauth = true` 的服务器一行——登录与否、过期时间、能否自动刷新；
  不打印 token。
- Gateway 连接时用这份文件装 rmcp 的 `AuthorizationManager`（`FileCredentialStore`，
  `komo-runtime/src/mcp_auth.rs`）；没登录过就记 warn、这台服务器等登录（§2.2），报错指名要跑的命令。
  取用与刷新交给 rmcp：每次请求从文件取 token，快过期时用 refresh token 换新并写回；服务器拒绝
  token 时静默刷新重试一次。服务器不发 refresh token 时，过期后要重新 `login`。

### 2.2 登录后不用重启（2026-10-16，照 pi）

照 pi 的 `McpServerConnection`（`packages/coding-agent/src/extensions/mcp/runtime.ts`）：每台配置过的
服务器一个连接对象（`McpConnection`，全部放在 `McpHub` 里），Gateway 整个生命周期都在；连没连上是它
的状态，不是"有没有这个对象"。以前启动时被 401 拒了就整台跳过、工具一个不挂，登录之后只能重启。

- **状态**：已连上 / 等登录 / 连不上。只有 `oauth = true` 的服务器会进"等登录"；bearer token 在
  `.env` 里，改它走配置。断开不单列：会话自己报已关闭，下一次调用时重连（pi 的 `getClient`）。
- **进"等登录"时记下凭证文件此刻的内容**（pi 的 `tokensAtSignIn`）。**每条 Run 受理之前**比一次
  （pi 在每个 turn 开始时）：文件变了 = 有人跑过 `komo auth mcp login`，当场重连；连上了就把工具追加
  进执行器的工具目录，这条 Run 的能力面里就有它们。没人登录时只是读一下文件。
- **工具目录追加与撤销**：挂着的名字不换（`ToolExecutor::install`）；只有服务器从配置删掉或改了配置
  时才撤（`ToolExecutor::withdraw`，§2.3）。已经受理的 Run 能力面已冻结；新名字从下一条 Run 起才进来。
- **运行中凭证被拒**（401，或 token 过期又刷新不了）：这台服务器转成"等登录"，这次调用报
  `failed`，正文指名 `komo auth mcp login <name>`。401 说明服务器没处理这个请求，所以判"没执行"、
  不判"结果不明"；**也不在登录后自动重试**（照 pi），要不要再调由模型决定。之后登录了，下一次调用
  （或下一条 Run 受理时）重连。
- **服务器段**：新连上的服务器从下一条 Run 起出现在系统提示里，这个会话的前缀缓存因此失效一次（同
  skills 目录变了）。instructions 只记第一次连上时的，重连不换，免得摘要在会话中途变样。pi 是往对话
  末尾追加新的服务器段，komo 暂不做（§6）。
- **第一次没连上的后台重连**（2026-10-21）：启动或热重载新加时"连不上"（不是登录问题，比如网络断了、
  命令不存在、本机代理比 Gateway 晚起来）的服务器，**后台**隔一阵再连：5 秒起每次翻倍，至多每 60 秒
  一次（`RETRY_FIRST` / `RETRY_MAX`），直到连上、要登录或从配置删掉；同一个原因只 warn 第一次，之后
  记 debug。连上了，工具在下一条 Run 受理时挂进来，同登录后连上的一样。Run 受理时仍不重试——每条 Run
  都去连一台坏掉的服务器，会让受理平白多等一个握手超时。以前没有这一条：它的工具没挂上、没人调得到，
  "下一次用到时再连"永远不会发生，只能 `komo gateway restart`（实测：magpie 代理晚 13 分钟起来，
  devops-mcp-prod 与 cloudflare 一直不可用）。配置一字未改的 `komo config reload` 仍不重连它，也不需要。
  连上过、后来断了的不走这条：工具已经挂着，调用时自己重连。
- 实现：`komo-runtime/src/mcp.rs`（`McpHub` / `McpConnection`）、`GatewayState::refresh_mcp`。

### 2.3 改 `[mcp.servers]` 热重载即生效（2026-10-17，照 pi）

照 pi 的 `mcp_servers_change`（`packages/coding-agent/src/extensions/mcp/index.ts`）。配置重载（文件
mtime、`komo config reload`、`SIGHUP`）的变化键有 `mcp` / `mcp.*` 时，`McpHub::reconcile` 按服务器名
比对新旧配置：

- **新增**：建 `McpConnection`，**后台**连（pi 的 `session_start` 不 await 连接），重载不等握手。
- **删除**：从 hub 拿掉，它的工具从执行器目录撤掉（pi 的 `hideTools`，`ToolExecutor::withdraw`），连接断开。
- **修改**（`McpServerConfig` 不相等）：**先删后加**，同 pi 对配置变了的服务器的处理——旧工具撤掉、
  旧连接断开，新连接用新配置后台连上，工具在下一条 Run 受理时重新挂上。
- 配置没变的服务器原样留着，连接不动。校验不过的配置整份不装（`docs/komo_bot.md` §3 第 1 步），
  服务器一台也不动。
- 增删之后接着重渲 skills 目录行：`requires_tools` 按撤完 / 挂上之后的工具名门控。
- `komo config reload` 的回应多一段 `mcp`：每台服务器此刻的状态（连接中 / 已连上 / 要登录 / 失败）。
  因为不等握手，刚加的、刚改的多半是"连接中"。

**受理 Run 时**（照 pi 的 `waitForDirectServers`）：只等还在连、而且有工具挂得上（不全是 `hidden`）的
服务器，上限 10 秒（pi 的 `startupWaitMs` 默认值），从这台服务器开始连时算起——挂住的服务器至多拖慢这
一段，之后的 Run 不再等它。超时不阻塞受理，这条 Run 先不带它的工具。pi 只等直出的服务器，因为它的脚本
侧调用时才按名字找服务器；komo 的能力面受理时按名字冻结，没挂上的工具这条 Run 从头到尾调不到，所以只
在 codemode 里调的服务器也等（2026-10-17 随 §2.4 改）。

**对在跑的 Run 的影响**（komo 比 pi 多的一条：pi 只在空闲时重载，komo 多会话并发，重载时总可能有 Run
在跑）：

- 能力面受理时已冻结，不因重载变化。撤掉的工具不再出现在之后的模型请求的 schema 里。
- **正在执行的那一次 MCP 调用不被掐断**：它握着自己那份会话（`Arc<McpServer>`），hub 放掉它的那份后，
  调用照常返回；最后一个引用释放时 stdio 子进程才退。改了配置时也一样，旧子进程与新子进程短暂并存。
- 之后再调已删除 / 改了配置的服务器的工具（顶层调用或 codemode 子调用）：拿到一句工具错误——"MCP 服务器
  X 已从配置移除，它的工具不能再调"，或"配置改了，正在重连；连上后从下一条 Run 起可用"。不执行、不
  panic、不停下来问人；同名工具重新挂上之后又调得到。

实现：`McpHub::reconcile` / `wait_for_startup`、`McpConnection::close`、`ToolExecutor::withdraw`、
`GatewayState::reconcile_mcp`、`komo-gateway/src/reload.rs` 的 `apply`。

### 2.4 启动不等握手（2026-10-17，照 pi）

以前 `build_tools` 等每台服务器握手完（每台最多 30 秒）才往下走：一台卡住的服务器让整个 Gateway 晚最多
30 秒就绪，这段时间各渠道都收不到消息。现在同热重载新加的服务器一样（pi 的 `session_start` 不 await
连接）：

- **启动**：`McpHub::start` 建好每台的连接对象、各自后台连，立刻返回；`build_tools` 一个 MCP 工具也不挂。
  刚起来的服务器状态是"连接中"。
- **第一条 Run 受理时**挂工具：`GatewayState::refresh_mcp` 先按 §2.3 的规则等（至多开始连之后 10 秒），
  再把连上的工具挂进目录、重渲 skills 目录行。不另写一条路。正常服务器握手只要百来毫秒，第一条 Run 基本
  不用等。
- **被接续的 Run**（Gateway 重启后从账本里接着跑的）不经过受理：它冻结的能力面里可能有 `mcp__memos__*`
  这类名字，而那台服务器此刻还在连、执行器目录里还没有这个工具。执行器认出"这个名字属于一台还在连的
  服务器"时先等它（`ToolExecutor::await_mcp` → `McpHub::await_tools`，同一个 10 秒上限），连上了挂工具
  再执行；到点没连上、连接失败、要登录的，报一句指名服务器的工具错误（"MCP 服务器 X 还没连上（等了 10
  秒）…"/"MCP 服务器 X 连接失败（原因）…"），**不再是"没有名为…的工具"**。Run 照常往下走，不停下来
  问人。覆盖三处：装配这一段的 schema 时（`segment`，接续的 Run 的 schema 与服务器段才与受理时一样）、
  顶层调用（`begin`）、codemode 子调用（`durable_nested_call`；脚本的发现目录装配前也先等一次）。
- **怎么认归属**：工具还没 `tools/list` 之前只有名字可用。冻结的能力面（`AgentSurface`）只存名字、不存
  来源，所以按命名规则认：`mcp__<server>__<tool>`，`<server>` 照 `tool_name` 的规则换掉特殊字符后比前缀。
  服务器名里可以有 `__`，一个名字可能被不止一台认领，几台一起等。认不出服务器的名字（不是 MCP 的、
  服务器已从配置删掉的）照旧报"没有这个工具"；服务器连上了却没列出这个工具的，同理。
- 受理时那次"等登录的凭证变了没有"（`McpHub::refresh`）**不等正在连的服务器**：它握着连接锁，等它会把
  受理拖到握手超时（30 秒）而不是上面的 10 秒。锁被占着就跳过，下一条 Run 再比。
- 这里的等待**不重连**：第一次连接已经有了结果（失败）的服务器当场报原因，同 §2.2 "启动时连不上的服务器
  Run 受理时不重试、只在后台重连"一个口径；后台重连正握着连接锁时报"正在重连"。

不做"第一次调用才起子进程"的完全懒加载：直出工具的 schema 受理时就要知道。

### 2.5 手动重连：`komo mcp reconnect`（2026-10-21）

后台重连（§2.2）至多隔 60 秒才连一次；连上过的服务器断了也要等下一次调用才发现。人知道服务器刚好了
（本机代理刚重启、刚换了上游）时不该等，也不该为此重启 Gateway。

- `komo mcp reconnect [SERVER...]` → `POST /v1/mcp/reconnect {"servers": [...]}`，空 = 全部配置过的服务器。
  点了配置里没有的名字 400，一台也不动。
- 每台**不管此刻什么状态**都断开重连一次（`McpConnection::reconnect`，拿连接锁，正在连的那次连完再连），
  几台一起连，**等握手**（每台至多 30 秒）后交回各自的状态——与热重载不同，人手动敲的命令要当场看到连没连上。
- 连着的会话换掉：在途调用握着旧的那份，返回之后才断开（同 §2.3）。工具名照旧挂着，不撤；重连后新列出来
  的工具挂进执行器，从下一条 Run 起可用（`GatewayState::reconnect_mcp`），并重渲 skills 目录行。
- 要登录的服务器重连之后还是"要登录"，提示照旧指向 `komo auth mcp login`。没连上的同第一次没连上一样后台
  隔一阵再连（§2.2）；同一台同一时刻只有一个后台重连在跑。
- instructions 仍只记第一次连上时的（§2.2），重连不换。

## 3. 暴露方式（2026-10-14，照 pi）

MCP 工具默认**不声明给模型**，只在 codemode 脚本里调：一台服务器几十个工具的 schema 每次请求都带，
大半用不上（实测 devops-mcp-prod 38 个工具、schema 约 39K 字符）。照 pi 的 `exposure`：

| 值 | 模型 schema | codemode 脚本 | 用途 |
|---|---|---|---|
| `codemode`（默认） | 不声明 | 能调；脚本用 `search_tools` / `describe_tool` / `describe_namespace` / `ALL_TOOLS` 找 | 一般的服务器 |
| `direct` | 像内置工具一样声明 | 能调 | 工具少、几乎每次都用 |
| `hidden` | — | — | 不挂（不进工具目录，能力面里也就没有） |

- `tool_exposure` 按工具覆盖：键是服务器上的工具原名，或带 `*` 的模式。原名优先；模式之间**字面部分
  最长的赢**（一样长按键的字典序）。pi 是"第一条匹配的赢"，靠 JSON 键序；TOML 表读进来不保序，所以改成
  最具体的赢。没匹配上任何工具的键在连接时记 warn。没有 pi 的 `deferred`：komo 没有 `tool_search`。
- **隐藏 schema 不等于撤权。** `codemode` 的工具仍在 Run 的能力面里（`AgentSurface` 不变），codemode
  子调用照常按能力面核对；顶层发来的同名调用（升级前的续跑、模型凭记忆拼出名字）也照常执行——
  它本来就在能力面里，没有越权，旧会话的历史与恢复也不用改。`tool://` 按能力面答，读得到它们。
- **系统提示的服务器段**（主 Agent 与子代理都有）：能力面里有 `codemode` 工具、而且这次调得到
  codemode（Profile 给了）时，每台服务器一行 `- mcp__<server>：<摘要>`。摘要是配置的
  `description`，没写就是服务器 instructions 的**第一行**（pi 的 `serverSummary`）。整段 4096 字、每台
  250 字封顶：先缩短摘要，名字都放不下时后面的只计数。codemode 调不到时这段不出现——说了"在脚本里
  调"也调不到。内容只取决于能力面与连上过的服务器：同一条 Run 里不变；登录后才连上的服务器从下一条
  Run 起多出一行（§2.2），此外 Gateway 不重启就不变，不破坏前缀缓存。
- **instructions 全文不进任何提示**（照 pi）：握手时服务器交回的 `instructions` 是外部文本，只在脚本里
  `describe_namespace("<server>")` 读得到，系统提示只取它的第一行作摘要。
- codemode 调不到（Profile 没给它）时**不自动改成直接声明**（照 pi）：这些工具这次调不到，要直接
  调就把那台服务器写成 `exposure = "direct"`。（2026-10-15 之前"沙箱自检没过"也会让 codemode 不挂、启动时 warn
  列出被搁浅的工具；Monty 没有自检，这一条随之没了。）

脚本里的发现函数在 worker 里本地做完，不经宿主、不进 journal（`docs/codemode.md` §6）。

## 4. 只读与恢复

计划是 `Operation::McpCall { server, tool, read_only }`。`read_only` 来自**操作者**的
`read_only` 列表，不信服务器自报的 `readOnlyHint`：自称安全的东西决定不了恢复方式。调用一律
执行（2026-10-11 起没有 Policy 与审批，原来 strict 下的 `mcp-call` Ask 与 `policy.toml` 里的
按工具免问一并删掉）；`read_only` 只决定恢复方式：

| | 恢复（顶层调用与 codemode 子调用同一条准则） |
|---|---|
| `read_only` 里的工具 | `SafeReread`，`McpTool::replay` 也是 safe：重启后重读一次 |
| 其余 | `NoSafeRecovery`：重启后不重跑，中断结果交给模型判断（`docs/komo_bot.md` §8.6） |

codemode 脚本里两类都能调（2026-10-13 起；之前只放行 `read_only` 里的）：子调用逐次记进
journal，已交付的写结果原样接回，不会重做（`docs/codemode.md` §5、§7）。

调用失败：只读工具当普通失败；其余工具在传输层断掉时报"结果不明"（`ToolError::Uncertain`）
——请求可能已经到了服务器。超时由执行器的 `call_timeout` 兜底。

## 5. 结果

- 模型看到的正文：文本块原样；图片 / 音频只留一句说明（不把 base64 塞进上下文）；资源给出它
  的文本或 URI；一段文本都没有时用 `structuredContent` 的 JSON。超长时照常由投影截头尾、给
  `artifact://` 引用（§8.3）。
- 完整的 `CallToolResult`（含 `structuredContent`、`isError`）存进 `output.json` 的 `result`。
- `isError: true` → `tool.result` 状态 `failed`，正文照常交给模型。

## 6. 不做的（首版）

- （OAuth 原在此列，2026-10-14 落地，见 §2.1。）客户端凭证（client credentials）、
  企业托管授权等其他 OAuth 流程仍不做。
- （断线重连原在此列，2026-10-16 落地：断了的下一次调用时重连，见 §2.2。）
- （热加载原在此列，2026-10-17 落地：增删改服务器热重载即生效，见 §2.3。）
- `resources` / `prompts` / 采样（sampling）/ elicitation：只用 tools。
- `deferred` 暴露（先搜再声明，pi 的 `tool_search`）：默认的 `codemode` 已经不把 schema 塞进上下文，
  而且声明从头到尾不变。tool_search 每加载一次就改一次工具声明：pi 靠 provider 原生的对话中途追加
  （Anthropic `inline-tools` beta 的 `tool_addition` + 一个 `defer_loading` 占位工具；Responses 的
  `additional_tools` 或客户端 `tool_search_call/output`）保住前缀缓存，Chat Completions 退回顶部完整
  列表、整段失效。komo 三个适配器都只会发顶部完整列表，做之前要先逐个 provider 接中途追加并实测
  `cache_read`（中转站是否转发 beta 未验证）。2026-10-14 决定暂不做。
- 服务器段变化后在对话末尾追加新的一段（pi 的做法）：komo 的段只在登录后才连上的服务器出现时变一次
  （§2.2），直接换系统提示，代价是那个会话的前缀缓存失效一次。

## 7. 验收

- `komo-gateway/tests/mcp`（真 Gateway + `tests/mcp/server.py` 这个真 stdio 服务器）：
  子进程只拿到声明的变量；有副作用的工具只执行一次；起不来的
  服务器不影响别的；默认 `codemode` 时 schema 里没有 `mcp__*`、系统提示有服务器那一行（只取
  instructions 第一行），脚本里 `describe_namespace` 拿得到全文、`search_tools` 用中文搜得到、调用只
  执行一次；`direct` 进 schema、`hidden` 不挂、只剩直出工具时不出服务器段。
- `komo-gateway/tests/mcp` 的 OAuth 两条（`tests/mcp/oauth_server.py`：真 HTTP 服务器，带 RFC 8414 /
  9728 发现端点，只认文件里写的 bearer token）：
  `logging_in_after_start_mounts_the_tools_on_the_next_run`（启动时没登录、工具不在；写进凭证后同一台
  Gateway 的下一条 Run 调得到，服务器执行一次）、
  `a_rejected_token_asks_for_login_and_the_next_login_reconnects`（服务器换 token 后调用记 `failed`、
  正文指名 `komo auth mcp login secure`、服务器没执行；重新登录后下一条 Run 调通）。
- `komo-gateway/tests/mcp` 的热重载四条（§2.3）：`a_server_added_by_reload_is_seen_by_the_next_run`、
  `a_server_removed_by_reload_is_gone_for_the_next_run`、`a_running_run_is_told_the_server_was_removed`
  （在跑的 Run 停在 `gate` 上时删掉服务器，之后再调拿到"已从配置移除"、Run 照常完成）、
  `an_invalid_mcp_reload_leaves_the_servers_alone`。
- `komo-gateway/tests/mcp` 的启动三条（§2.4）：`a_stuck_server_does_not_delay_startup`（一台握手永远不回
  的服务器下 Gateway 5 秒内就绪，第一条 Run 的 schema 里有正常服务器的工具并调通）、
  `a_resumed_run_waits_for_its_mcp_server`（`Fault::BeforeStartRun` 造出受理过、没开跑的 Run，重启时服务器
  握手前睡 2 秒：顶层调用与脚本里的调用都等它连上再调通）、
  `a_resumed_run_is_told_why_its_mcp_server_is_missing`（重启后命令不存在：两处都报"MCP 服务器 demo 连接
  失败"，Run 照常完成、不问人）。
- `komo-runtime`：`mcp::tests` 的 `start_returns_without_waiting_for_a_stuck_server`、
  `await_tools_waits_for_the_owner_and_explains_the_rest`、`a_tool_name_is_claimed_by_its_server_prefix`、
  `a_call_to_a_tool_not_yet_mounted_waits_for_its_server`（执行器顶层调用先等服务器再调通）。
- `komo-runtime`：`mcp::tests` 的 `reconcile_*`（增、删、改）与
  `removing_a_server_lets_the_call_in_flight_finish` / `changing_a_server_lets_the_call_in_flight_finish`
  （在途调用照常返回，子进程活到它返回之后才退）；
  `executor::tests::a_withdrawn_tool_says_why_and_comes_back_when_reinstalled`。
- `komo-kernel`：`protocol::config::tests`（`tool_exposure` 原名 > 最具体模式 > 服务器默认，`*` 匹配；
  `mcp_changes_are_not_start_only`）。
- `komo-agent`：`prompt::tests`（服务器段每台一行、摘要取法、主 / 子代理都有、预算内截断与计数）。
- `komo-runtime`：`executor::tests::codemode_only_tools_stay_callable_but_out_of_the_schema`、
  `config::tests::mcp_exposure_defaults_to_codemode_and_loads_overrides`。
- `komo-runtime`：`mcp::tests`（命名、白名单）、`config::tests`（解析、`command`/`url` 互斥）、
  `config::validate::tests`（服务器名、`read_only` 越界）。
- `komo-runtime`：`mcp_auth::tests`（回调跳过别的路径、`error=` 回调报错不换码、凭证文件往返与
  0600、status 不带 token）、`config::tests::an_oauth_mcp_server_loads_and_refuses_a_bearer_token_beside_it`。
- （`komo-kernel` 的 `policy::defaults::tests::mcp_reads_pass_and_other_mcp_calls_ask_under_strict`
  随 Policy 于 2026-10-11 删除。）
