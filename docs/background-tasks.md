# 后台任务：结果交回 home

> 状态：已落地（2026-10-08）。取代 `docs/home-dispatcher.md` 那套"home 改成分发器"的设计；
> 与 `docs/komo_bot.md` 冲突处以本文为准。

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
                       │ dispatch(task, title) → 回"已派出 #2a9c：标题"，这一轮收尾
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
  `dispatch` 的工具说明判断（几秒能做完的直接做）。
- **结果交回 home。** 任务会话的 Run 进终态时，`service::run_watch::finish` 调
  `service::tasks::report_to_parent`：任务的最终回复（或失败 / 取消的原因）拼成一条以
  `[后台任务 #短号「标题」…]` 开头的输入，`submit` 进派它的会话，peer 是任务 Run 的来源对端。
  home 那一轮的回复照常投回来源。任务自己的最终回复**不再**直接投给渠道。
- **只交回一次。** 请求键 `task-result:{run}`：看客收尾与重启补挂各收尾一遍也只受理一条。
- **交回失败时不丢结果**：派它的会话已经不接受输入（删掉了）等情况，退回把结果直接投给
  来源对端，并记 warn。
- **任务会话里不能再派任务**：`GatewayTaskSpawner::parent_of` 看到派它的会话本身是
  `kind = task` 就拒绝，结果只交回一层。
- **follow**：`follow(task_id, text)` 把一句话提交进已有的任务会话，它带着自己的历史接着做；
  结果同样交回 home。短号来自派出回执与交回消息。
- **审批与"需要你判断"**：任务里的审批照 §11.4 投来源 + home chat；"需要你判断"的消息前面
  挂上 `后台任务 #短号「标题」：`，同时看着几个任务时分得清是哪一个在问。

同步的 `delegate`（子 Run 在同一 Session 里、父等它）不变：那是"这件事我自己等着"，
`dispatch` 是"另起一个会话去跑，我不等它"。

## 3. 重启

看客只活在内存里。启动时 `run_watch::reattach_unfinished` 对未终态的 Run 重新挂看客：
来自聊天的（`runs.peer` 非空）照旧，**任务会话里的顶层 Run 即使没有 peer 也挂**（从 TUI /
HTTP 派出去的任务没有渠道对端，但结果仍要交回 home）。委派子 Run（`runs.parent` 非空）不挂。

已知窗口：任务 Run 落了终态、交回 home 之前进程崩溃，这条结果不会被补交——与聊天 Run
"终态之后、投递之前崩溃"是同一个窗口，同样没有补发。

## 4. 顺带修掉的

"需要你判断"（`verify` / `blocked`）重复投递：看客投过一次，周期兜底
（`sweep_unseen_interventions`）没看到"投过"的记号，换成清单里的措辞再投一遍。现在看客投之前
先占 `start_delivering_intervention(run)`，与审批同一个名额。

## 5. 删掉的

`[home]`（`mode` / `dispatcher` / `worker`）及其校验、`freeze_run` 的分发器分流、
`RunSnapshot.dispatcher_tasks_ref`（旧行里的这个字段反序列化时忽略）、任务看板
（`komo-agent::context::tasks`、`context_sources::task_board`）、`#短号` 确定性路由。
配置里残留的 `[home]` 段会被忽略，可以直接删掉。

## 6. 验收

- `tests/agents`：`a_background_task_reports_back_through_home`（派出 → 任务以 home 的 Agent
  跑完 → 结果经 home 转告、任务原话不直接投 → follow 进同一任务、结果再经 home 转告）、
  `home_answers_while_a_task_waits_on_approval`。
- `tests/recovery`：`reattach::a_background_task_in_flight_across_a_restart_still_reports_back`、
  `verdicts::a_chat_verify_is_delivered_once_even_after_the_sweep`。

## 7. 后续

- 长任务的进度提示（hermes 每 180 s 一句"还在跑"）。
- 交回的那一轮允许模型不说话（hermes 的 NO_REPLY，只对内部输入开放）。
- 审批卡片也带上 `#短号「标题」`。
