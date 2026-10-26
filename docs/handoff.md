# Handoff：由模型发起的"交接后换上下文"

> 状态：**提议，未实施**（2026-10-26）。来源：pi-durable 的工具控制 `control.handoff`（本机 `../pi`
> 提交 `d5b3de22d`：spec §7.3、§8.1、§8.5，README "Reset and Handoff"；`38ff99177` 修正为"run 继续"），
> 以及 pi coding-agent 的 `/handoff` 扩展示例（`packages/coding-agent/examples/extensions/handoff.ts`）。
> 文中 §x 不带文件名时指 `komo_bot.md`。

## 0. 结论

给 komo 加一个**由模型自己判断**的上下文切换：模型调用 `handoff` 工具交出一段自足的交接说明，这一轮工具收尾时
写一条带这段说明的 `komo.reset` entry（`head: "self"`，与 `/new` 同一种 entry），**同一个 run 接着跑**——下一次
请求只看到交接说明与之后的内容，用户这次输入拿到的是新上下文里给出的回答。

它补的是压缩和 `/new` 之间的空档：

| | 谁决定 | 留下什么 | run |
|---|---|---|---|
| 在线压缩（§6） | 策略（贴着窗口） | 摘要 + 最近 `keep_recent_tokens` 的原文 | 继续 |
| `/new` | 用户 | 什么都不留 | 正在跑的以 `reset` 收场 |
| **handoff** | **模型** | **只有模型写的交接说明** | **继续** |

适合的场景：一个长 run 里前半段在排查、后半段换成实现，排查的过程（大段工具输出、走过的弯路）对后面只是噪音；
长期跑的任务会话（`task`）与监听会话阶段性地"清一次场"；用户明确说"带着结论换个干净的上下文继续"。

默认**不开**：它是有损的，只留模型自己写下的东西。由 Agent 的能力面选进来（§2.4）。

## 1. pi 的做法

- `Conversation.reset(handoff?)`：一条 write submission，`pi.reset` entry、`head: "self"`，给了文本就带一条
  `[UserMessage]`。忙时按收件箱规则排队；在工具轮里放置时当前 run 以 `unanswered{reset}` 收场。
- 工具结果可以带 `control: { addTools?, terminate?, handoff? }`（spec §7.3）。一轮里任何结果带了 `handoff`
  （多个时取调用顺序最后一个），生成任务的 `tools` 阶段：
  1. 先追加与 `reset(handoff)` 完全相同的 `pi.reset` entry，并把这次 boundary 的活动范围起点挪到它；
  2. 排队的 write 与选中的 steer 放在交接文本**之后**，followUp 等 run 的 final boundary；
  3. 队列里有 reset 时它赢：run 以 `unanswered{reset}` 收场；
  4. 否则建下一个 generation，**在新上下文里**准备与请求；run 的输入以最终的回答（或错误）结算；
  5. 同一轮所有结果都 `terminate` 时，写完 reset 后结束 run。
- coding-agent 的 `/handoff <目标>` 扩展是另一条路：单独发一次摘要请求，按"目标"生成交接提示，放进编辑器给人审，
  人再发到新会话。

## 2. komo 的做法

### 2.1 工具结果的控制字段

- `komo_kernel::types::tool::ToolOutput` 加一个可选字段 `control: Option<ToolControl>`，首版只有
  `handoff: Option<String>`（不搬 `addTools` / `terminate`，komo 没有对应需求）。serde 默认 `None`、`None` 时不序列化，
  旧 `output.json` 照读。
- 它随 `output.json` 落盘，结果 entry 收尾时抄进 `komo.tool-result` 的 `data.control`。恢复时"复用完整
  `output.json`"那一行（`docs/storage-durable.md` §5）原样带回 `control`，所以"结果已落盘、轮次没收尾就崩"不会丢掉交接。
- 只有顶层调用的 `control` 生效。codemode 嵌套调用（`kind: "nested"`）里的结果不进 transcript，`control` 忽略；
  `handoff` 本身也列进 `NOT_IN_SCRIPTS`。

### 2.2 `handoff` 工具

- 参数：`{ note: string }`——交接说明正文，必须自足（下一次请求只看得到它）。工具描述里给出 pi 扩展同款的结构建议：
  已做的决定与结论、涉及的文件与引用（`artifact://` 仍可 `read`）、下一步要做什么。
- `prepare`：去掉首尾空白后非空；上限 16 KiB（超了是参数错误，让模型精简）。
- `execute`：不做任何副作用，返回 `ToolOutput { status: completed, result: { handedOff: true }, control: { handoff: note } }`。
  `Tool::replay` 是 `Safe`（纯函数）。
- 模型视图的结果正文写一句"已交接：下一次请求从交接说明开始"。这条结果 entry 落在 reset 之前，新上下文里看不到它，
  只在 `/tree` 与导出里可见。

### 2.3 生成任务的 `tools` 收尾

改 `finish_round`（`crates/komo-runtime/src/durable/harness/generation.rs:955`），在**同一次提交**里：

1. 读这一轮的结果 entry（`Checkpoint::Tools` 已知道这一轮的调用），取调用顺序最后一个带 `data.control.handoff` 的；
2. 有就先追加 `komo.reset` entry：`head: "self"`、`model: [UserMessage(note)]`、`data: { handoff: { task: <工具任务>, call: <call id> } }`，
   与 `Harness::reset(Some(..))`（`harness/submit.rs:163`、`harness/inbox.rs:155`）写的 entry 只差 `data`；
3. 再 `apply_boundary(At::PostTools)`：排队的 write、选中的 steer 落在 reset 之后（ID 序天然如此）；
4. `result.reset`（队列里的 `/new`）照旧赢：run 以 `unanswered{reset}` 收场；
5. 否则照旧建下一个 generation、把 run 交给它。它的 `prepare` 从新的 head 推上下文：系统段在新 head 后重写一条完整的
   `komo.system`（`plan_system_entries` 的"head 之后还没有 system entry"分支），钉住的 `project` / `skills` / `memory`
   段在这里重算——与 `/new` 之后完全一样。

整步是一次提交，崩溃只有"没收尾"与"已收尾"两种状态；没收尾时重跑 `finish_round` 读到同样的结果，决定相同。

### 2.4 能力面与限额

- `handoff` 是 komo-agent 的一个工具名，Agent profile 显式选进来才出现在工具表里；默认 profile 不带。
- fresh 的任务会话（`task` 派出、`context: "fresh"`）可以选；fork 的任务沿用父的能力面（D5 W1b 的规则），父有它就有。
- **每个 run 最多交接 `max_handoffs_per_run` 次**（默认 2，`[agents.<id>]` 下的键）。超过的那次 `prepare` 拒绝，结果说明
  "这一轮已交接过 N 次"。计数记在 `komo.live.run`（与轮数、token 预算同处）。防的是模型反复交接、越交越短的循环。
- 轮数与 token 预算跨交接累计（同一个 run）。

### 2.5 与其他机制的关系

| 机制 | 交接之后 |
|---|---|
| 在线压缩 | `online_state` 遇到 `head` 指自己的 entry 归零（`komo-kernel/src/durable/compaction.rs:440`），与 `/new` 相同。后台压缩若正在跑，它的摘要以交接之前的 entry 为切点，放置时要判为过期（pi："a queued head write targeting an entry before it is stale"）——**要核对 komo 的压缩放置是否已经这样判**，没有就补 |
| 衰减 | 交接之前的结果都不在上下文里了，衰减的 `edits` 自然失效 |
| 提示缓存 | 前缀整段换掉，这一次请求冷启动。交接本来就是为了换上下文，代价可以接受；`/tree` 与 `run inspect` 里能看到 |
| Memory | 交接说明是**模型写的** user 角色消息，提取时不能当 `user_statement`（同 `task.result` 的规则，`docs/memory.md` §9.10）。靠 `komo.reset` 的 `data.handoff` 认 |
| 编排 | 子会话里的交接只影响子会话；`task.result` 在子 run 最终回答时交回，不受影响 |
| 树与导出 | `pi_tree` 已把 `head: self` 投成自引用的 `Compaction`；带 `model` 时摘要就是交接说明，**要补一条导出测试** |
| SSE / TUI / 渠道 | 投影出一个"已交接"分隔帧（字数 + 展开看全文），聊天渠道不单独推送 |
| 遥测 | 生成任务的 `tools` 阶段 span 上发 `komo.handoff` 事件（只有字数，不带正文），计数器 `komo.handoffs`（`docs/observability.md` §3） |

### 2.6 用户发起的 `/handoff`（第二步）

两种做法，建议先做第一种：

1. **让模型调工具**：`/handoff <目标>` 提交一条普通输入，正文是固定模板（"把到目前为止的工作整理成交接说明，调用
   `handoff` 工具；交接之后接着做：<目标>"）。不多一次模型请求，交接说明由手里有完整上下文的主模型写；要求当前 Agent
   的能力面里有 `handoff`。
2. **pi 扩展的做法**：单独一次摘要请求（按目标生成交接提示），结果作为 `Harness::reset(Some(note))` 的 write 提交，
   之后用户再发目标。多一次请求，好处是可以让人先审交接说明（TUI 放进输入框）。

## 3. 不做

- `addTools` / `terminate`：komo 的工具表由能力面决定、没有"按需加载"的工具；结束 run 只有回答一条路。有需求再说。
- 交接时自动开新 Session：pi 扩展是"新会话"，komo 用同一个 conversation 里的 `head`，历史仍在、`/tree` 能回看、
  记忆照常积累，不需要新 Session。

## 4. 落地步骤

| 步 | 内容 | 验收 |
|---|---|---|
| 1 | `ToolOutput.control`、结果 entry 的 `data.control`、复用 `output.json` 时带回 | 旧 `output.json` 解码不变；崩溃在"结果已发布、轮次未收尾"后重开，交接照样生效 |
| 2 | `finish_round` 写 reset、boundary 次序、`max_handoffs_per_run` | 移植 pi `harness-inbox.test.ts` 里 handoff 的几条：run 继续并以最终回答结算；write / steer 落在交接文本之后；followUp 等 final；排队的 `/new` 赢 |
| 3 | `handoff` 工具与能力面、komo-agent 的工具名 | 默认 profile 没有它；选进来后模型请求里有它；codemode 里调用被拒 |
| 4 | Memory 排除、`pi_tree` 导出、SSE 分隔帧、遥测事件 | 交接说明不进 `user_statement`；导出被 kernel `SessionTree` 读回，leaf 处上下文以交接说明开头 |
| 5 | `/handoff <目标>`（§2.6 第一种） | 渠道与 TUI 都能用；能力面没有 `handoff` 时给出明确提示 |
