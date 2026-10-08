# MCP

> 状态：已落地（2026-10-08）。与 `docs/komo_bot.md` 冲突处以本文为准。codemode 见
> `docs/codemode.md`。

## 1. 形状

MCP 服务器是**操作者接进来的工具来源**，不是第七个基础工具。每个 MCP 工具注册成一个普通的
`Tool`（`komo-runtime/src/tools/mcp.rs`），与内置工具走同一条路：`prepare` 出计划 → Policy /
审批 → `start_call` → 执行 → 落账。没有第二条执行路径。

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

## 3. Policy 与恢复

计划是 `Operation::McpCall { server, tool, read_only }`。`read_only` 来自**操作者**的
`read_only` 列表，不信服务器自报的 `readOnlyHint`（§7.1：自称安全的东西不进 Policy）。

| | strict | auto | 恢复 |
|---|---|---|---|
| `read_only` 里的工具（`OperationMatch::McpRead`） | Allow（`mcp-read`） | Allow | `SafeReread`：中断后重读 |
| 其余（`OperationMatch::McpCall`） | Ask（`mcp-call`），卡片上是服务器、工具与完整参数 | Allow | `NoSafeRecovery`：中断后停下问人 |

单个工具要免问，在 `policy.toml` 按工具名写一条：`tools = ["mcp__github__create_issue"]`。

调用失败：只读工具当普通失败；其余工具在传输层断掉时报"结果不明"（`ToolError::Uncertain`）
——请求可能已经到了服务器。超时由执行器的 `call_timeout` 兜底。

## 4. 结果

- 模型看到的正文：文本块原样；图片 / 音频只留一句说明（不把 base64 塞进上下文）；资源给出它
  的文本或 URI；一段文本都没有时用 `structuredContent` 的 JSON。超长时照常由投影截头尾、给
  `artifact://` 引用（§8.3）。
- 完整的 `CallToolResult`（含 `structuredContent`、`isError`）存进 `output.json` 的 `result`。
- `isError: true` → `tool.result` 状态 `failed`，正文照常交给模型。

## 5. 不做的（首版）

- OAuth（只支持 `.env` 里的 bearer token）。
- 断线重连：服务器进程挂了，它的工具调用会失败，重启 Gateway 才重新连。
- 热加载：增删服务器要重启。
- `resources` / `prompts` / 采样（sampling）/ elicitation：只用 tools。
- `deferred` 暴露（先搜再声明）：工具多的服务器用 `tools` 白名单收窄。

## 6. 验收

- `komo-gateway/tests/mcp`（真 Gateway + `tests/mcp/server.py` 这个真 stdio 服务器）：
  只读工具不问人、子进程只拿到声明的变量；有副作用的工具批了才执行、只执行一次；起不来的
  服务器不影响别的。
- `komo-runtime`：`mcp::tests`（命名、白名单）、`config::tests`（解析、`command`/`url` 互斥）、
  `config::validate::tests`（服务器名、`read_only` 越界）。
- `komo-kernel`：`policy::defaults::tests::mcp_reads_pass_and_other_mcp_calls_ask_under_strict`。
