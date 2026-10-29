# 对外动作确认（P3）

> 状态：草案（2026-10-29），待操作者认可。方向已在 [`roadmap-muse.md`](roadmap-muse.md) §4.1 拍板，这里定做法。
> 不带文件名的章节号指 [`komo_bot.md`](komo_bot.md)。

## 1. 要解决什么

2026-10-11 删掉审批，是因为审批都发生在操作者自己发起、自己在看的 Run 里，没在判断什么（§7）。P1 之后
目标与领域能挂 Cron 和 monitor，P2 之后会有更多**没人在看的 Run**。这些 Run 可能替用户给别人发消息、
提工单、改线上配置。一句话的边界：

**由触发器发起的 Run，调用声明为"对外"的工具时，先发一张确认卡，等操作者答了再执行。** 用户发起的 Run
维持现状；不恢复 Policy，不恢复规则表，不解析命令文本。

不做的：

- **bash 不在内。** bash 里的 `lark-cli`、`curl` 照样能对外，确认挡不住，这一点写明不装作挡得住。真正的
  边界来自执行环境（§7.3、`roadmap-muse.md` P5）。
- 没有"总是允许"。一张卡只管这一次调用的这一份 `ExecutionPlan`；想少收卡，就把工具从对外清单里拿掉。
- 不做按钮卡片。现在的飞书 / Telegram 都没有卡片回调（删审批时一起删了），先用文字卡 + `/answer`，
  按钮留给 P4。

## 2. 决定（待认可）

| # | 问题 | 建议 | 理由 |
|---|---|---|---|
| D1 | "触发器发起"怎么判 | 看 Run 的**第一条输入**：不是人打的字（`cli.input`、白名单里的 `chat.input`）就算 | 第一条输入已在 `komo.live.run.inputs[0]`，不用新存储；后插话不改出身 |
| D2 | 后台任务会话算不算 | 算。`kind = task` 会话的 Run 都按无人值守处理 | 子代理跑起来以后父 Run 的人不一定在看；与提取把 task 会话当"不是用户说的"一致（§9.10） |
| D3 | "对外"由谁声明 | 内置工具 `Tool::outward()`（默认 `false`，现有内置工具都不对外）；MCP 在服务器配置里写 `outward = ["工具名或模式"]` | 同 `read_only` / `tool_exposure`：操作者或工具声明，不信服务器的 hint，不解析文本 |
| D4 | 等多久 | `[confirm] timeout_mins`，默认 1440（24 小时，同 monitor 上限）；到点按"没确认"处理：不执行，告诉模型 | 不答就不做；不让一个 Run 永远挂着 |
| D5 | codemode 里嵌套的对外调用 | 无人值守时**直接拒**，提示模型在顶层调同名工具 | 脚本跑在 worker 的墙钟里，等不了一张卡；顶层调用本来就在能力面里（`docs/mcp.md` §3） |
| D6 | 卡发到哪 | 同现有 intervention：有来源渠道的发回来源，Cron 发 home chat；都没有就只在 `komo intervention list` 里 | 复用 §11.4 的投递与重放；**现在飞书 `home_chat` 是注释掉的**，Cron 的卡会没人收到，见 §6 |

## 3. 无人值守的判定（D1、D2）

`ToolTask::context`（`komo-runtime/src/durable/harness/tool.rs`）准备工具上下文时已经读出 Run 的第一条
输入。在这里算一个 `attended: bool` 放进 `ToolContext`：

| 第一条输入 | 会话 | attended |
|---|---|---|
| `cli.input`（TUI、HTTP） | 非 task | 是 |
| `chat.input`，发送者是操作者（`allow_from` / `allow_tenant`） | 非 task | 是 |
| `cron.input`、请求键 `cron:` / `cron-manual:` | 任何 | 否 |
| `monitor.event` / `monitor.end`（请求键 `monitor:` / `monitor-end:`） | 任何 | 否 |
| `task.result`（请求键 `task-result:`） | 任何 | 否 |
| 任何 | `kind = task` | 否 |

- 人在无人值守的 Run 里插话（steer）不把它变成有人值守：出身看第一条，保守。
- 判定只读已提交的事实，重启后算出来一样。
- `ToolEnv.source` 现在恒为 `Interactive`；这次顺手按同一张表填对（`Cron` / `Interactive`），`ExecutionPlan.source`
  随之正确，确认卡和事后审计都读它。

## 4. 对外声明（D3）

- kernel `Tool` trait 加 `fn outward(&self) -> bool { false }`。内置工具这次没有一个返回 `true`；以后加发消息
  之类的专用工具时由它自己声明。
- `McpServerConfig` 加 `outward: Vec<String>`，匹配规则同 `tool_exposure`（原名优先，模式里字面最长的赢）。
  `McpTool::outward()` 读它。配置热重载，下一次调用起生效。
- 本机配置建议（落地时给操作者看，操作者改）：`devops-mcp-prod` 的提单、流转、审批、Apollo 变更与重启类，
  `cloudflare` 的 `execute`。只读服务器（codegraph、prometheus 等）不写。
- `komo config check` 对没匹配上任何工具的模式记 warn（同 `tool_exposure`）。

## 5. 等确认（D4）

在工具任务的 `prepare` 成功之后、提交意图（`Checkpoint::Execute`）之前插一个新阶段：

```text
Call ──prepare──▶ Confirm{arguments, plan, replay, asked_at, deadline} ──批准──▶ Execute ─▶ …
                         │
                         ├─拒绝 / 到点 / 取消──▶ settle_unrun（没执行，结果交给模型）
```

- 只有 `!attended && tool.outward()` 才进 `Confirm`；其余照旧直达 `Execute`，有人值守的 Run 行为一字不变。
- `Confirm` 是持久 checkpoint：计划已定、确认没执行。重启后任务回到 `pending`，从 `Confirm` 接着等，不会
  因为重启而执行或重复发卡（卡的投递键已幂等，§11.4）。
- 等的办法：任务每拍读会话文档 `komo.confirm`（`{handle → verdict}`）里有没有这张卡的答复，没有就
  `sleep_until(min(下一拍, deadline))`，同 `Probe` 的写法。答复路由只写这份文档，不碰任务。
- 结果交给模型的文字：
  - 拒绝：`操作者拒绝了这次 <工具> 调用，没有执行。`
  - 到点：`等了 <N> 分钟没有确认，没有执行。`
  - 批准：照常执行，结果里不另加话。
- 等待期间 Run 一直开着：同一轮其余调用照常；会话后来的输入照现有规则排队或插话。取消 Run 走现有 abort
  路径，卡随之作废。

## 6. 卡与回答（D6）

- kernel 加 `InterventionKind::Confirm`，裁决 `approve` / `reject`（`InterventionVerdict` 加两个值；
  这个枚举标着"冻结"，这次是有意的扩展，老客户端读到不认识的值按未知处理）。
- `komo intervention list` 同时列故障（`blocked`）与待确认（`confirm`）两类；handle 用工具任务 id。
- 卡的正文（文字）：

  ```text
  ⚠️ 需要你确认一个对外动作
  来源：Cron 「每天 9 点看工单」 / monitor #6847 / 后台任务 …
  事项：<挂着的目标或领域标题>（有就写）
  调用：mcp__devops-mcp-prod__issue_transit
  参数：<JSON，截到 1 KiB>
  24 小时内没回复就不执行。
  回复 /answer <handle> approve 执行，/answer <handle> reject 不执行。
  ```

- 回答入口：聊天 `/answer`（只认操作者）、`komo intervention answer <handle> approve|reject`、
  `POST /v1/interventions/{handle}/answer`。都写 `komo.confirm`，并追加一条 `komo.confirm` entry 记下
  裁决、谁答的（渠道 peer 或 `cli`）、什么时候——事后能查。重复回答返回 `already_answered`。
- 投递复用 `announce` / `attention_peer` / `intervention_key`。**`home_chat` 没配时**，Cron 的卡没有收件人：
  卡照样在 `komo intervention list` 里等到点，`komo doctor` 与 `config check` 在"有对外声明、却没有
  home chat"时报警。现在本机飞书 `home_chat` 因 alert-duty 暂停被注释掉，落地前要操作者决定是否恢复。

## 7. codemode（D5）

codemode 脚本里的嵌套调用经宿主执行。宿主判到"无人值守且这个工具对外"时直接返回错误：

```text
<tool> 是对外动作，这次 Run 没有人在看，需要确认；脚本里等不了确认。请在顶层直接调用 <tool>。
```

顶层调用本来就在能力面里（`docs/mcp.md` §3「隐藏 schema 不等于撤权」），模型可以先在脚本里
`describe_tool` 拿参数形状，再到顶层调。有人值守的 Run 里嵌套调用照旧。

## 8. 完成标准

- 一个 Cron Job 的 Run 调一个声明为对外的 MCP 工具：没执行，home chat 收到卡；`/answer … approve` 后执行，
  结果回到 Run；`reject` 后模型拿到"没执行"。
- 同一个工具在 TUI / 飞书里用户发起的 Run 里调：不发卡，直接执行。
- 等确认期间重启 Gateway：不执行、不重发卡，答复后照常执行。
- 到点没答：不执行，模型拿到超时说明。
- codemode 脚本里在无人值守 Run 中调对外工具：被拒，模型改在顶层调，走卡。
- 没声明对外的工具（包括 bash）在无人值守 Run 里照旧执行——这是已知边界，写在 §1。

## 9. 分片

依赖是直线，按顺序做；P3c 与 P3b 可以并行。

| 片 | 内容 | 主要位置 |
|---|---|---|
| P3a | `attended` 判定与 `ToolEnv.source`；`Tool::outward`、MCP `outward` 配置；`Checkpoint::Confirm` 与等待、超时、拒绝 | kernel `traits` / `types::tool` / `protocol::config`；runtime `durable/harness/tool.rs`、`tools/mcp.rs`、config |
| P3b | `InterventionKind::Confirm`、列表、回答写 `komo.confirm` 与 entry、投递与卡正文、`/answer` 与 CLI | kernel `protocol::http`；gateway `service/durable/interventions.rs`、`dispatcher.rs`、render；client；bin |
| P3c | codemode 嵌套调用的拒绝；`doctor` / `config check` 的 home chat 报警；文档（§7、§7.5、§11.4、`mcp.md`） | runtime codemode 宿主；gateway config；docs |
