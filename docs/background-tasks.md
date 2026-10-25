# 后台任务：结果交回 home

> 状态：已落地（2026-10-08），2026-10-25 起跑在新存储（pi-durable 提交模型）上。取代 `docs/home-dispatcher.md` 那套"home 改成分发器"的设计；
> 与 `docs/komo_bot.md` 冲突处以本文为准。
> 2026-10-16 定并落地：`dispatch` + `follow` 合成 `task`，同步 `delegate` 删掉，加 `model` / `effort` / `contract`，交回的结果在宿主还在跑时并入（§2a）。
> 2026-10-23 定并落地：`context: "fork"`，任务带着父会话的上下文起步、复用父的提示缓存（§2b）。
> 新存储上：任务是父 conversation 派生的**子 conversation**，交回是子 run 收尾的**同一次提交**里往父 conversation 提交一条输入（runtime 的 `report`，`crates/komo-runtime/src/durable/harness/orchestration/`）；旧的 `run_watch` / `report_to_parent` / 请求键 `task-result:{run}` 的看客路径已在 v0.8 存储重写中移除，见 `docs/storage-durable.md` §4.6、§13.2。

## 1. 问题

操作者的私聊全部落进同一个 home session（§11.2），而同一 Session 的 conversation 同一时刻只跑一条 run：一件要跑几分钟的事放在 home 里跑，
这段时间从任何渠道发来的话都排在它后面。

上一版的解法是把 home 改成**分发器**（`[home] mode = "dispatch"`）：home 用一个只有
`dispatch` / `follow` 的 Profile 跑，需要动手的一律派进任务会话，结果由任务会话**直接**
投回渠道，分发器靠系统提示里的"任务看板"知道任务的状态。实际用下来两个毛病：

1. **home 看不见结果。** 结果绕过 home 直接投给渠道，home 的历史里只有一句"已派出 #cb16"。
   用户接着问"进度怎么样"，home 答不上来，只能去读看板上截断到 80 字的首行。
2. **多一个身份，多一套东西。** 分发器 Profile、`[home]` 配置与校验、`freeze_run` 的分流、
   受理时冻结的看板（`RunSnapshot.dispatcher_tasks_ref`）、`#短号` 确定性路由——全是为了
   让一个不能动手的 home 也能用。

## 2. 现在的形状

```text
私聊 / komo home ──▶ home session（就是这个 Agent 本身，能动手，也能派任务）
                       │ task(prompt, title, model?) → 回"已派出 #12：标题"，这一轮照常往下
                       ▼
                     任务会话 #12（sessions 行 kind = task，子 conversation 12，同一个 Agent）
                       │ run 收尾（回答 / 失败 / 取消 / 预算 / faulted）
                       ▼
                     同一次提交里往 home 的 conversation 提交一条内部输入
                       │ "[后台任务 #12「标题」已完成] …"
                       ▼
                     home 跑一轮，把结论转告来源渠道
```

- **没有分发器。** home 用会话自己的 Agent 跑；什么时候派、什么时候自己做，由模型按 `task` 的工具说明判断（几秒能做完的直接做）。工具说明只留这条判断、prompt 要自包含与结果交回的方式，追问、查进度、`contract`、并行的细则在 `tool://task/doc`（按需读，`docs/komo_bot.md` §4）。
- **派出 = 一次提交。** `task` 是一个 `komo.tool` 任务，`call` 阶段在 `Tool::prepare` 之后转进 `orchestration`（不走通用的 intent → execute）：同一次提交里建**锚任务**（`komo.background`，conversation 所有、background、立刻 completed）、子 conversation（所有者是锚任务）、`komo.task` document（父、标题、模型 / effort 名字、结果契约）、`sessions` 行（`kind = task`、`parent_session`）、第一条输入与它的 run，并把工具任务的 checkpoint 推进到 `orchestrate { reply }`，之后另一次提交落"已派出"结果 entry。两次之间崩溃，恢复从 `orchestrate` 接着落结果，**不会再派一遍**。锚任务让子 conversation 是 background 的：它不挡父的 idle，父的 run 被取消也不连带子（与旧规则一致）；子有活时锚任务停在 `completing`。
- **句柄**是子 conversation 的十进制 ID（`#12`；`komo session list` 里的"任务·#xxxx"标注仍是会话 UUID 后四位，客户端协议冻结，与它对不上）：`task_id` 解析成 conversation 后核对 `komo.task.parent` 就是调用方，不需要登记表。
- **交回 = runtime 的 `report`，同一次提交。** 子 conversation 的 run 每个出口（回答、模型失败、预算、abort、调度器写的 faulted / orphaned）都经 `report::end_run` / `end_run_abnormal`：在结束 run 的**同一次提交**里往父 conversation 受理一条 steer 输入——entry kind `task.result`，请求键 `task-result:{子 conversation}:{这条 run 的首个输入}`，在父里按请求键去重，所以原子、幂等，没有"终态了、交回之前崩溃"那个旧窗口。正文是 `[后台任务 #12「标题」已完成 / 没有完成] …`（任务的最终回复，或失败 / 取消的原因）。
- **内部输入。** 交回的输入（以及监听的 `monitor.event` / `monitor.end`）带 `internal` 标记，规则三条：①取消父的 run 时 `abort_conversation` 只撤用户输入，**不撤内部输入**；②父的 run 非正常收尾（取消、失败、预算、fault、orphan）时，同一次提交里按 final boundary 放置收件箱里排着的全部输入、起后继 run——父照旧跑一轮把结论转告用户；③显式的单条 `Harness::withdraw` 对内部输入仍有效。**删除会话**时先撤回全部排队的输入（含内部输入）再 abort，不会再起后继 run（`docs/komo_bot.md` §8.10）。
- **父忙时并入。** 交回的输入是 steer：父的 run 在跑，就在它的 postTools boundary 并进当前 run（旧实现的"插话并入"）；父空闲就直接放置、起新 run。回复由宿主那一轮投回来源对端——来源对端取"当前路径上最近一条带 peer 的用户输入"的 peer，没有就退回 home chat（只有主会话；TUI / HTTP 开的会话不投）。任务自己的最终回复不再直接投给渠道。
- **父会话不在了**（删掉、purge）：`report` 在受理之前就返回，结果不交（任务会话本身仍在，`komo session list` 看得到）。
- **任务会话里不能再派任务**：调用方 conversation 自己有 `komo.task` 就拒绝，结果只交回一层。fresh 任务的能力面里没有 `task` 与 `monitor`；fork 任务保留父的（见 §2b）。
- **接着做**：`task(prompt, task_id)` 往已有的任务 conversation 提交一条输入，它带着自己的历史接着做；结果同样交回。不带 `contract` 的追问是 steer（任务正忙时并进当前 run），带 `contract` 的排在后面。
- **查进度**：`task(task_id)` 不带 `prompt`，只读那个任务的 `komo.live`、entry 与 `komo.task`，不提交任何东西：状态、已跑多久、正在执行的调用及其输出尾部；已收尾的 run 的终态来自 `komo.task.lastRun`。
- **取消**：模型没有取消子任务的工具；显式取消是 `Harness::abort_conversation(engine, 子 conversation)`（子 run 以 `aborted` 收尾，照常交回一条"没有完成"）。`/cancel` 只停 home 当前这一轮（`abort_run`），不动任务。
- **"需要你判断"**：任务里的调用没有审批，也没有结果不明的等待；会让任务停下的只有 faulted 的任务（blob 引用损坏），出现在 `komo intervention list` 里，句柄前挂上 `后台任务 #12「标题」：`，同时看着几个任务时分得清是哪一个在问（`docs/komo_bot.md` §7.5）。

## 2a. `task`：合并 `dispatch` / `follow` / `delegate`（2026-10-16 定并落地）

同步的 `delegate`（子 Run 在同一 Session 里、父等它）删掉。不阻塞的子任务留不在父的 conversation 里——挪出去就是任务会话。它原来多出来的几样并进 `task`：

```text
task(prompt, title, task_id?, model?, effort?, contract?)
```

- **`model` / `effort`**：alias 与档位按当前配置快照在 `prepare` 校验，可选 alias 以 enum 写进工具说明（只给 `effort` 时所选模型在派出那一步才知道，由 gateway 的 `OrchestrationHooks::check_effort` 按 Agent 默认模型或任务上一次的模型校验）。省略时新任务用 Agent 的默认，接着做沿用那个任务上一次的。两者以**名字**写进子 conversation 的 `komo.task`，不冻结成配置：每个阶段（每次请求）按当前配置解析，已开始的请求用 checkpoint 里冻结的。**交回父的那条输入不带**，父按自己的默认。
- **`contract`**：存在 `komo.task.contracts`（key 是输入的 submission ID），装配时补在那条输入的任务后面；交回之前 runtime 用 `executor::check_contract` 复验任务的最后回复，复验结论写进交回的正文（不合契约也照样交回）。
- **Cron 触发的 run 不能派**：`prepare` 拒绝——结果交回 Cron 会话时没有对端也没有看客。
- **不是子代理**：同一个 Agent、主 Agent 的装配（记忆、skills 照常）。
- 旧的 `dispatch` / `follow` / `delegate` 不再有：新存储不读旧会话。

## 2b. `context: "fork"`：带着父会话的上下文派任务（2026-10-23 定并落地）

fresh 任务只拿到 `prompt`，父会话里聊清楚的东西要靠模型写进简报，写漏了任务就猜。fork 让任务从父会话**此刻**的对话接着想，并且首请求复用父会话的提示缓存——前缀与父的上一次请求逐字节相同，只多出这一轮的结尾。新存储里这是 pi-durable 的 `fork(at)` 原生做的：子 conversation 沿 `parent.at` 读父的 entry，不复制。

```text
task(prompt, title, task_id?, model?, effort?, contract?, context?)   context: "fresh"（默认）| "fork"
```

- **只用于新开任务。** 带 `task_id` 时给 `context` 在 `prepare` 拒；fork 时给 `model` / `effort` 也拒：fork 沿用父的模型与 effort，换了就不是同一个缓存前缀。`contract` 照常。
- **切口在父此刻最新的 entry 上**（`fork(at = 父最新 entry)`，派出时只有这一个工具任务在写父）。切在"发起调用的那条 assistant entry"上会把同一轮里排在前面、已收尾的调用结果落在切口之外，所以取最新一条。发起调用的那条消息里还没有结果的调用，在**子里**补一条 `komo.tool-result`，不进父：`task` 那次是"这次调用派出的就是你（fork）。……你最后的回复会作为这次调用的结果交回给它"（`FORK_CALL_RESULT`）；同一轮排在后面的是"派你的会话在派出你之后才执行这次调用，你看不到它的结果"（`FORK_LATER_CALL_RESULT`）。
- **角色说明在输入里，不在系统提示里。** 子的第一条输入开头是 `FORK_NOTICE`：上面是派你的会话到此刻的对话，你只做下面这件事；父同时还在跑，同一个工作目录里的文件可能正在变，不要接着做它没做完的事，不要改它正在改的东西，也不要再派任务；做完就停，最后的回复交回。然后是 `prompt`。
- **身份沿用父。** fork 任务的系统段取父那份（`replay_sections`），模型与 effort 沿用父，provider session 身份（提示缓存路由键）用父的——pi-durable 里 fork 会拿新的 provider session 身份，这里按 komo 改成不换；记忆段已在父的系统提示里，不再召回。能力面**保留父的（含 `task` 与 `monitor`）**，否则工具声明变了整段缓存作废；"不能再派"落在派出那一步（调用方 conversation 有 `komo.task` 就拒，落一条失败结果）。
- **fork 不复制 document**：子不继承父的 `komo.live` 等；父追加在切口之后的 entry 子看不到。
- **局限。** ① 前缀压不掉：父会话已经贴着窗口时派 fork，fork 自己跑几轮就可能超长（超长时按压缩的救援路径压 fork 自己，`docs/komo_bot.md` §6）——工具说明因此让模型在对话很长时用 fresh。② 前缀里的 `artifact://` 与 `tool-output/` 引用指向父会话的 Session 目录，fork 里 `read` 不到（只读根是当前会话的）。
- **不做。** 用户手动分叉会话（`/fork`、`/clone`）是另一件事（`docs/session-tree.md`）。

验收：`harness/tests/orchestration.rs`（26 项，含 3 项 Turso：派出 / 交回 / 重开不重复交回，fork 与接着做，监听登记与对账；派出提交之后崩溃不重派）；gateway 的 `tests/agents/durable.rs`（`a_background_task_reports_back_through_home`、`a_fork_starts_from_the_parents_conversation_and_shares_its_prefix`、`a_fork_resumed_after_a_restart_sends_the_same_prefix`、`a_status_query_shows_what_a_running_task_is_doing` 等）；`tests/recovery_durable`（`a_background_task_in_flight_across_a_restart_still_reports_back`）。真机上 fork 首请求 4810 个输入 token 里 4016 个读自缓存（83%，2026-10-23 旧路径实测，前缀构造没变）。**未做**：Anthropic 原生接口上的核对（本机 relay 不回报缓存用量）。

## 3. 重启

没有只活在内存里的看客负责"交回"：交回在子 run 收尾的提交里，重启后任务停在哪个阶段由引擎按 checkpoint 接着跑（`docs/komo_bot.md` §8.4）。派出提交之后、落结果 entry 之前崩溃，从 `orchestrate` 接着落结果，不重派；子 run 收尾与交回同一次提交，所以不存在"终态之后、交回之前崩溃"的窗口，也不需要对账补交。gateway 仍给每个未结算的输入挂一个等结算的小任务（`watch::reattach`），那是**渠道投递**用的——把 home 转告的那一轮回复投回来源对端，按逻辑键幂等（`deliver_once`）。

## 4. 删掉的

`[home]`（`mode` / `dispatcher` / `worker`）及其校验、分发器 Profile、任务看板、`#短号` 确定性路由（旧的四位 UUID 短号）；旧存储里的 `run_watch::finish` / `tasks::report_to_parent` / `GatewayTaskSpawner` 的状态部分与"终态之后、交回之前崩溃"的已知窗口，随 v0.8 存储重写移除。配置里残留的 `[home]` 段会被忽略，可以直接删掉。

## 5. 验收

见 §2b 末尾；另有：`harness/tests/inbox.rs`（`a_failed_run_is_followed_by_the_queued_user_input`、`a_failed_run_places_the_queued_user_and_internal_inputs_together_in_id_order`：内部输入在取消 / 失败后照常放置）、`tests/agents/durable.rs` 的 `home_answers_while_a_task_is_stuck_in_a_call`（任务停在一次调用里时 home 照样答话）。

## 6. 后续

- 长任务的主动进度提示（任务自己隔一阵说一句）：按需查询已做（§2，`task(task_id)`），主动报仍未做。
- 交回的那一轮允许模型不说话（hermes 的 NO_REPLY，只对内部输入开放）。
- 登记里的已结束监听只留最近 64 条；`komo.task` 随子 conversation 一直在，没有按时间清理。
