# MCP

> 状态：已落地（2026-10-08）。与 `docs/komo_bot.md` 冲突处以本文为准。codemode 见
> `docs/codemode.md`。

## 1. 形状

MCP 服务器是**操作者接进来的工具来源**，不是第七个基础工具。每个 MCP 工具注册成一个普通的
`Tool`（`komo-runtime/src/tools/mcp.rs`），与内置工具走同一条路：`prepare` 出计划 → 能力面核对 →
`start_call` → 执行 → 落账（2026-10-11 起没有 Policy 与审批，见 `docs/komo_bot.md` §7）。没有第二条执行路径。

```text
config.toml [mcp.servers.<name>]
   │  Gateway 启动时（build_tools）并行连接，30s 超时；连不上的记 warn 跳过
   ▼
rmcp 客户端（stdio 子进程 / Streamable HTTP）── tools/list
   ▼
McpTool × N，名字 mcp__<server>__<tool>，挂进工具目录
   ▼
Profile 的 tools 照常挑（省略 = 全部，含 MCP）；Run 受理时冻结进能力面
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
```

- `command` 与 `url` 恰好写一个；`args` / `env` 只配 `command`，`bearer_token_env` 只配 `url`。
- 凭证只在 `.env`。**stdio 子进程不继承 Gateway 的环境**：先清空，只放回 `PATH`、`HOME`、`USER`、
  `LANG`、`TMPDIR`、`SHELL`，再加 `env` 列出的变量。HTTP 的 token 同理。
- 整段是**只在启动时生效**的键（`ConfigSnapshot.start_only.mcp`）：改了重载会报"需要
  `komo gateway restart`"，不静默忽略。
- 校验：服务器名只能是字母、数字、`_`、`-`（它要进工具名）；`read_only` 写了 `tools` 白名单
  之外的名字是警告。

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
  rename，0600），不进 `.env`。之后 `komo gateway restart`。
- `komo auth mcp status`：每个 `oauth = true` 的服务器一行——登录与否、过期时间、能否自动刷新；
  不打印 token。
- Gateway 连接时用这份文件装 rmcp 的 `AuthorizationManager`（`FileCredentialStore`，
  `komo-runtime/src/mcp_auth.rs`）；没登录过就记 warn 跳过这个服务器，报错指名要跑的命令。
  取用与刷新交给 rmcp：每次请求从文件取 token，快过期时用 refresh token 换新并写回；服务器拒绝
  token 时静默刷新重试一次。服务器不发 refresh token 时，过期后要重新 `login`——新 token 写进同一
  份文件，在跑的 Gateway 下一次请求就读到，不必重启。

## 3. 只读与恢复

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

## 4. 结果

- 模型看到的正文：文本块原样；图片 / 音频只留一句说明（不把 base64 塞进上下文）；资源给出它
  的文本或 URI；一段文本都没有时用 `structuredContent` 的 JSON。超长时照常由投影截头尾、给
  `artifact://` 引用（§8.3）。
- 完整的 `CallToolResult`（含 `structuredContent`、`isError`）存进 `output.json` 的 `result`。
- `isError: true` → `tool.result` 状态 `failed`，正文照常交给模型。

## 5. 不做的（首版）

- （OAuth 原在此列，2026-10-14 落地，见 §2.1。）客户端凭证（client credentials）、
  企业托管授权等其他 OAuth 流程仍不做。
- 断线重连：服务器进程挂了，它的工具调用会失败，重启 Gateway 才重新连。
- 热加载：增删服务器要重启。
- `resources` / `prompts` / 采样（sampling）/ elicitation：只用 tools。
- `deferred` 暴露（先搜再声明）：工具多的服务器用 `tools` 白名单收窄。

## 6. 验收

- `komo-gateway/tests/mcp`（真 Gateway + `tests/mcp/server.py` 这个真 stdio 服务器）：
  子进程只拿到声明的变量；有副作用的工具只执行一次；起不来的
  服务器不影响别的。
- `komo-runtime`：`mcp::tests`（命名、白名单）、`config::tests`（解析、`command`/`url` 互斥）、
  `config::validate::tests`（服务器名、`read_only` 越界）。
- `komo-runtime`：`mcp_auth::tests`（回调跳过别的路径、`error=` 回调报错不换码、凭证文件往返与
  0600、status 不带 token）、`config::tests::an_oauth_mcp_server_loads_and_refuses_a_bearer_token_beside_it`。
- （`komo-kernel` 的 `policy::defaults::tests::mcp_reads_pass_and_other_mcp_calls_ask_under_strict`
  随 Policy 于 2026-10-11 删除。）
