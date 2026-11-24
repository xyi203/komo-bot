# Komo 需求与架构综合优化方案（1–6）

> 状态：**未批准草案**（2026-11-23）。
> 本文针对 `komo-bot` 在完成存储重写（pi-durable）与产品重设计（[`redesign.md`](redesign.md)）后暴露出的 6 类系统性优化点，给出可落地的设计方案、涉及模块与分波次实施计划。
> 凡经操作者拍板的项，落地时按仓内规范写回对应权威文档：记忆相关写回 [`memory.md`](memory.md)（不另立独立记忆规格），目标写回 [`goals.md`](goals.md)，工具/运行时写回 [`komo_bot.md`](komo_bot.md) / [`codemode.md`](codemode.md) / [`confirm.md`](confirm.md)，客户端与部署写回 [`web.md`](web.md) / [`clients.md`](clients.md) / [`deploy.md`](deploy.md)。

### 落地进度

- **2026-11-23**：§2.2 第 1、2 条的后端与 CLI（`POST /v1/tasks/{id}/stop` + `komo task stop`、`POST /v1/monitors/{handle}/stop` + `komo monitor stop`；操作者停的监听交回结束消息，模型自己停的照旧不交回）；§2.3 第 3 条的第一步（harness 经 `Runtime::report` 报的非致命失败——部分回复 / 工具进度提交失败——先记 `warn` 再交 `EngineOptions.report`，不再静默）；§5.4 / §6.1 的握手放宽（只比协议号与 monty，`komo` 版本号不同记 `warn` 照常跑）。
- 未做：上述两条的网页「停止」按钮、错误指标与核心 span、`/healthz` 增补、`komo update --restart`。

## 0. 源码核实基线与待拍板决策

撰写本方案前已逐项核对当前源码，先澄清**已在代码中落地、但旧文档仍记为缺口**的四件事，避免重复施工：

1. **`candidate` 的 `confirm` 转 `active` 已实现**：`crates/komo-store/src/repos/memory/mod.rs`（`TursoMemoryRepo::confirm`）已将 `Candidate` 同时置为 `state = "active"`；**未完成的是 `Contested` 的对端裁决与网页/编译联调**。
2. **`LineReply.intervention` 已透传**：`crates/komo-kernel/src/events.rs`、`legacy_events.rs` 与前端 `apps/client/src/features/conversation/reply.tsx` 已按 `handle` 匹配渲染按钮；**真正未解的是同对话 `Confirm`/崩溃阻塞导致 `say_in` 被挡在 boundary 外的死锁**。
3. **`GET /v1/matters` 已带 `waiting` 映射**：`MatterListResponse.waiting`（`DurableCore::waiting_by_goal`）已在服务端实现；需完成前端对齐与联调。
4. **TS 协议生成已基本收口**：`packages/protocol/src/generated/` 已有 173 个 `ts-rs` 生成文件，当前手写残留仅剩 `push.ts`、`version.ts`、`index.ts`；SSE 内存环形窗口 `RecentEvents`（512 KiB）也已存在，仅冷补读仍走全量投影。

### 待拍板决策（O1–O8）

| # | 问题 | 推荐方案 | 理由 |
|---|---|---|---|
| O1 | 内置记忆与外部提供方（Hindsight）的长期分工 | **已拍板（2026-11-23，方案 A）**：`primary` 模式下内置只提取**长期约定 + 目标归属线索**（及手动记忆/采纳计数），停止重复提取一般线索；Hindsight 专注一般历史线索召回，同步修补助手回复污染、`push_unattached` 与作用域标签 | 保留 cross-encoder 召回优势（`memory.md` 附录 G.7）与目标结案归档能力，同时消除双写、记忆页两张皮与自证污染 |
| O2 | 目标（追踪中）与纯后台定时的关系 | **已拍板（2026-11-23）**：目标加 `inject_context: bool`（默认 `true`，纯命令/看门狗定时默认 `false`），并将定时扩为 1:N 列表 | 不新增顶层概念（保住「对话、目标、记忆」三栏），同时彻底解除后台杂活对 600 token 注入预算的挤占 |
| O3 | 同一对话内触发器 Run 停在 `Confirm` 如何破死锁 | **双通道**：① 活跃阻塞介入直接由前端侧通道（`GET /v1/interventions` + 全局 SSE）在对话顶部/输入区上方置顶渲染确定性按钮条；② 对话里的文字回复仅作补充，不再要求等待 `say_in` 落盘才能操作 | 零改动 durable 串行不变量，从根上消除 `postTools` boundary 死锁，且比 LLM 转译更安全 |
| O4 | 部署形态：远程部署与本地运行的统一 | **已拍板（2026-11-23）**：**同时支持远程部署与本地运行**，两者统一采用「单二进制 Gateway 同源托管静态 Web 壳」作为标准形态；退掉 CF Pages + 路径分流方案 | 既兼顾本机开箱与远程常驻，又彻底消除前后端分离带来的版本错位、跨域与 SSE 代理断流问题（回到 `clients.md` D5） |
| O5 | 本机免配与远程认证路径 | **已拍板（2026-11-23）**：**本机运行**走 `local-link` / 本机令牌（零 OIDC 配置）；**远程部署**以 OIDC 为首选，补轻量单人远程认证退路（Cloudflare Access JWT 头或单人主口令） | 同时满足远程安全边界与本地/自托管新人「不受阻」原则 |
| O6 | `codemode`（Monty）的定位取舍 | **已拍板（2026-11-23，方案 2）**：① 借鉴 `pi` 补上纯 Rust `tool_search` + `deferred` 将 MCP 从 `codemode` 解绑；② 近期保留 Monty，去掉 `monty-fs` 裸文件挂载（读文件统一走 `tools.read`）并放宽 worker 握手协议；远期按需评估切换 QuickJS（`rquickjs`） | 兼顾日均 ~36 次脚本编排刚需与近期改动成本，同时立刻消除 MCP 强绑定、`komo update` 瘫痪窗口与 `monty-fs` 重放分歧 |
| O7 | `aux.db` 是否并入 `durable.db` | **中期执行（Wave 3）**：迁完后删 `toasty` 与 MVCC DDL 绕行补丁 | 消除双库双并发模型、释放编译关键路径、让目标活动时间线与引用检查走同库索引 |
| O8 | 施工次序：先封板还是先开新功能 | **先执行 Wave 0 封板**（把已写未编译/未实测的 3b/4b/4c/5a–5e、Push、Telegram、Visual 分支统一编通验过），再按 Wave 1 → 3 推进 | 止住未验证代码持续叠加造成的回归黑洞 |

---

## 1. 记忆闭环：让记忆真正生效、进化与收口

> 目标：兑现 `PRODUCT.md`「长期记忆越用越懂我」，把当前「防错很严、生效与进化断链」补成完整闭环。批准后核心规格写回 [`docs/memory.md`](memory.md)。

### 1.1 `contested` 冲突对端持久化、裁决联动与触达

- **现状问题**：`MemoryItem` / `memory_items` 无冲突对端字段，`TursoMemoryRepo::confirm` 对 `Contested` 仅改 `confirmation` 不改 `state`（`crates/komo-store/src/repos/memory/mod.rs:214-221`）；前端红点 `usePendingMemories` 只查 `state=candidate`，冲突双方双双静默退出召回。
- **方案**：
  1. **数据模型（追加列）**：
     - `komo-kernel/src/types/memory.rs` 的 `MemoryItem` 新增 `pub contested_with: Option<MemoryId>`（`#[serde(default, skip_serializing_if = "Option::is_none")]`）。
     - `aux.db` 的 `memory_items`（`crates/komo-store/src/models/memory_item.rs`）追加可空列 `contested_with TEXT`，沿用建池前非 MVCC 迁移补列。
     - `consolidate` / `extract` 产生冲突将双方标为 `Contested` 时，双向写入彼此的 `contested_with`（新条目指向旧条目，旧条目指向新条目；若已有链则保持指向最新冲突对端）。
  2. **原子裁决（`confirm` / `forget`）**：
     - `TursoMemoryRepo::confirm` 在同一 `with_write_retry` 事务内：
       - 若目标为 `Candidate`：置 `state = Active`、`confirmation = UserConfirmed`（保持现状）。
       - 若目标为 `Contested`：置目标 `state = Active`、`confirmation = UserConfirmed`、`contested_with = None`；若存在 `peer_id = contested_with` 且对端仍为 `Contested`，同时将对端置为 `state = Superseded`、`superseded_by = Some(target.id)`、`contested_with = None`，并同步更新 `supersedes` 引用。
     - 若用户对 `Contested` 的一方调 `forget`：除将自身置 `Forgotten` 外，若对端仍为 `Contested` 且满足激活条件（原为 `Active` 或已有用户确认），自动恢复对端为 `Active` 并清空 `contested_with`——避免“忘了一条、另一条永远卡死在 contested”。
  3. **触达与界面**：
     - `GET /v1/memories` 支持多状态筛选（如 `state=candidate,contested`），或新增 `pending_review=true` 参数返回「未确认的 `candidate` + 全部 `contested`」。
     - 前端 `usePendingMemories`（`packages/api/src/memory.ts:110`）改为查询 `pending_review=true`：图标栏红点同时覆盖候选与冲突；记忆页「等你过目」区块分两小节，冲突条目**成对并排展示**（提供「保留这条 / 保留旧的 / 两条都不要」一键动作），并移除前端把「已确认候选」塞进「暂停中」的临时绕行。

### 1.2 `candidate` 按独立证据自动晋升

- **现状问题**：模型推断写入 `Candidate` 后，测试钉死永不晋升（`memory.md` 附录 C.3.2），隐式习惯/经验不人工点击永远不生效。
- **方案**（落地 `memory.md` C.3.2 规格）：
  - 在 `[memory]` 新增可热重载配置：
    - `auto_promote_runs: u32`（默认 `3`，设 `0` 可关闭自动晋升）
    - `auto_promote_window_days: u32`（默认 `90`）
  - 在 `merge` 追加新证据后执行纯函数判定 `should_promote(item, now, policy)`，同时满足以下条件时将 `state` 升为 `Active`（**保持 `provenance = ModelInference` 与 `confirmation = Unconfirmed` 不变**）：
    1. `item.state == MemoryState::Candidate`（绝不晋升 `Contested`）；
    2. `item.evidence` 中满足「来自用户原话或工具结果」的独立证据覆盖 **≥ `auto_promote_runs` 个不同的 `run_id`**（同一 Run 内多条事件去重只计 1 次）；
    3. 最新一条有效证据距 `now` ≤ `auto_promote_window_days` 天。
  - 注入文案区分：`ModelInference + Unconfirmed + Active` 标注为 `多次独立佐证 · 未确认`，让模型和用户都能识别其可信度低于用户原话。

### 1.3 让 `adopted_count` 闭环反哺检索降权与 `sweep` 清理

- **现状问题**：`record_adoption` 已落库 `adopted_count` / `last_adopted_at`，但 `sweep::judge`（`crates/komo-memory/src/sweep.rs:50-109`）只看曝光时间 `last_used_at`，高频注入却零采纳的噪音条既不降权也永不被清。
- **方案**：
  1. **轻量检索降权（确定性、非黑盒）**：在 RRF 融合后、截断 `top_k` 前，仅对**非 `UserConfirmed` 且 `application == Reference`（线索）**的条目计算低采纳阻尼系数：
     - 当 `usage.count >= 8` 且 `adopted_count == 0` 时，分数乘 `0.6`；
     - 当 `usage.count >= 15` 且 `adopted_count == 0` 时，分数乘 `0.35`（并在记忆页标记「多次带出但未用上」，供每周回顾处理）。
     - `UserConfirmed` 与 `Convention` 不受降权影响。
  2. **`sweep` 补齐两项规则**：
     - **零采纳老化**：对非 `UserConfirmed`、非 `Convention` 的线索，若 `usage.count >= 12 && adopted_count == 0` 且创建超 `idle`（默认 90 天），`sweep` 给出新理由 `NeverAdopted` → 转 `Archived`（不直接 `Forgotten`，留可找回退路）。
     - **降级保护**：召回诊断处于向量降级期间（`vector_degraded`），跳过 `NeverUsed` 清理；路径存在性校验仅检查正文中显式出现的绝对路径，并对含否定语境（「不存在 / 已删除 / removed / deleted」）的条目跳过 `MissingPath` 判定，消除误报。
  3. **触发入口**：为 `consolidate` 与 `sweep` 在网页记忆页「更多」菜单提供「预演整理 / 执行整理」入口（对应现有 `POST /v1/memory-consolidate` 与 `POST /v1/memory-sweep`），并在 §4.3 每周回顾中提供一键触发。

### 1.4 消除开箱「0 门槛」噪声与补齐网页管理闭环

- **现状问题**：`min_similarity` / `min_keyword_score` / `min_keyword_cosine` 默认全为 `0`，开箱约 73% 无关注入；而设置页把检索参数拿掉了。
- **方案**：
  1. **开箱可用默认门槛**：
     - 保留显式配置优先；未配置时按后端协议提供保守内置默认（例如 `min_keyword_score = 0.25`，常见 cosine 向量模型如 `text-embedding-3-*` / `bge-*` / `nomic-embed-*` / `qwen3-embedding` 提供 `min_similarity` 推荐预设表，未知模型默认 `0.35` 并允许显式写 `0` 关闭）。
     - 新增诊断接口 `POST /v1/memory-calibration`（输入 3–5 条代表性查询或从最近会话采样），返回正负样本的两臂分数分布与推荐门槛，供设置页「记忆」高级折叠区一键应用。
  2. **记忆详情页补齐写闭环**：
     - 详情页支持修改归属（`belongs_to`，对接已有 `classify` 接口）、修改使用方式（约定/线索）与触发条件（`applies_when`）；
     - ⌘K 全局搜索（`features/palette/`）加入第三组「记忆」（调用 `GET /v1/memories/search?limit=5`，解决 `redesign.md` §1.3 L65 待定项）；
     - 当生效约定总 token 超出 `convention_tokens`（默认 800）时，在记忆页顶部和 `komo doctor` 显示明确告警条（列出被截断的约定数量），不再静默截断。

### 1.5 修复召回与提取的三个盲区

1. **目标定时 Run 开放目标作用域记忆注入**：
   - 修改 `crates/komo-gateway/src/service/durable/hooks/recall.rs:109-114`：对于 `cron.input`，若会话来源为 `cron:<goal_id>`，不再整体跳过召回，而是执行**定向注入**：仅注入全局通用约定 + `belongs_to == Some(goal_id)` 的约定与线索（以及该目标自身说明与下一步），不拉取无关目标与无关闲聊线索，既避免噪音又让定时执行拿得到为目标积累的知识。
2. **多轮短句/指代查询增强**：
   - `hooks/recall.rs` 构造召回查询文本时，若末条用户消息字符数 < 20（或命中典型指代模式如「继续 / 按刚才 / 这个 / 那个 / 重跑」），向前拼接同一段内最近 1 条 ≥ 20 字的用户消息（总长截断至 400 字），解决多轮短句召回失准。
3. **后台 `task` 会话的关键经验沉淀**：
   - 保持 `kind = task` 会话不提取用户偏好（防猜测用户），但放开**仅限工具观察（`provenance = ToolObservation`、`kind = Experience`）**的轻量提取通道，写入 `Candidate`（由 §1.2 独立证据晋升或用户过目生效），且只针对正常完成（非取消/故障）且关联了目标或由用户主/支线派出的任务，让长排查踩过的环境坑不再白白流失。

### 1.6 明确内置记忆与 Hindsight（外部提供方）的清晰边界（已拍板：O1 方案 A，2026-11-23）

- **现状问题**：`memory.md` 附录 G.6 的落地导致名义独占、实为双写（`lib.rs:1523-1559`），存在助手回复污染（`hindsight.rs:254-256`）、共享召回退化为 `scope:personal`（`hindsight.rs:180-185`）、`push_unattached = false` 时把外部线索全滤光（`provider.rs:150`）、以及记忆页「两张皮」（在内置删了线索但 Hindsight 下轮仍召回）5 处冲突。
- **已定方案（方案 A：分层协作，停提一般线索）**：
  1. **提取分工（停止一般线索双写）**：
     - `mode = "shadow"`：保持双写双查，仅供对比评估；
     - `mode = "primary"`：
       - **Hindsight 侧**接管一般历史线索（`Reference`）的提取、归并（`observation`）与 cross-encoder 召回；
       - **内置 `LearningPass::extract`** 切换为 `conventions_and_goals_only` 轻模式（提示词升版）：只提取①新的/变更的**长期约定**（`Convention`），②明确属于当前活跃目标的**目标归属线索**（带 `belongs_to`，保留目标结案自动归档闭环），并照常结算已注入内置条目的 `adopted_count`；**不再抽取无归属的一般历史线索**；
       - 用户显式触发的 `komo memory add` 与内置确认/冲突裁决照常走 `aux.db`。
  2. **净化喂料（守住防自证红线）**：
     - `komo-memory` 构造交由 `MemoryProvider::ingest` 的 `EvidenceBatch.text`（`lib.rs:1388-1394`）时，不再直接传含助手完整回答的 `transcript.rendered`，改传**净化版窗口**（仅包含用户消息与工具结果摘要，剥离助手的自由推理与总结正文），并在 `hindsight.rs:256` 的 `context` 字段明确声明仅提取用户陈述与工具观察事实。
  3. **补齐作用域与分层兼容**：
     - 写入标签：非 `agent:*` 作用域除写具体 `scope:<name>` 外同时附带 `scope:shared`；`private` 只打 `scope:agent:<id>`；
     - 共享召回（`scopes` 为空）查 `scope:shared`，指定 `scopes` 时按完整列表转 `scope:<name>`（覆盖 `personal`、`project:*`、`environment:*`），不再写死只查 `scope:personal`；
     - `prepare_with` 分层过滤时，外部提供方已通过自身 `min_reranker`（如 0.3）的命中结果由独立开关 `[memory.retrieval] push_external`（默认 `true`）控制，不再被 `push_unattached = false` 一刀切误杀。
  4. **记忆页对齐与降级口径**：
     - `primary` 模式下，记忆页清晰分为两区：**「约定与目标记忆（内置）」**（可确认 / 裁决 / 改归属 / 遗忘）与**「自动回忆库（Hindsight）」**，并在 `MemoryProvider` 接缝上评估补齐 `forget(id)`（对接 Hindsight 删除/作废接口），让用户能在界面直接删除外部误记；
     - 当 Hindsight 宕机或超时降级时，退回内置仅提供目标 + 约定 + 手动记忆 + 存量内置线索，并在召回诊断明示「外部回忆服务不可用，本次未加载近期一般线索」。
- **验收要点**：`primary` 模式下空闲提取不再往 `aux.db` 写入无归属 `Reference`；喂给 Hindsight 的请求正文不含助手自由回答；`push_unattached = false` 时 Hindsight 过门槛线索仍正常注入；非 `personal` 作用域（如 `project:*`）在共享召回中可被正确命中。

---

## 2. 介入防死锁、操作者控制面与永续主对话加固

> 目标：消除重设计删除待处理页/任务页后出现的交互死锁与控制盲区，并把永续主对话的底座补齐。

### 2.1 彻底解决同对话 `Confirm` / `RestartLimit` 死锁（对应决策 O3）

- **现状问题**：`DurableCore::say_in`（`crates/komo-gateway/src/service/durable/chats/say.rs:91`）提交 `SubmissionDraft::Write`，在会话忙时必须等到下一个 `postTools` boundary 才能落盘。当该会话自身因触发器 Run 卡在 `Checkpoint::Confirm` 或因崩溃循环停在 `RestartLimit` 时，Run 永远到不了 boundary → 提问发不出来 → 网页按钮挂不上 → 用户发消息变成 steer 排队互等 24 小时。
- **方案**：
  1. **前端侧通道置顶确认条（主路径，不依赖 `say_in` 落盘）**：
     - 在对话页（`/` 与 `/chats/$id`）输入框正上方增加**「待你确认 / 已暂停」固定浮条**，直接消费 `useInterventions()`（经 §6.3 全局 SSE 实时推送），筛选条件为「目标路由会话 == 当前对话（含本会话自身、本会话派出的任务、以及绑定到本会话的定时目标）」。
     - 每张卡片展示：**来源标签 + 所属目标标题（若有）+ 工具名 + 折叠参数预览 + 倒计时**，以及确定性按钮（`批准 / 拒绝` 或 `已处理继续 / 放弃`），点击直调 `POST /v1/interventions/{handle}/answer`。
     - 这样即便当前会话被阻塞、`say_in` 尚未落盘，操作者也能在 1 秒内看到卡片并一击解冻。
  2. **确认卡正文补齐目标上下文**：
     - `crates/komo-gateway/src/service/durable/interventions/confirms.rs` 与 `chats/attention.rs` 渲染确认问句及推送正文时，反查会话的 `cron:<goal_id>` 或 `komo.task.goals`，将「所属目标：<标题>」写入 `InterventionSummary`（新增可选字段 `goal: Option<GoalRef>`）和推送消息（Web Push + Telegram 均受益）。
  3. **防自然语言代答歧义**：
     - 保留空闲对话里口头回复由模型调 `komo(action="intervention.answer")` 的便利，但加两条硬守卫：当该对话指向的未答复介入 **> 1 条**时，系统附注强制要求模型核对句柄或提示用户点卡片按钮，严禁猜测对应关系；对 `Confirm` 类高危动作，优先在界面突出确定性按钮。

### 2.2 补齐后台任务与监听的操作者控制面

1. **接通 `DurableCore::stop_task`**：
   - 新增路由 `POST /v1/tasks/{id}/stop`（`crates/komo-gateway/src/http/v2/tasks.rs`），调用已有 `DurableCore::stop_task`（`crates/komo-gateway/src/service/durable/sessions/lifecycle.rs:252`）；
   - 新增 CLI 命令 `komo task stop <TASK_ID>`；
   - 网页过程面板（`features/process/`）与目标详情「活动」列表中，凡状态为 `running` / `waiting` 的任务行显示「停止」按钮（二次确认）。
2. **补齐操作者停止 `monitor` 的入口**：
   - 新增路由 `POST /v1/monitors/{id}/stop`（`crates/komo-gateway/src/http/v2/monitors.rs`），复用 `GatewayOrchestration::stop_monitor` + 写入 `komo.monitors` 的 `Ended { reason: "stopped_by_operator" }`；
   - 新增 CLI 命令 `komo monitor stop <MONITOR_ID>`；
   - 网页目标详情的「监听」区块与对话过程面板中，运行中的监听行提供「停止」按钮（区别于 `unlink` 仅摘引用）。
   - **统一停止行为口径**：无论真 `monitor` 还是自动转后台的 `bash`，被手动 `stop` 时默认都写一条简短的「已被操作者停止」结果交回会话（或统一加 `silent: bool` 参数，默认模型 `monitor_stop` 静默、操作者手动停交回说明），消除当前两者行为相反的心智陷阱（`monitor.md` §4 vs §8）。
3. **修复两个运行时边界漏洞**：
   - **`context: "fork"` 允许只读父会话截口前的 `artifact://`**：在 `komo-tools` 的 `artifact://` 路径解析中，传入当前会话的 `ancestor_sessions` 只读列表（从 `fork` 血缘读出，仅放行 `read`，仍禁写），解决 fork 子任务读不到上下文里超长工具输出引用的问题。
   - **统一 `ToolEnv.source` 与 Cron 派任务口径**：正确填充 `ToolEnv.source`（`Interactive` / `Cron` / `Task` / `Monitor`），并明确规则：**允许**绑定了目标的 Cron Run 派生单层后台任务（产物算该目标活动，完成后通过目标路由交回绑定支线或主对话），但禁止任务会话再嵌套派任务（保持深度上限 1）。同步清理 `background-tasks.md` 与 `confirm.md` §10 的矛盾描述。

### 2.3 加固永续主对话底座与恢复可观测性

1. **自动压缩向默认开启演进**：
   - 补齐实测脚本（覆盖 OpenAI / Anthropic / DeepSeek / Codex 等主力端点）验证 `crates/komo-llm/src/lib.rs:232` 的 `is_context_overflow` 正则集合，并按实测补齐常见中转站错误文案；
   - 修正中文 token 估算器：区分 ASCII 与非 ASCII 字节（中文/CJK 按约 `1.5 字节/token` 而非 `4 字节/token` 估算），避免实际超限远早于阈值触发；
   - 验收通过后，将 `CompactionConfig::default().enabled`（`komo-kernel/src/protocol/config.rs:659`）与 `komo init` 模板改为 `true`，并在 `redesign.md` §2.8 落地「隔久回来且上下文 > 50% 时趁缓存已冷先压缩」。
2. **冷补读增量优化**：
   - `service/durable/feed/mod.rs` 的 `RecentEvents`（512 KiB 内存环）之外，为 `project_backfill` 补上从指定 `after` entry ID 之后的范围读取（结合已有 `ContextRange` 缓存），避免 PWA 长时间休眠或重启后重连触发 O(历史) 全量投影。
3. **恢复 durable 主链路可观测性（修复盲飞）**：
   - 立即在 `crates/komo-gateway/src/service/durable/core.rs:78-81` 为 `EngineOptions.report` 接入 `tracing::warn!`（并计入错误指标），不再静默丢弃部分回复/工具进度提交失败；
   - 按 `docs/observability.md` 既定 6 步补齐 `komo-runtime/src/durable/` 的核心 span 与指标：`run`、`generation`（含 `effort` / token / cache read & write / 耗时，顺带满足前端算 tok/s 需求）、`tool`、`compaction`、`storage.commit`（含 gate 等待与 fsync 耗时）；
   - 新增无登录只读探针 `GET /healthz` 附带 `uptime_secs` 与 `engine_ok`，供外部 Uptime 监控（如 Uptime Kuma / Healthchecks）做存活告警（补齐 `docs/telegram.md` L13 提到的外部监控空白）。

---

## 3. 目标概念解耦与生命周期联动

> 目标：在保住「对话、目标、记忆」极简导航结构的前提下，解除目标对象超载和生命周期断链。

### 3.1 解耦「目标注入」与「后台定时」，放宽 1:N 定时（已拍板：O2，2026-11-23）

- **现状问题**：`TursoCronRepo::due`（`crates/komo-store/src/repos/cron.rs:346`）仅触发 `Active | Waiting` 的目标，而 `open_matters()` 把所有 `Active | Waiting` 塞进 600 token 的每轮对话注入，导致看门狗、巡检、清理脚本等后台定时挤占主对话上下文；且 `matters.schedule` 只能挂 1 个定时。
- **方案**：
  1. **新增 `inject_context: bool` 属性**：
     - `Matter`（`komo-kernel/src/types/matter.rs`）与 `matters` 表追加 `inject_context: bool`（旧行默认 `true`）。
     - 语义：`true` = 面向操作者的真实目标/职责，进入每轮对话「你在做和在管的事」注入组；`false` = 后台静默任务/纯自动化（例如看门狗、命令型定时、存量 cron 回填项默认设为 `false`），**调度器照常触发其定时，但不占用 `matter_tokens` 预算**。
     - 网页目标页默认展示「追踪中」，可将 `inject_context = false` 的项带「自动化」轻标签归在同一列表尾部或折叠小节「后台例行」，不新增顶层导航项。
  2. **1:N 定时配置**：
     - 将 `matters.schedule` JSON 演进为向下兼容的 `schedules: Vec<ScheduleItem>`（读取时兼容旧单对象 JSON 自动包成单元素 Vec），每个 `ScheduleItem` 带独立 `id`（短 UUID）、`name`（如「每日晨报」「周五复盘」）、`cron`、`timezone`、`prompt`/`command`、`notify`、`status`（active/paused）、`next_run_at`、`last_error`；
     - 表列 `matters.next_run_at` 存该目标下**所有启用定时的最早下一次触发时刻**，继续复用现有 `matters_next_run` 索引；`cron_firings` 新增可选 `schedule_id` 字段区分同一目标下的不同节奏。

### 3.2 统一有终点与无终点目标的状态机

- **现状问题**：`redesign.md` P7 把领域包装成「没有终点的目标」，但底层 `MatterKind::Area` 只有 `Active` / `Retired`，不支持 `Waiting` 与 `Parked`，用户无法暂停一项长期职责或记录它正在等外部条件。
- **方案**：
  - 在 `komo-kernel/src/types/matter.rs` 放宽 `MatterKind::Area` 的状态转换表，允许 `Active ⇄ Waiting`（填 `waiting_for`）和 `Active/Waiting ⇄ Parked`（暂停追踪，同时暂停名下定时，重开/恢复即续跑）；
  - 仅保留终态差异：有终点目标（`Goal`）终态为 `Done` / `Dropped`，无终点目标（`Area`）终态为 `Retired`（网页文案「不再追踪」）。

### 3.3 补齐结案、归档与支线绑定的生命周期联动

1. **目标结案联动面板（网页 + API）**：
   - `POST /v1/matters/{id}/{done|drop|retire}` 新增可选请求参数：
     - `stop_monitors: bool`（默认 `false` 保兼容；网页勾选框触发时若 `still_linked` 非空，弹出轻确认框列出活跃监听并默认勾选「一并停止并摘除监听」）；
     - `unbind_chat: bool`（可选同时解绑支线，并询问是否一并归档该支线）。
   - 结案返回 `closure.conventions` 非空时，网页弹出**「处置名下约定」轻抽屉**（每条约定一行按钮：「升为全局约定」`classify --for none` / 「改挂其他目标」/ 「遗忘」`forget`），不再只是闪过一行纯文本回执。
   - 结案写入与线索归档合并为同一 `aux.db` 事务（消除非原子窗口）。
2. **支线归档/删除时的绑定清理与提示**：
   - `DELETE /归档` 支线会话时，若被某个目标的 `chat_session` 引用，明确写入清零 `matters.chat_session = NULL`（并在归档确认弹窗提示「解绑后，目标『XXX』的定时通知将改发到主对话」），消除静默回落。
3. **放宽支线绑定与统一活动口径**：
   - `POST /v1/matters/{id}/chat` 支持可选 `session_id`：不传时照旧新开支线并绑定；传入已有活跃支线 ID 时，检查该支线未被其他目标绑定后直接绑定（仍保持 1:1）；
   - `komo` 工具补上 `goal.bind_chat`（允许在支线里聊出目标时直接绑定当前支线，与 P6「新建与修改走对话」一致）；
   - 统一 `goal.list` 计数口径：同时计入 `komo.task.goals` 与 `cron:{goal_id}` 活跃会话；
   - 修复跨午夜超期刷新：`hooks/recall.rs` 在 `revalidate_matters` 中顺带核对 `today` 是否跨日且有目标跨过 `due`，跨日则触发 `komo.memory.revoke` 刷新已注入块的「已超期」标记。

---

## 4. 主动性节制护栏（最小 P2）

> 目标：在 Web Push 与 Telegram 备用通知真正跑起来前，补上符合 `PRODUCT.md`「温暖而不打扰」的最小护栏，把 `roadmap-muse.md` P2 的精华轻量落地。

### 4.1 通知门控：分级、免打扰时段与在线抑制

在 `config.toml` 新增可热重载段 `[proactive]`（并开放核心开关到网页设置「通用」）：

```toml
[proactive]
level = "normal"            # off | low | normal | high
quiet_hours = "23:00-08:00" # 免打扰时段（按 Gateway 本地时区）
quiet_allow_confirm = true  # 免打扰时段仅允许 Confirm / 紧急阻塞穿透，还是全部延后
daily_digest_at = "09:00"   # low 档或免打扰期间攒下的低优消息汇总发送时刻
max_daily_pushes = 12       # 非确认类主动推送每日软上限
suppress_when_web_active = true # 网页/桌面端有活跃 SSE 且近期有交互时，静默外部推送
```

- **统一收口在 `GatewayState::notify_operator`**（`crates/komo-gateway/src/service/state.rs:174`）：
  1. 给 `PushMessage` 增加优先级枚举 `Urgency::{ActionRequired, Normal, Digest}`：`Intervention`（Confirm / Blocked）为 `ActionRequired`，`Cron` 结果与非操作者回复默认为 `Normal`（可由目标定时配置覆盖）。
  2. **在线活跃抑制**：Gateway 跟踪最近一次 Web/Desktop 客户端活跃心跳或对话提交时刻（例如 2 分钟内有活跃 SSE 连接且页面可见），若 `suppress_when_web_active = true` 且 `urgency != ActionRequired`，则只在对话列表标点、不推 Telegram 与系统通知。
  3. **免打扰与降噪攒批**：处于 `quiet_hours`、`level = "low"` 或超出 `max_daily_pushes` 时，将 `Normal` 级消息落入 `aux.db` 轻表 `proactive_digest_queue`，在次日 `daily_digest_at` 合并为一条晨间摘要投进主对话并推送一次；`level = "off"` 时不触发非手动定时推送。

### 4.2 Cron 默认安静引导与三态通知判断

- 对目标的 `prompt` 型定时任务，在不破坏用户自定义 prompt 哈希的前提下，通过系统附注（同 `komo.channel` 机制，不改用户输入正文）提供一行默认规范：*「若本次巡检无异常、无需要操作者知道的新进展，请整段仅回复 `NO_REPLY`」*（可按定时配置 `allow_silent = true/false` 开关，默认 `true`）。
- 后续支持在目标定时配置里选择 `notify = "smart"`（对应 `roadmap-muse.md` P2 艾森豪威尔三态：马上说 / 攒进每日摘要 / 不说），由模型末行结构化标记交回 Gateway 路由。

### 4.3 轻量「每周回顾（Weekly Review）」代谢循环

- 提供一个可一键启用/停用的内置系统目标模板（或设置开关「每周回顾」，默认每周日晚 20:00，用户可改时间或关闭），运行只读聚合后在主对话发出一份结构化卡片摘要，串联 4 项代谢动作：
  1. **目标体检**：已超期目标、超过 14 天无推进的 `active`/`waiting` 目标、搁置超 30 天的 `parked` 目标（提示推进/重启/放弃）；
  2. **记忆代谢**：本周新增待过目的 `candidate` 与未裁决 `contested` 对、多次带出却 `adopted_count == 0` 的低效线索、超预算的约定清单；
  3. **结案遗留**：已结案目标名下尚未处置的约定与仍挂着的监听；
  4. **一键确认**：回顾里只提建议不自动改承诺，用户回复或点按钮后才落盘。

---

## 5. 产品定位收敛与上手/部署体验

> 目标：结束「本地优先 vs 远程服务」的反复拉扯，把自托管与本机使用都变成顺畅、可预期的一条路径。

### 5.1 定位与部署收敛（已拍板：O4，2026-11-23）

- **明确「远程部署 + 本地运行」双支持，统一单二进制同源形态**：
  - **统一部署形态**：无论是跑在远程 VPS / NAS 上还是装在自己电脑（macOS / Linux）上，均以**单二进制 Gateway 同源托管静态 Web 壳（`[web] serve = true`）**为标准形态——远程通过 Cloudflare Tunnel / Tailscale / 反向代理暴露单一 HTTPS 域名，本机直接监听回环地址。
  - **退掉 CF Pages + Workers 路径分流方案**：保留 `[web] serve = false` 仅给本地前端 `pnpm dev` 或特殊高级玩家使用，从 `deploy.md` 推荐路径中移除前后端分离发布，彻底消灭 `protocol_version` 错位横幅遮挡、CORS 和 SSE 双层缓冲问题。
  - **本机直连 & 远程应急**：
    - 本机跑 `komo` / `komo web` 与桌面端「本机模式」继续走 `POST /v1/auth/local-link` 与本机令牌，免 OIDC 开箱即用；
    - 远程应急保留 `docs/telegram.md` 内置最小 Telegram 私聊（并在网页设置「连接」里展示 Telegram 启用状态与操作者 ID，不再隐身）。

### 5.2 降低首次上手与远程认证门槛（已拍板：O5，2026-11-23）

1. **提供无 IdP 的轻量单人远程认证**：
   - 在 `[auth]` 下保留 `[auth.oidc]`（Pocket ID / 任意 OIDC）为首选；
   - 新增两个低门槛选项（三选一，互斥校验）：
     - **`[auth.access]`（Cloudflare Access / 反向代理信任头）**：校验 Cloudflare Access 的 `Cf-Access-Jwt-Assertion`（用已有 `ring` 验 JWKS 签名 + `aud` + 允许邮箱），无需额外自建 Pocket ID；
     - **`[auth.passphrase]`（单人主口令 + 限流）**：`komo auth set-passphrase` 在 `.env` 存 Argon2/PBKDF2 哈希，网页登录页输入口令换取同款 30 天 HttpOnly `auth_sessions` cookie（内置按 IP/全局 5 次失败退避限流）。
   - 同时为现有 OIDC/会话补齐安全上限：cookie 绝对过期上限 90 天（滑动 30 天）、设备令牌支持设置有效期或闲置 180 天自动失效。
2. **`komo init` 与首次 Web 引导闭环**：
   - `komo init` 允许零编辑直接启动 Gateway（已有“无 key 也能启动”能力），并打印一次性本机链接；
   - 浏览器打开后，若检测到未配置可用对话模型或未配置向量模型，自动进入简洁的**首次引导页（2 步：选 Provider 填 Key → 可选配向量模型启用混合记忆）**，不再让新人手改 TOML，也不再因缺 embedding 默默把记忆永久锁死在 `mode = "keyword"`。

### 5.3 网页补齐远程运维闭环（重启、MCP、Codex）

1. **受控延迟重启（`POST /v1/gateway/restart`）**：
   - 复用 `crates/komo-gateway/src/service/units.rs` 的 `defer` 机制（仅当 Gateway 处于 launchd / systemd 托管模式，且先等待当前无活跃提交或排在 HTTP 202 响应返回 500ms 后执行；非托管前台模式明确返回不可远程重启提示）；
   - 当网页修改了启动期键（如 `memory.embedding`、`[push]`、`[telegram]`、`[auth]`）后，回执栏直接提供**「立即重启 Gateway」按钮**，并在重启期间前端自动轮询 `/healthz` 恢复连接，免去远程 SSH。
   - 改进 `units::defer` 的固定等待：将 `1.5s` / `--on-active=2` 升级为先发平滑停机信号（排干正在落盘的写入再退出），缩短且消除固定秒数竞态。
2. **网页设置补齐缺失项**：
   - **MCP 增删改与 OAuth**：在设置「连接 → 工具」支持添加/编辑/删除 stdio 与 HTTP MCP 服务器（走同一套 `toml_edit` + `version` + 先校验后落盘），统一联调 OAuth 登录按钮（无 `public_url` 时回退本机回调或给出明确配置引导）；
   - **Codex 网页设备码登录**：新增 `POST /v1/auth/codex/start` 与 `GET /v1/auth/codex/status`，在设置「模型」里展示设备码与授权链接，完成远程登录。

### 5.4 消除 `komo update` 到重启间的 `codemode` 瘫痪窗口

- **现状问题**：`komo update` 原子替换磁盘二进制不重启运行中进程，旧 Gateway 随后拉起新 `__codemode-worker` 时因 `hello == ours`（严格比对 `komo` 版本字符串，`crates/komo-tools/src/codemode/mod.rs:278-293`）立刻全盘报错，导致所有 `codemode` 和默认 MCP 工具在重启前全挂。
- **方案**：
  - **握手按协议号兼容**：`Hello` 校验只强要求 `protocol == WORKER_PROTOCOL && monty == MONTY_VERSION`，`komo` 自身版本号不同仅记 `warn` 日志并附带「二进制已更新，建议重启 Gateway」状态标记，**不再阻断同协议版本的 worker 执行**；
  - **二进制热更新安全兜底**：Linux 上旧进程可通过 `/proc/self/exe`（若未删除）或 `komo update` 提示 `--restart` 选项（默认询问或提供 `komo update --restart` 一键平滑重启）；仅当 `WORKER_PROTOCOL` 或 `MONTY_VERSION` 真的破坏性变更时才拒绝执行并置顶重启提示。

---

## 6. 运行时、协议、存储与文档减负

> 目标：把超出「简洁单人服务」定位的复杂度和工程债逐步卸载掉。

### 6.1 `codemode`（Monty）止血收口与 MCP 解绑（已拍板：O6 方案 2，2026-11-23）

- **现状与对照 `pi` 评估**：
  - 线上统计显示 `codemode` 近 45 天调用 1649 次（日均 ~36 次，`codemode.md` §6），属于高频刚需，不宜直接卸载；
  - 对照 `~/oss/pi`（`packages/durable/src/tool-search/tool-search.ts` 与 `packages/coding-agent/docs/{codemode,mcp}.md`），Komo 当前的核心痛点有两个：① MCP 强绑在 `codemode` 上（缺少 `tool_search` 独立发现路径，导致 `komo update` 握手失败或禁用 codemode 时 MCP 全瘫，且无人值守 `outward` 工具不得不临时改写顶层 schema 击穿前缀缓存）；② `monty-fs` 裸文件挂载读文件不记账，引发崩溃重放分歧，且安全边界在放开 `tools.bash` 后已名存实亡。
- **已定方案（方案 2：`tool_search` 解绑 MCP + 保留精简 Monty）**：
  1. **引入 `pi` 式 `tool_search` + `deferred` 暴露模式（纯 Rust，零新依赖）**：
     - 在 `komo-tools` 新增只读安全工具 `tool_search(query, limit?)`（内存 BM25 检索当前会话未声明的 deferred 工具名、描述与参数名，默认上限 8）；
     - 命中工具通过会话文档（如 `komo.loaded_tools`，支持 rewind/fork/压缩保留）持久加入本分支的已声明工具集，下一轮模型直接按原生顶层工具调用；
     - MCP 服务器新增 `exposure = "deferred"`，且在 `codemode` 未启用或不可用时，`exposure = "codemode"` 的工具自动退化为 `deferred` 可经 `tool_search` 发现调用，彻底解除 MCP 对 `codemode` 的单点绑定；
     - 无人值守 Run 下的 `outward` MCP 工具不再强制全量塞进顶层 schema 击穿前缀缓存，可由 `tool_search` 按需加载并照常进入 `Checkpoint::Confirm`。
  2. **保留 Monty 并做三项收口**：
     - **移除 `monty-fs` 裸文件挂载**：脚本内禁止直接 `open()` / `pathlib` 读宿主文件系统，读文件统一走 `tools.read` / `tools.rg`（所有外部输入必经宿主可重放边界），彻底消除未记账只读文件变更引起的重放分歧，同时卸掉 `monty-fs` + `cap-std` 相关依赖与符号链接保护逻辑；
     - **放宽 worker 握手协议**（联动 §5.4）：仅校验 `protocol == WORKER_PROTOCOL && monty == MONTY_VERSION`，不再因 `komo` 版本号不同阻断执行；
     - **可选增补跨调用小状态 `store(key, value)` / `load(key)`**：借鉴 `pi` 的 `CodemodeStoreDoc`（conversation 级可 rewind/fork 文档），用于跨脚本保留游标与 ID 列表，减少中间大文本回灌上下文。
  3. **远期演进预留（记录备选）**：
     - 若后续 Monty 的 Python 子集维护成本（111/1649 报错、频繁升 `prelude` 协议、`mimalloc` 冲突、76+ crate 编译负担）持续高于收益，可沿用同一套 `komo.nested` 嵌套任务层平滑切换为 `pi` 式 **QuickJS（`rquickjs`）纯内存无 IO 沙箱**（完整 ES2023 + 引擎内建硬内存限额 + 真 `Promise.allSettled` 非阻塞交付）；本次暂不引入 `rquickjs` 桥接成本。
     - 明确不采用 `pi-rust` 的重型 `ToolProgram` 插件 ABI 协议栈。
- **验收要点**：停用 `codemode` 时仍可通过 `tool_search` 正常发现并调用 MCP 工具；脚本内 `open()` 明确报错指路 `tools.read`，`monty-fs` 从依赖树移除；`komo update` 替换同协议新二进制后重启前 `codemode` 与 MCP 不断服。

### 6.2 收敛 MCP 开关状态机与 `komo skills` 统一路径

1. **移除内存临时态 `POST /v1/mcp/disconnect`**：
   - 删除 `crates/komo-gateway/src/http/mcp.rs:46` 及对应路由与前端/CLI 入口，所有服务器启停统一收口为持久的 `PUT /v1/mcp/{name}`（写 `enabled = false/true` + 热重载，工具从能力面干净移除）+ `POST /v1/mcp/reconnect`。
2. **`komo skills enable|disable` 统一经 Gateway**：
   - 让 CLI 的 `komo skills enable|disable`（`crates/komo/src/commands.rs:712-762`）改为优先调用 Gateway 现有的 `PUT /v1/skills/{name}`（立即触发 `refresh_skills_prompt()`）；仅在 Gateway 未运行时才直写 `runtime/skills-disabled.json`；
   - 同时把 `runtime/skills-disabled.json` 的 mtime 纳入配置热重载轮询，彻底消除 CLI 与 Gateway 的状态分裂。

### 6.3 新增全局轻量 SSE，终结全端 6+ 路轮询风暴

- **现状问题**：`packages/api/src/` 叠加了 `useChats`（4s/15s）、`useInterventions`（30s）、`useMonitors`（4s/15s）、`useMatter`（5s）、`usePendingMemories`（60s）、`useConfigIssueCount`（5m）以及 Electron 托盘 5s 轮询。
- **方案**：
  - Gateway 新增单条轻量全局流 `GET /v1/events/global`（复用鉴权与 SSE 基础设施，仅推**失效提示 / 轻摘要事件**，不推完整对话正文）：
    - `chats.changed { session }`（任一会话有新消息、状态变更或改名）
    - `interventions.changed { count }`（新增或答复介入）
    - `matters.changed { id? }`（目标变更或定时状态更新）
    - `memories.pending_changed { count }`（新增候选或冲突）
    - `monitors.changed` / `config.reloaded` / `mcp.changed`
  - 前端（Web + Electron）建立单一 `useGlobalEvents()` 订阅：收到事件即精准 `queryClient.invalidateQueries(...)`，并将各路 `refetchInterval` 降为「仅在全局 SSE 断开时回退轮询（30s）」，同时兼顾移动端回前台重连后自动触发一次关键 query 刷新（闭合 `redesign.md` §1.8 L109）。

### 6.4 协议单一生成源与消除静默截断

1. **100% 收口到 `ts-rs`**：
   - 将 `PushKeyResponse`、`PushSubscribeRequest` 等类型定义移入 `komo-kernel/src/protocol/` 并加入 `export_all`（删除手写的 `packages/protocol/src/push.ts`）；
   - 把 `PROTOCOL_VERSION` 通过 `ts-rs` 或统一构建脚本生成进 `packages/protocol/src/generated/version.ts`（删除手写 `version.ts`，与 Electron 共享单一真相）；
   - 为 `EventPayload` 补齐强类型导出，替换前端 `WireEvent.data: unknown` 的手工类型断言。
2. **消除前端静默截断与全表扫**：
   - **会话标题查找**：不再用 `GET /v1/sessions?all=1&limit=50` 赌第一页；新增轻量 `POST /v1/sessions/titles`（批量按 ID 查标题与状态）或由目标/记忆响应直接附带关联会话标题；
   - **记忆页计数与点阵**：`GET /v1/memories` 返回服务端聚合 `stats { total, active_conventions, active_clues, pending_candidates, contested, archived }`，列表改为真正游标分页，不再写死 `limit=500`；
   - **协议版本不一致提示条**：将 `apps/client/src/features/auth/protocol-banner.tsx` 从覆盖式 `fixed` 改为文档流顶部非遮挡布局（给 Electron 拖窗口区让位）；
   - **浏览器改 Gateway 地址探测**：放弃 `no-cors`（无法区分 200/404/500），采用明确跳转探测或由 Gateway 配置允许的 CORS 探针头。
3. **清理历史死接口与死表**：
   - 明确删除已废接口 `/v1/sessions/{id}/fork`（若确认网页不再恢复单条分叉）；保留 `/v1/tasks` 供过程面板、目标活动和 `komo task stop` 使用，去掉其“待定”状态。

### 6.5 中期架构收口：`aux.db` 并入 `durable.db`（对应决策 O7）

- **动机**：消除 Turso WAL（裸 SQL）与 Turso MVCC（`toasty` ORM）双轨并存带来的宏编译耗时、MVCC DDL 不落盘绕行、以及跨库无索引全扫（目标活动时间线、purge 引用检查）。
- **演进路径（3 步零丢数迁移）**：
  1. **Schema 合并**：在 `komo-store/src/durable/schema.rs` 追加 `matters`、`cron_firings`、`memory_items`、`memory_terms`、`memory_embeddings`、`auth_sessions`、`push_subscriptions` 的裸 SQL `*_TABLE_DDL`（去掉已退役的 `deliveries`、`cron_jobs`），并为 `sessions` 补上 `(source)` / 目标关联索引；
  2. **一次性启动迁移**：Gateway 启动若检测到存在 `aux.db` 且 `durable.db` 中对应迁移标记未完成，在单事务内 `ATTACH` / 复制表数据、校验行数一致后将 `aux.db` 重命名为 `aux.db.migrated`（不删除，方便回退）；
  3. **卸载 `toasty`**：移除 `komo-store` 对 `toasty` 与 `toasty-driver-turso` 的全部依赖及 `[patch]`，复测冷编与增量编译时间写回 `komo_bot.md` §13.4。

### 6.6 积压切片封板与文档“单一真相”重构

- **文档清理清单**（紧随各波次代码落地同步完成）：
  1. **[`PRODUCT.md`](../PRODUCT.md)**：统一「单人常驻服务 + 本机可用」定位，删掉 L15 残留的「飞书/Telegram/微信插件进程」矛盾句，补记 Telegram 备用入口；
  2. **[`docs/komo_bot.md`](komo_bot.md)**：把已删除的渠道插件、TUI、Policy、toolbox、旧 `state.db`/JSONL 故障注入表整体归档至 `docs/archive/komo_bot_v08_history.md`，主文档瘦身为反映现行 11 个 crate 与最新架构的精简权威版（修掉 §1“七个 crate”、§4 `rg` 自相矛盾、§11“没有任何聊天入口”等错漏）；
  3. **[`docs/tasks.md`](tasks.md)**：将状态改为 `superseded` 移入 `docs/archive/`（其中仍有效的部分已全部进入 `redesign.md` 与本文）；
  4. **[`docs/web.md`](web.md) / [`docs/clients.md`](clients.md) / [`docs/deploy.md`](deploy.md)**：统一停止按钮位置（§4 vs §5）、⌘K 路由（`/goals/$id`）、本机 `local-link` 与同源部署口径；
  5. **[`docs/goals.md`](goals.md) / [`docs/confirm.md`](confirm.md) / [`docs/codemode.md`](codemode.md)**：同步无终点目标状态机、1:N 定时、`inject_context`、`durable.db*` 保护名单及 `home_chat` 清理后的最新口径。

---

## 7. 建议实施顺序与验收标准

按风险与收益分为 4 个波次，**先止血封板，再补核心闭环，最后做架构减负**：

| 波次 | 目标 | 核心交付项 | 验收标准 |
|---|---|---|---|
| **Wave 0：封板止血** | 消化积压的未编译/未实测债，建立干净基线 | 解决 `claude/komo-web-redesign-61bc08` 分支基线；全量编通并端到端实测 `redesign.md` 3b/4b/4c/5a–5e、cron 回填（真实 `~/.komo` 副本演练）、真实 Pocket ID OIDC、真实 Telegram Bot、真实 iOS/桌面 PWA Web Push；修复 `EngineOptions.report` 空函数 | `cargo test --workspace` 与 `pnpm build/lint/typecheck` 全绿；上述未实测项全部有真机验收记录 |
| **Wave 1：核心闭环（P0）** | 打通「越用越懂我」与「不死锁、停得掉」 | §1.1 `contested_with` 裁决与红点；§1.2 独立证据自动晋升；§1.3 `adopted_count` 降权与 `sweep`；§1.4 默认门槛与详情页编辑 + ⌘K 搜记忆；§1.5 目标定时记忆注入与短句增强；§2.1 侧通道确认条破死锁 + 目标名上下文；§2.2 `stop_task` 与 `monitor stop`；§2.3 压缩实测开启与 durable 核心遥测 | 冲突记忆一键裁决后胜出方立即可召回、对端 superseded；同会话触发器停在 Confirm 时网页可直接点按钮解冻；后台 task 和 monitor 均可在网页/CLI 停止；Grafana 恢复主链路指标 |
| **Wave 2：目标、护栏与上手** | 概念顺滑、主动不打扰、新人顺畅上手 | §1.6 Hindsight 方案 A（净化喂料 + 停提一般线索）；§3.1–3.3 `inject_context`、1:N 定时、Area 状态机、结案联动抽屉、灵活绑支线；§4.1–4.3 `[proactive]` 免打扰/在线抑制/摘要/每周回顾；§5.1–5.4 双支持同源收敛、轻量认证、首次 Web 引导、网页受控重启、`codemode` 握手兼容；§6.1 `tool_search` + `deferred` 解绑 MCP | 后台看门狗定时正常运行且不占 600 token 注入；免打扰时段非确认推送攒进晨报、网页活跃时不双推 Telegram；本机免 OIDC 开箱、远程无 Pocket ID 也能 5 分钟登录；`komo update` 后未重启前同协议 `codemode`/MCP 不挂；关掉 `codemode` 后 `tool_search` 照常调用 MCP |
| **Wave 3：架构减负与文档重构** | 卸载历史包袱，恢复单一真相 | §6.1 移除 `monty-fs` 裸文件挂载（+ 可选 `store/load`）；§6.2 删 `mcp/disconnect`、`komo skills` 统一经 Gateway；§6.3 全局轻量 SSE 替换 6+ 路轮询；§6.4 100% `ts-rs` 协议与消除截断；§6.5 `aux.db` 并入 `durable.db` 卸载 `toasty`；§6.6 全仓文档单一真相重构 | 前端空闲状态零轮询风暴；`packages/protocol` 无手写协议副本；`monty-fs` 与 `toasty` 移出依赖树、单数据库运行且冷编时间显著下降；`PRODUCT.md` 与 `docs/komo_bot.md` 无自相矛盾项 |
