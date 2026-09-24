# Komo Memory：对照 v0.8 的差距与改进

> Status: Analysis —— 不是规格。Memory 的规格是 `docs/komo_bot.md` §9，本文与它冲突时以 §9 为准；
> 下面任何一条要落地，先改 §9。
> Source: Obsidian `01-inbox/认知心理学与 agent memory.md` 的六条启发。
> 核对基线：`main` @ `d0b6708`（2026-09-24）。

---

## 1. 结论

认知心理学笔记对 Agent Memory 的要求，v0.8 已经把**防错的那一半**做进了结构：来源与确认
分开、使用次数不增加真实性、冲突停召回、取代留链不改写、遗忘立即生效且检查点带不回。
这部分不需要动。

缺的是**让记忆生效的那一半**，按优先级：

| # | 级别 | 问题 | 性质 |
|---|---|---|---|
| 1 | P0 | `confirm` 不改 `state`：确认过的 candidate / contested 仍不召回 | 缺陷，已核实 |
| 2 | P0 | candidate 没有按独立证据晋升的规则 | 规格缺口 |
| 3 | P1 | shell 子进程不带 `KOMO_HOME`：非默认实例里 `komo memory search` 连到 `~/.komo` | 缺陷，已核实，不止影响 Memory |
| 4 | P1 | contested 只有显式 search 看得到，没有主动触达 | 规格缺口 |
| 5 | P2 | 久未佐证的 active 注入时没有提示 | 改进 |

## 2. 启发 → v0.8 对照

| 启发 | v0.8 现状 | 位置 | 判断 |
|---|---|---|---|
| 记忆是重建，可多轮召回 | 自动召回一段对话一次（为前缀缓存）；主 Agent 的系统提示让它用 shell 跑 `komo <子命令>` 查 komo 状态，`komo memory search` 支持 hybrid | §9.4；`komo-agent/src/context/prompt.rs` | 路已有，被 #3 破坏 |
| 经历按证据沉淀成认识 | 逐字相同或判为 `Same` 只补证据，不动 `state` / `confirmation`；独立证据只认用户 / 工具事件；失败、取消、未知的 Run 不能成为 experience | §9.3、§9.6；`consolidate.rs` `merge`；`extract.rs:283` | 防自证做到；**无晋升**（#2） |
| 遗忘是功能 | contested / superseded / forgotten；forget 立即停用，resume 与旧检查点不能带回；`valid_until` | §9.6、§9.7；验收 `a_forgotten_memory_never_comes_back_into_a_turn` | 做到 |
| 冲突与取代 | 模型判 same / supports / contradicts / supersedes / unrelated，`Plan::authorize` 决定能做什么：推断取代用户陈述、任何东西取代用户已确认内容 → 降级为 contested；取代留 `supersedes` 链 | `consolidate.rs` | 做到；contested 无对端链（见 #1） |
| 召回按决策价值 | 关键词 ∪ 向量 → RRF → 可选判断层重排（只排序不增删，默认关） | §9.4；`rerank.rs` | 够用 |
| 做事方式比事实重要 | `kind = experience`；Skills 由人写，无候选流程 | §3、§9.2 | 载体够，靠 #2 让 experience 生效 |
| 记忆知道自己多可信 | provenance / confirmation / state 三字段；注入行带种类 · 来源 · 确认状态 · 观察日期 · `id@revision` | §9.2；`komo-agent/src/context/memory.rs` `render_line` | 做到 |

## 3. 问题与改法

### 3.1 P0 · confirm 不改 state

**现象。** `komo-store/src/repos/memory/mod.rs` 的 `confirm` 只写 `confirmation = user_confirmed`
与 `updated_at`。自动召回走 `RecallQuery::admits` → `MemoryItem::is_recallable_at` →
`MemoryState::is_recallable`，只放行 `Active`。所以：

- 确认一条 candidate：`confirmation` 变了，`state` 仍是 candidate，**永远不进上下文**；
- 确认 contested 的一方：它仍是 contested；§9.6 要求的"旧版本随后标为 superseded"不会发生。

现有测试（`confirming_needs_the_expected_revision`、
`confirm_and_forget_carry_the_expected_revision_and_repeat_harmlessly`）都用 active 条目确认，
没暴露。

**改法。** confirm 同时把 candidate / contested 置为 active。contested 的对端要标 superseded，
但 `MemoryItem` 只有 `supersedes: Option<SupersededRef>`，**contested 双方没有记下彼此**
（`consolidate.rs` 里的 `contested_with` 只在函数内用）。两个选项：

- 冲突时把对端 id 写进条目（加字段，schema 追加）；
- confirm 时对同作用域的 contested 条目重新判关系。

建议前者：确定、可测，不在用户操作路径上调记忆模型。

**验证。** 确认 candidate 后 `is_recallable_at` 为真、下一段对话能召回；确认 contested 一方后
它 active、对端 superseded；确认 active 行为不变；重复确认幂等。

### 3.2 P0 · candidate 按独立证据晋升

**现象。** `extract.rs:339`：用户陈述、工具观察直接 active，模型推断进 candidate。后续 Run 里
的同一主张经 `merge` 挂证据，但证据数不参与任何判断。"用户三次在不同任务里删掉背景段落"这类
**只能靠推断得出**的认识，除了用户手动 confirm（且先修 3.1）外永远不生效。

**改法。** candidate 同时满足下列条件时置为 active，**`confirmation` 仍 unconfirmed，
`provenance` 仍 model_inference**：

- 证据来自 ≥ N 个不同 Run（同一 Run 多条事件只算一次）；
- 证据都是用户或工具事件（沿用 §9.3 的独立证据定义）；
- 最近一条证据在 T 天内；
- 不是 contested。

N、T 进 `[memory]` 配置，初值 3 次 / 90 天。判断放在 `merge` 之后——那是证据数唯一会变的地方。
注入文案从"候选，未确认"变为"多次独立佐证，未确认"一类，读的人知道它不是用户原话。

**与 §9 的关系。** §9.6「使用次数和多次模型复述不能增加确认等级」不受影响：改的是 state，
不是确认等级；独立用户 / 工具事件也不是模型复述。§9.2「模型推断默认 candidate，不作为已确认
事实注入」要补一句晋升规则。

**验证。** 2 个 Run 仍 candidate；第 3 个不同 Run 后 active 且 confirmation 不变；同一 Run 内
3 次不晋升；证据全部早于 T 不晋升；contested 不晋升。

### 3.3 P1 · shell 子进程不带 KOMO_HOME

**现象。** `komo-runtime/src/tools/shell.rs` 的 `INHERITED` 是
`PATH, HOME, LANG, LC_ALL, TZ, USER, TMPDIR`，`process.rs` 先 `env_clear()`。子进程里的
`komo` 按 `komo-client/src/discovery.rs` `komo_home()` 找数据目录：没有 `KOMO_HOME` 就用
`$HOME/.komo`。默认实例没问题；**非默认数据目录的实例里，模型跑 `komo memory search`
（以及 `komo cron list` 等所有自查命令）连的是 `~/.komo` 的 Gateway**，读到另一个实例的
记忆。这破坏了 2 节第一行"按需再查一次"的路，也让不同数据目录的实例互相串了状态。

**改法。** shell（以及 python，若也要自查）给子进程显式注入 Gateway 自己的数据目录作为
`KOMO_HOME`——注入实际值，不是从 Gateway 环境继承，这样前台跑的 `KOMO_HOME=… komo gateway
--foreground` 也对。`KOMO_HOME` 是路径不是凭证，不违反 §5.3。

**验证。** 以临时 `KOMO_HOME` 起 Gateway，shell 里跑 `komo gateway status`，印出的数据目录是
该临时目录；子进程环境单测断言 `KOMO_HOME` 存在且等于 Gateway 数据目录。

### 3.4 P1 · contested 没有主动触达

**现象。** contested 停召回、等用户裁，这是对的；但只有 `komo memory list/search` 显式列状态时
看得到（`komo-gateway/src/http/memories.rs`），Notifier 不知道它。用户不去查，冲突就永远不裁，
两边都不进上下文。

**改法。** 经 Notifier 已有的 home 渠道按天汇总一次新增 contested（条数 + 双方正文 + confirm /
forget 命令），不新增审批流。依赖 3.1 的对端链来成对展示。

**验证。** 造一条冲突，下一次汇总里出现且只出现一次；裁定后不再出现。

### 3.5 P2 · 久未佐证提示

不做衰减分数、不自动退役（多数 active 是用户原话或工具结果，自动退役会静默丢掉用户说过的话）。
preference 的最近一条证据早于阈值时，`render_line` 附"较久未佐证"，由模型在会导致实际动作时
先问。只改渲染，段内逐字复用（§9.4）不受影响——阈值按段开始时刻算。

## 4. 明确不做

| 不做 | 理由 |
|---|---|
| working / episodic / semantic / procedural 四分类 | Working Memory 是 Session / Run（§9.1），episode 原文就是 JSONL；kind 三值够用，再分只增加提取模型的出错面 |
| 浮点 confidence / 加权打分公式 | 无可测输入；离散的 provenance / confirmation / state + 证据数可测、可解释 |
| 按使用或召回次数加分、晋升 | 召回器选中 ≠ 为真；§9.2 规定 usage 只度量使用 |
| 每轮重召回、迭代召回进主循环 | 破坏前缀缓存（§9.4），收益未测；按需再查走 shell（先修 3.3） |
| 自动把 experience 生成 Skill | Skills 由人写（§3）；3.2 落地后稳定的 experience 自然生效 |

## 5. 落地顺序

1. 3.1 + 3.3：两个缺陷，互不依赖，各一个 fix 分支。
2. 3.2：先改 §9.2 / §9.6，再实现。
3. 3.4：依赖 3.1 的对端链。
4. 3.5：有真实库数据后再定阈值。
