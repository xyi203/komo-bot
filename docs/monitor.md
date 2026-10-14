# 监听：脚本每出一行，就交回会话一条

> 状态：已落地（2026-10-14）。与 `docs/komo_bot.md` 冲突处以本文为准。

## 1. 问题

有一类等待不是"等一个结果"，而是"每出一件事就要知道"：MR 流水线里每个 job 的结果、
发布工单的每次状态变化、日志里的每条 panic、灰度期间每次指标越线。已有的三条路都不合适：

- `shell` 里写轮询循环：同步的，这一轮被它占住，中途的事件要等命令退出才一起交回；
- `dispatch`：任务会话跑完才交回一次，中途不报；
- 命令 Cron Job：最细一分钟一次，结果投给 home chat，不回到会话里让模型接着处理。

## 2. 形状（照 Claude Code 的 Monitor）

```text
会话里的模型 ── monitor(command, title, timeout_minutes?) ──▶ "已开始监听 #3f2a：标题"，这一轮收尾
                                        │
                     Gateway 起脚本（bash、数据目录只读挂载，与 shell 同一套）
                                        │ stdout 逐行
                                        ▼
                     泵：连着出的行合成一条，上一条没收尾时攒着
                                        │ submit（请求键 monitor:{id}:{n}）
                                        ▼
                     同一个会话："[监听 #3f2a「标题」]\n<行>…" → 模型跑一轮，需要的话转告用户
                                        │ 脚本退出 / 超时 / 刷屏
                                        ▼
                     结束消息（请求键 monitor-end:{id}）：原因、最后的行、stderr 末尾
```

- **两个工具，都是编排操作**（`Operation::Monitor` / `Operation::MonitorStop`）：工具只做
  `prepare`，起进程与交回在 Gateway 的 `service::monitors::GatewayMonitors`
  （kernel 的 `Monitors` 接缝，装配与 `TaskSpawner` 同一个接法）。codemode 脚本里调不了。
- **事件交回挂它的会话，投给挂它那条 Run 的来源对端**（`runs.peer`）。在 home 里挂的，
  事件经 home 的模型转告到聊天；在 TUI 里挂的，事件在事件流里看得到。
- **只有 stdout 的行算事件。** 空行丢掉；一行最多 1000 个字符。stderr 不当事件，结束时
  带最后 20 行。
- **脚本按 `shell` 的规矩起**：`ShellTool::child_spec` 给出 bash（没有就 `/bin/sh`）、bwrap
  只读挂载数据目录、明确的环境（`shell::base_environment` + `KOMO_SESSION` +
  `KOMO_MONITOR`，不带 `KOMO_RUN`——它不属于任何一条 Run）。

## 3. 合批与背压

每条事件都是一条 Run、一轮模型，所以合批是成本问题，不是美观问题：

| 规则 | 值 | 理由 |
| --- | --- | --- |
| 第一行之后再等多久 | 1 秒 | 一次连着打出来的几行合成一条 |
| 同一个监听同时在飞的事件 | 1 条 | 上一条没收尾（终态，或停在要人的介入上）时新行攒着，合进下一条；脚本再吵，排进会话的也只有一条 |
| 一条事件最多带几行 | 40（开头 10 + 最新 30） | 中间写明"省略 N 行" |
| 一条事件攒了多少行算刷屏 | 2000 | 脚本没过滤：停掉，结束消息说明原因 |
| 一个监听最多交回几条事件 | 200 | 每条一轮模型，到上限停掉 |
| 在飞的事件等多久不再等 | 10 分钟 | 下一条照样排在它后面（同会话严格串行） |
| 时限 | 默认 30 分钟，上限 24 小时 | Gateway 是常驻的，流水线常常超过 30 分钟；到点终止进程组并交回"超时" |

工具说明里写给模型的脚本守则（与 Claude Code 同一套）：先过滤，只打需要处理的行；管道每一
段逐行刷新（`grep --line-buffered`、awk 里 `fflush()`，不用 `head`）；失败状态也要打印，
只打成功标志的话出事时什么也看不到；有终点的事到终态让脚本自己退出；轮询间隔不短于 15 秒。

## 4. 停止与结束

| 怎么结束的 | 交回什么 | 登记 |
| --- | --- | --- |
| 脚本自己退出 | 结束消息：`exit=N`、剩下的行、stderr 末尾 | 删 |
| 到了时限 | 结束消息：超时 | 删 |
| 刷屏 / 事件到上限 / 交不回会话 | 结束消息：原因 | 删 |
| 模型调 `monitor_stop` | 不交回（它自己知道） | 删 |
| Gateway 停机 | 不交回 | **留着**，下一代启动时交回"中断了" |

`monitor_stop(monitor_id)` 只在挂它的那个会话里找；短号对不上时列出这个会话还在跑的监听，
撞号时列出 6 位候选。

## 5. 重启

**监听不跨重启。** 进程只活在这一代 Gateway 里：

- 停机（`Running::stop`）先终止所有监听的进程组（`run_child` 的 TERM → KILL），登记
  `runtime/monitors/<id>.json` 留着。
- 启动（`service::start` 第 8d 步，调度器起来之前）：`runtime/monitors/children/<id>.json`
  里的遗留进程组（Gateway 崩了没来得及停）**核对身份后**终止——pid 会复用，按
  `ps -o etime=` 换算的起来时刻与登记差 10 秒以内才算同一个，对不上宁可留着也不误杀；
  然后每份登记给它的会话交回一条"已中断：Gateway 重启了，脚本没有在跑，也不会自动重挂"，
  登记删掉。
- 在册不进执行器的 `runtime/children/`：那里一行活着的进程会让恢复扫描把上一代的所有 Run
  停下来（§8.7），而监听与任何一条 Run 都无关。

**续跑的 `monitor` 调用不重新起进程。** 上一世已经 `start_call` 过、结果没落盘就崩了的那次
调用，这一世回来收口时（`fresh = false`）只认同一个请求键 `monitor:{run}:{call}` 下还在跑
的那个；认不到就答"上一次挂的监听随 Gateway 重启中断了，没有自动重挂"——任意命令不自动
重跑（§8.6）。

## 6. 没做的

- 没有 `komo monitor list|stop` 命令与 HTTP 接口：现在只有模型能停，操作者要停就让模型停，
  或者重启 Gateway。
- 事件 Run 的回复照常投给来源对端：模型没法"这条不用说"。要少打扰，靠脚本过滤。
- 没有 WebSocket 模式。要订阅推送流，脚本自己连（`websocat` 之类），每帧打一行。

## 7. 验收

`crates/komo-gateway/tests/monitor/main.rs`（真数据目录、真子进程、脚本模型）：

- `lines_come_back_as_inputs_and_the_exit_is_reported`：行作为 `[监听 #短号「标题」]`
  输入交回同一个会话；退出码与 stderr 末尾进结束消息，stderr 不当事件；登记删掉。
- `lines_printed_while_an_event_is_being_handled_are_merged_into_the_next`：6 秒 30 行、
  每条事件处理 3 秒，事件 Run 不超过 3 条，30 行一行不丢。去掉"同时只有一条在飞"时是 5 条，
  测试失败。
- `stopping_kills_the_script_and_reports_nothing_more`：短号给错时列出在跑的；停掉之后进程组
  没了，不再交回结束消息。
- `a_restart_interrupts_the_monitor_and_tells_the_session`：停机终止脚本、登记留着；重启后
  会话收到"已中断"，脚本没有被重新起来。

单元测试：`service::monitors`（切行、合批、结束原因、`etime` 解析、正文）、
`tools::monitor`（计划、时限收紧、空参数）、`types::monitor`（短号、时限）。

**未验证**：Linux 上 bwrap 包着的脚本（`--die-with-parent` 让崩溃时脚本随 Gateway 一起没，
遗留进程组那一支在 Linux 上走不到）与 systemd 停机时的进程组回收；以上测试都在 macOS 上跑。
