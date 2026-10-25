# 后台任务：结果交回 home

> 状态：已落地（2026-10-08）。取代 `docs/home-dispatcher.md` 那套"home 改成分发器"的设计；
> 与 `docs/komo_bot.md` 冲突处以本文为准。
> 2026-10-16 定并落地：`dispatch` + `follow` 合成 `task`，同步 `delegate` 删掉、子代理并进来，
> 加 `model` / `effort` / `contract`，交回的结果在宿主还在跑时并入（§2a；`docs/komo_bot.md` §2、§4、§6）。
> 2026-10-23 定并落地：`context: "fork"`，任务带着父会话的上下文起步、复用父的提示缓存（§2b）。
> 新存储上的执行路径（子 conversation、同一次提交交回、fork 直接读父链）见 `docs/storage-durable.md` §4.6 与 §13；本文的语义（派出、接着做、`model` / `effort`、契约、不连带取消）不变，§3 的"终态之后、交回之前崩溃"窗口在新路径里不存在。

## 1. 问题

操作者的私聊全部落进同一个 home session（§11.2），而同一 Session 严格串行
（`crates/komo-store/src/repos/queue.rs` 的 `CLAIM_SQL`）：一件要跑几分钟的事放在 home 里跑，
这段时间从任何渠道发来的话都排在它后面。

上一版的解法是把 home 改成**分发器**（`[home] mode = "dispatch"`）：home 用一个只有
`dispatch` / `follow` 的 Profile 跑，需要动手的一律派进任务会话，结果由任务会话**直接**
投回渠道，分发器靠系统提示里的"任务看板"知道任务的状态。实际用下来两个毛病：

1. **home 看不见结果。** 结果绕过 home 直接投给渠道，home 的历史里只有一句"已派出 #cb16"。
   用户接着问"进度怎么样"，home 答不上来，只能去读看板上截断到 80 字的首行。
2. **多一个身份，多一套东西。** 分发器 Profile、`[home]` 配置与校验、`freeze_run` 的分流、
   受理时冻结的看板（`RunSnapshot.dispatcher_tasks_ref`）、`#短号` 确定性路由——全是为了
   让一个不能动手的 home 也能用。

## 2. 现在的形状（借鉴 hermes 的 async delegation）

```text
私聊 / komo home ──▶ home session（就是这个 Agent 本身，能动手，也能派任务）
                       │ task(prompt, title, model?) → 回"已派出 #2a9c：标题"，这一轮照常往下
                       ▼
                     任务会话 #2a9c（kind = task，origin = task:{home}，同一个 Agent）
                       │ 跑完 / 失败 / 取消
                       ▼
                     结果作为一条内部输入交回 home（请求键 task-result:{run}）
                       │ "[后台任务 #2a9c「标题」已完成] …"
                       ▼
                     home 跑一轮，把结论转告来源渠道
```

- **没有分发器。** home 用会话自己的 Agent 跑；什么时候派、什么时候自己做，由模型按
  `task` 的工具说明判断（几秒能做完的直接做）。2026-10-23 起 description 只留这条判断、prompt 要自包含与结果交回的方式，追问、查进度、`contract`、并行的细则在 `tool://task/doc`（按需读、不进工具定义，`docs/komo_bot.md` §4）。
- **结果交回 home。** 任务会话的 Run 进终态时，`service::run_watch::finish` 调
  `service::tasks::report_to_parent`：任务的最终回复（或失败 / 取消的原因）拼成一条以
  `[后台任务 #短号「标题」…]` 开头的输入，`submit` 进派它的会话，peer 是任务 Run 的来源对端。
  home 那一轮的回复照常投回来源。任务自己的最终回复**不再**直接投给渠道。
- **只交回一次。** 请求键 `task-result:{run}`：看客收尾与重启补挂各收尾一遍也只受理一条。
- **交回失败时不丢结果**：派它的会话已经不接受输入（删掉了）等情况，退回把结果直接投给
  来源对端，并记 warn。
- **任务会话里不能再派任务**：`GatewayTaskSpawner::parent_of` 看到派它的会话本身是
  `kind = task` 就拒绝，结果只交回一层。
- **接着做**：`task(prompt, task_id)` 把一句话提交进已有的任务会话，它带着自己的历史接着做；
  结果同样交回 home。短号来自派出回执与交回消息。（2026-10-16 之前是单独的 `follow(task_id, text)`。）
- **查进度**（2026-10-19）：`task(task_id)` 不带 `prompt` 只读地返回那个任务最近一条 Run 的快照
  （状态、已跑多久、正在执行的调用及其 `.partial` 输出尾部），不提交任何输入；
  `GatewayTaskSpawner::status` 与 `follow` 共用同一个按短号解析任务会话的 `resolve_task`。
  起因：会话 01a151ec 里用户问"为什么要这么久"，没人答得上任务在做什么。
- **"需要你判断"**（2026-10-11 起没有审批；2026-10-12 起结果不明交给模型，`verify` 只剩旧 Run，
  会产生的介入只有 `blocked`）：消息前面
  挂上 `后台任务 #短号「标题」：`，同时看着几个任务时分得清是哪一个在问。

## 2a. `task`：合并 `dispatch` / `follow` / `delegate`（2026-10-16 定并落地）

同步的 `delegate`（子 Run 在同一 Session 里、父等它）删掉。不阻塞的子 Run 留不在父的 Session
里——同一 Session 严格串行，父不等它，它就排在父后面；挪出去就是任务会话。它原来多出来的
几样并进 `task`：

```text
task(prompt, title, task_id?, model?, effort?, contract?)
```

- **`model` / `effort`**：alias 与档位按当前配置快照在 `prepare` 校验，可选 alias 以 enum 写进
  工具说明（只给 `effort` 时所选模型在派出那一步才知道，档位在 `GatewayTaskSpawner` 里校验）。省略时新任务用 Agent 的默认，接着做沿用那个任务上一条 Run 的。透传给任务 Run 的
  `submit`（冻结进它的快照）；**交回 home 的那条输入不带**，按 home 自己的默认。
- **`contract`**：冻结进任务 Run 的快照（`RunSnapshot.contract`），装配时补在那条 Run 的任务
  后面；`report_to_parent` 交回之前用同一份校验器复验任务的最后回复，复验结论写进交回的输入
  （不合契约也照样交回）。
- **Cron 触发的 Run 不能派**：`prepare` 拒绝——结果交回 Cron 会话时没有对端也没有看客。
- **不是子代理**：同一个 Agent、主 Agent 的装配（记忆、skills 照常），能力面里没有 `task`。
  原来子代理那套（子代理版身份、不召回记忆、`ReplayScope::Thread`、`without_delegate`）只为
  升级时还没跑完的旧子 Run 留着。
- **并入**：交回的输入带插话标记，home 还在跑、且来源对端相同时在一轮收尾后并进来
  （`docs/komo_bot.md` §6），不另开 Run；home 空闲时照旧开一条。被并走的那条的看客见到
  `run.absorbed` 就收工，回复由宿主的看客投。
  带 `task_id`、没有 `contract` 的追问同样以插话受理（2026-10-23），并进任务正在跑的那条 Run，
  结果只由宿主那一次交回；带 `contract` 的追问照旧排队（并入会沿用宿主的契约，新契约会丢）。
- 旧的 `dispatch` / `follow` / `delegate` 调用按落盘时的名字回放，不改写。

## 2b. `context: "fork"`：带着父会话的上下文派任务（2026-10-23 定并落地）

fresh 任务只拿到 `prompt`，父会话里聊清楚的东西要靠模型写进简报，写漏了任务就猜。fork 让任务
从父会话**此刻**的对话接着想，并且首请求复用父会话的提示缓存——前缀与父的上一次请求逐字节
相同，只多出这一轮的结尾。

参照（2026-10-23 读源码与官方文档）：Claude Code 的 fork 子代理（`subagent_type: "fork"`：
系统提示、工具、模型与父完全相同，首请求命中父的缓存，fork 里不能再 fork，只交回最终结果）；
oh-my-pi 的 `/tan`（拷贝父 transcript、沿用父的缓存键、给父在飞的调用补一个中断结果、追加一段
"你只做下面这件事"的说明、清掉继承来的计划）；pi-durable 的 `fork(at)`（历史从哪来与归谁管
分开记，切口处没有结果的调用补错误结果）。

```text
task(prompt, title, task_id?, model?, effort?, contract?, context?)   context: "fresh"（默认）| "fork"
```

- **只用于新开任务。** 带 `task_id` 时给 `context` 在 `prepare` 拒（已有的任务会话带着它自己的
  历史）。fork 时给 `model` / `effort` 也拒：fork 沿用父 Run 的模型与 effort，换了就不是同一个
  缓存前缀。`contract` 照常，补在任务后面。
- **按值冻结前缀，不存指针。** `GatewayTaskSpawner` 派出时按父会话这条 Run **下一次请求**的样子
  装配一遍（`GatewaySegments::fork_view`：父会话每装配一段，就按会话记下这一段的模板；派任务
  只发生在这一段里，所以模板一定在，重启后续跑的那一段装配时先把它补上。拿模板与此刻的日志
  走 `compaction::assemble_context`，与在线压缩判断时的重新装配同一条路，不做新的衰减 / 压缩
  决定），得到系统提示正文与消息序列：截止到含这次 `task` 调用的那条 assistant 消息，连同同轮
  排在它前面、已经收尾的调用结果（编排调用是屏障，§6）。两样（`ForkPrefix`）写进任务会话目录的
  payload，引用与来处（父会话、父 Run、调用号）记在 fork 第一条 Run 的快照上
  （`RunSnapshot.fork: ForkOrigin`，随 `run.accepted` 落账）；先落正文再受理，§8.5。不存"父会话 + seq 上限"
  的指针：父会话可以被删除、purge（`purge_blockers` 不看子会话），父的 payload / `output.json`
  在父的目录里，而一个会话的上下文应当从它自己的目录读得出来（§8.2）。代价是磁盘上多一份，
  上限是父的上下文大小。
- **身份沿用父 Run。** fork 任务 Run 的快照照抄父 Run 的（能力面**含 `task`**、模型、effort、工作
  目录、投影），指向父会话 `payloads/` 的那几格（身份指令、项目规范、skills 目录）一概不带——
  系统提示由冻结的前缀给出，用不上它们，在任务会话的目录里也读不到；记忆模式写成 `off`（不召回
  也不提取：召回的段在父的系统提示里已经有了）。**这个任务会话此后每一段都用冻结的系统提示**；
  之后用 `task_id` 追问受理的 Run 也照抄 fork 那条 Run 的身份（`GatewayState::inherit_fork`），
  只有模型按这次解析——追问可以换模型，只是缓存不中。工具 schema 照常由能力面渲染（与执行
  同源）；派出到首请求之间 `[model.*]` 或 MCP 工具变了，只是这一次缓存不中，不影响对错。
- **开的口子：schema 里有 `task`，派不出去。** 这是"任务会话的 schema 里没有 `task`"
  （`docs/komo_bot.md` §4）唯一的例外——能力面里确实有它，schema 与执行仍同源；拒绝落在派出那一步
  （`GatewayTaskSpawner::parent_of`："这里已经是一个后台任务了，不能再派任务"），调用照常落一条
  失败结果，执行器不回退到全局目录。理由：Anthropic 的请求里工具
  定义在最前面，少一个工具，整段前缀缓存失效，fork 省下的就只剩"不用写简报"。fresh 任务会话照旧
  从能力面里去掉 `task`，没有缓存要保。
- **切口处的调用补结果。** 派出这次 `task` 的调用此刻没有结果，同一条 assistant 消息里排在它
  后面的调用还没计划。冻结前缀时给它们补结果（`tasks::close_open_calls`），只进 fork 的模型视图，
  不进任何账：
  `task` 那次是"这次调用派出的就是你（fork）。父会话在继续它自己的事，你最后的回复会作为这次
  调用的结果交回给它"；排在后面的是"父会话在派出你之后才执行这次调用，你看不到它的结果"。
- **角色说明在输入里，不在系统提示里。** 紧跟补的结果，作为 fork 第一条 Run 的输入开头
  （`tasks::FORK_NOTICE`）：上面是
  父会话的对话，你只做下面这件事；父会话同时还在跑，同一个工作目录里的文件可能正在变，不要接着
  做父会话没做完的事，不要改父正在改的东西；做完就停，最后的回复交回父会话。然后是 `prompt`。
- **缓存标识。** Responses 接口的 `prompt_cache_key` 用父会话的 id（`TurnRequest.cache_session`，
  取自 fork 的来处），否则键不同，前缀相同也路由不到同一份缓存；Anthropic 靠前缀命中，不需要键。
- **往后。** fork 任务会话的回放 = 冻结前缀 → 它自己的各条 Run（规则同 `docs/komo_bot.md` §8.3），
  `komo_agent::context::assemble` 把前缀接在最前、`run_from` 顺延。前缀与别的 Run 的转写同一个
  地位：在线压缩只在正在跑的这条 Run 里切，碰不到它；回放边界（`conversation.boundary`）之后
  fork 那条 Run 不在窗口里，前缀也就不在。用 `task_id` 追问 fork 任务照常。结果交回、并入、查
  进度与 fresh 任务完全相同。
- **局限。** ① 前缀压不掉：父会话已经贴着窗口时派 fork，fork 自己跑几轮就可能超长，而压缩只切
  fork 自己那条 Run——工具说明因此让模型在对话很长时用 fresh。要压，就给前缀单独记一刀（存储
  格式不用改：前缀本来就是一份消息序列）。② 前缀里的 `artifact://` 与 `tool-output/` 引用指向父
  会话，fork 里 `read` 不到（只读根是当前会话的）；要做就把父会话的 `tool-output/` 加进 fork 的
  只读根。③ 前缀里工具结果带的图片不随前缀冻结（`ToolResultForModel.images` 不序列化），fork 只
  看得到文字。
- **不做。** 用户手动分叉会话（Claude Code 的 `--fork-session` / `/branch`、pi 的 `/fork`）是另一件
  事，这里只管模型派任务。

验收（2026-10-23）：① 假 provider 上 fork 首请求的系统提示、工具定义与 home 上一次请求逐字相同，
消息以它为前缀，之后是派它的 assistant 消息、补的结果、说明 + 任务；缓存键是 home；模型沿用 home 的
（`tests/agents` 的 `a_fork_starts_from_the_parents_conversation_and_shares_its_prefix`）；② fork 里
看得见 `task`、派不出去，调用落一条失败结果（同一个测试）；fresh 任务的 schema 里仍没有 `task`
（`a_background_task_reports_back_through_home`）；③ 带 `task_id`，或带 `model` / `effort`，或 context
不认识时被 `prepare` 拒（`tools::task::tests`）；④ 把 home 的会话目录挪走之后，fork 的追问照样带着
前缀跑完（①那个测试的最后一步——purge 删的就是这个目录）；⑤ fork 停在介入上、重启后接着跑，那一段
的请求以重启前那次为逐字节前缀（`a_fork_resumed_after_a_restart_sends_the_same_prefix`）；切口的
补法见 `tasks::tests::the_cut_closes_the_fork_call_and_the_ones_after_it`，装配见 `komo-agent`
`context::tests::a_task_contract_trails_the_task_not_the_system_prompt` 的 fork 段，Responses 缓存键见
`llm::tests::a_fork_routes_its_cache_by_the_parent_session`。⑥ 真机（本机 magpie →
claude-opus-5-5，Responses）：同一组 home / fork 请求依次真发，fork 首请求 4810 个输入 token 里
4016 个读自缓存（83%）。那条路偶尔回 400 `safety filter: content_filter`，同一份请求重发就过，换哪段
文字都会碰到，与 fork 无关。**未做**：Anthropic 原生接口上的核对（本机的 relay 不回报缓存用量）；
komo 在真机上被模型自己选中 fork 的端到端。

## 3. 重启

看客只活在内存里。启动时 `run_watch::reattach_unfinished` 对未终态的 Run 重新挂看客：
来自聊天的（`runs.peer` 非空）照旧，**任务会话里的顶层 Run 即使没有 peer 也挂**（从 TUI /
HTTP 派出去的任务没有渠道对端，但结果仍要交回 home）。委派子 Run（`runs.parent` 非空）不挂。

已知窗口：任务 Run 落了终态、交回 home 之前进程崩溃，这条结果不会被补交——与聊天 Run
"终态之后、投递之前崩溃"是同一个窗口，同样没有补发。

## 4. 顺带修掉的

"需要你判断"（`verify` / `blocked`）重复投递：看客投过一次，周期兜底
（`sweep_unseen_interventions`）没看到"投过"的记号，换成清单里的措辞再投一遍。现在看客投之前
先占 `start_delivering_intervention(run)`，与周期兜底同一个名额。

## 5. 删掉的

`[home]`（`mode` / `dispatcher` / `worker`）及其校验、`freeze_run` 的分发器分流、
`RunSnapshot.dispatcher_tasks_ref`（旧行里的这个字段反序列化时忽略）、任务看板
（`komo-agent::context::tasks`、`context_sources::task_board`）、`#短号` 确定性路由。
配置里残留的 `[home]` 段会被忽略，可以直接删掉。

## 6. 验收

- `tests/agents`：`a_background_task_reports_back_through_home`（派出 → 任务以 home 的 Agent
  跑完 → 结果经 home 转告、任务原话不直接投 → follow 进同一任务、结果再经 home 转告）、
  `home_answers_while_a_task_waits_on_a_person`（任务停在一条等人核对的介入上时 home 照样答话；
  2026-10-11 之前靠审批把任务停住，审批删除后改用测试专用工具 `unsure` 让它停在 verify 上；
  2026-10-12 起结果不明不再停 Run，`unsure` 靠 `TEST_PARK` 前缀停住，生产构建里没有这一支）。
- `tests/recovery`：`reattach::a_background_task_in_flight_across_a_restart_still_reports_back`、
  `verdicts::a_chat_verify_is_delivered_once_even_after_the_sweep`。
- 查进度：`tests/agents` 的 `a_status_query_shows_what_a_running_task_is_doing`、
  `a_status_query_for_an_unknown_task_fails_cleanly`。

## 7. 后续

- 长任务的进度提示（hermes 每 180 s 一句"还在跑"）：**按需查询已做**（§2，`task(task_id)`）；
  主动报进度（任务自己隔一阵说一句）仍未做。
- 交回的那一轮允许模型不说话（hermes 的 NO_REPLY，只对内部输入开放）。
- §3 的已知窗口（终态之后、交回之前崩溃不补交）：同步委派删掉之后，等结果的事都走交回，
  这个窗口变得要紧。交回按请求键幂等，可以在对账里对"终态了、没有对应 `task-result:{run}`
  输入"的任务 Run 补交一次。
