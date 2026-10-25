# 监听：脚本每出一行，就交回会话一条

> 状态：已落地（2026-10-14）；长 `bash` 自动转后台与对话会话禁 `sleep` 等待见 §8（2026-10-19）。与 `docs/komo_bot.md` 冲突处以本文为准。
> 新存储上：监听的登记、工具任务的起停与恢复、结束 / 中断消息的原子交回、重启对账在 runtime（`docs/storage-durable.md` §13.2，`harness/orchestration/monitor.rs`），进程与泵留在 gateway（D5）；§5 里 `runtime/monitors/<id>.json` 属于状态的那部分由 `komo.monitors` 取代。

## 1. 问题

有一类等待不是"等一个结果"，而是"每出一件事就要知道"：MR 流水线里每个 job 的结果、
发布工单的每次状态变化、日志里的每条 panic、灰度期间每次指标越线。已有的三条路都不合适：

- `bash` 里写轮询循环：同步的，这一轮被它占住，中途的事件要等命令退出才一起交回；
- `dispatch`：任务会话跑完才交回一次，中途不报；
- 命令 Cron Job：最细一分钟一次，结果投给 home chat，不回到会话里让模型接着处理。

## 2. 形状（照 Claude Code 的 Monitor）

```text
会话里的模型 ── monitor(command, title, timeout_minutes?, resume_after_restart?) ──▶ "已开始监听 #3f2a：标题"，这一轮收尾
                                        │
                     Gateway 起脚本（bash、数据目录只读挂载，与 bash 同一套）
                                        │ stdout 逐行
                                        ▼
                     泵：连着出的行合成一条，上一条没收尾时攒着
                                        │ submit_interjecting（请求键 monitor:{id}:{n}）
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
- **事件与结束消息都按插话交回（2026-10-21）。** 事件、正常结束消息、重启后的"已中断"
  都走 `submit_interjecting`，与后台命令的完成通知（§8）同一套资格检查：宿主还在跑、
  对端与模型快照对得上时，在下一个工具边界并进去（`absorbed`，终态，泵照样放下一批）；
  否则照常排队。起因是 session `01a1588d` seq 177：「监听已结束」排了 46.44s，等宿主 Run
  收尾才处理；24ebb1a9 当时只改了 `detached.rs`。
- **只有 stdout 的行算事件。** 空行丢掉；一行最多 1000 个字符。stderr 不当事件，结束时
  带最后 20 行。
- **脚本按 `bash` 的规矩起**：`BashTool::child_spec` 给出 bash（没有就 `/bin/sh`）、bwrap
  只读挂载数据目录、明确的环境（`bash::base_environment` + `KOMO_SESSION` +
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

工具说明里写给模型的脚本守则（与 Claude Code 同一套；2026-10-23 起 description 只留先过滤、逐行刷新、失败状态也要打印、终态自己退出这几条最关键的，事件合并、轮询间隔、时限、`resume_after_restart` 等细则在 `tool://monitor/doc`，按需读、不进工具定义，见 `docs/komo_bot.md` §4）：先过滤，只打需要处理的行；管道每一
段逐行刷新（`grep --line-buffered`、awk 里 `fflush()`，不用 `head`）；失败状态也要打印，
只打成功标志的话出事时什么也看不到；有终点的事到终态让脚本自己退出；轮询间隔不短于 15 秒。

## 4. 停止与结束

| 怎么结束的 | 交回什么 | 登记 |
| --- | --- | --- |
| 脚本自己退出 | 结束消息：`exit=N`、剩下的行、stderr 末尾 | 删 |
| 到了时限 | 结束消息：超时 | 删 |
| 刷屏 / 事件到上限 / 交不回会话 | 结束消息：原因 | 删 |
| 模型调 `monitor_stop` | 不交回（它自己知道） | 删 |
| Gateway 停机 | 不交回 | **留着**，下一代启动时交回"中断了"，或（声明了 `resume_after_restart`）重挂 |

`monitor_stop(monitor_id)` 只在挂它的那个会话里找；短号对不上时列出这个会话还在跑的监听，
撞号时列出 6 位候选。

**停一个已经结束的不算失败（2026-10-21）。** 处理事件的 Run 常常在脚本自己退出之后才调
`monitor_stop`：session `01a153c3` seq 3578，#cd64 05:55:53 已退出（exit=0），05:56:33 的 `monitor_stop`
得到「停不了：#cd64 不在跑」；#24e5 同样撞上，模型据此误报「监听没报结果就没了」。现在进程收尾时
（交结束消息之前）在 `Live.finished` 记下说法，摘表时挪进“刚结束”表（这一代 Gateway 内存里，最多 64 个）；
`monitor_stop` 对上它就成功返回「不用停了：监听 #cd64「…」已结束：脚本退出（exit=0）；结束消息会单独
交回会话（可能已经到了）」，再停一个自己停过的答「已经停掉了」。只认同一个会话；重启之后不认。
转后台的 `bash` 走同一套。

## 5. 重启

**监听默认不跨重启。** 进程只活在这一代 Gateway 里：

- 停机（`Running::stop`）先终止所有监听的进程组（`run_child` 的 TERM → KILL），登记
  `runtime/monitors/<id>.json` 留着。
- 启动（`service::start` 第 8d 步，调度器起来之前）：`runtime/monitors/children/<id>.json`
  里的遗留进程组（Gateway 崩了没来得及停）**核对身份后**终止——pid 会复用，按
  `ps -o etime=` 换算的起来时刻与登记差 10 秒以内才算同一个，对不上宁可留着也不误杀；
  然后每份登记给它的会话交回一条"已中断：Gateway 重启了，脚本没有在跑，也不会自动重挂"，
  登记删掉。声明了 `resume_after_restart` 的例外，见下。
- 在册不进执行器的 `runtime/children/`：那里一行活着的进程会让恢复扫描把上一代的所有 Run
  停下来（§8.7），而监听与任何一条 Run 都无关。

**续跑的 `monitor` 调用不重新起进程。** 上一世已经 `start_call` 过、结果没落盘就崩了的那次
调用，这一世回来收口时（`fresh = false`）只认同一个请求键 `monitor:{run}:{call}` 下还在跑
的那个；认不到就答"上一次挂的监听随 Gateway 重启中断了，没有自动重挂"——任意命令不自动
重跑（§8.6）。

**可重挂的监听（2026-10-14）。** 模型挂的时候声明 `resume_after_restart = true`（工具说明：
脚本只读、重跑一遍没有副作用——轮询状态、查进度、tail 日志），登记里就多一格 `resume`：
挂它那次调用的请求键、挂它的 Run、工作目录、时限与截止时刻。下一代启动时（同样在调度器之前、
收完遗留进程组之后）：

- 截止前至少还剩 1 分钟：**同一个 id、同一个短号、同一个请求键**重新起脚本，时限只给剩下的；
  不额外交回消息（一条消息就是一轮模型），只在重挂后的第一条事件末尾注明"中间 Gateway 重启过，
  断开那几秒里脚本打的行没有收到"。事件的请求键多一段这一代独有的 UUIDv7
  （`monitor:{id}:{代}:{n}`），否则重挂后从 1 数起，撞上上一代交过的 `monitor:{id}:1` 被吞掉。
  续跑的 `monitor` 调用按请求键认回它，答的是同一个句柄。
- 已经过了截止（或剩不到 1 分钟）、或重挂不上（工作目录没了）：照不可重挂的那样交回结束消息。
- 2026-10-14 之前的登记没有 `resume`，一律按不重挂收；`Operation::Monitor` 的
  `resume_after_restart` 为 `false` 时不进 JSON，旧计划的哈希不变。

为什么要它：实测（10-14 会话 `01a13911`）盯一次 stg 刷数的 40 分钟里，Gateway 因为本地重装
komo 重启了三次，监听断了三次，每次都要模型多跑一整轮（读中断消息、查一遍现状、重新挂），
而那个脚本只是每两分钟查一次库里的进度。为什么不默认开：任意命令不自动重跑（§8.6），
"这个脚本重跑无害"只有写它的模型知道，与 `Tool::replay` 默认 `Unsafe` 同一个道理。

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
- `a_resumable_monitor_is_relaunched_after_a_restart_under_the_same_short_id`：声明了
  `resume_after_restart` 的，重启后脚本重新跑起来、短号不变、不交回"中断了"；重启前后各一条
  事件都交回了，后一条注明断过。
- `a_resumable_monitor_past_its_deadline_is_not_relaunched`：截止时刻已过的不重起，交回
  "时限已经到了"。
- `a_monitor_event_interjects_into_the_still_running_host`：宿主挂完监听后还在跑，事件在下一个
  工具边界以 `absorbed` 并进原 Run，宿主读得到它。换回 `submit` 时测试失败。

单元测试：`service::monitors`（切行、合批、结束原因、`etime` 解析、正文）、
`tools::monitor`（计划、时限收紧、空参数、`resume_after_restart` 默认关且旧计划字节不变）、`types::monitor`（短号、时限）。

**未验证**：Linux 上 bwrap 包着的脚本（`--die-with-parent` 让崩溃时脚本随 Gateway 一起没，
遗留进程组那一支在 Linux 上走不到）与 systemd 停机时的进程组回收；以上测试都在 macOS 上跑。

## 8. 长 `bash` 自动转后台（2026-10-19）

**问题（实测，本机 2862 次 bash 调用）：** 224 次跑过 60 s，45 次过 300 s，18 次过 600 s（最长 960 s），
全在 TUI / home 这类对话会话里，多半是等 CI / 工单的轮询循环和 release 构建；其中 65 次命令里带
`sleep`，跑过 60 s 的 224 次里 35 次带 `sleep`。会话严格串行，用户要等十几分钟才说得上话。

**形状：** 转后台就是一个**不发行事件、只在结束时交回一条消息**的监听。

```text
对话会话里的 bash ── 跑过 [execution] detach_after_secs（默认 120，0 = 不转）还没完
   │ 这次调用立刻结算：「已转后台 #3f2a」+ 到现在的输出末尾（状态 completed，结果体 `detached: <id>`）
   ▼
活子进程（管道、进程组、已收到的字节）交给 Gateway 的 `GatewayMonitors`（runtime 的 `Detacher` 槽，
   runtime 不依赖 gateway）；同一个 id 空间，`monitor_stop #3f2a` 停得掉它
   │ 退出 / 超时 / 被停
   ▼
内部输入 `bash-done:{call_id}`：`[后台命令 #3f2a「命令首行」结束] 退出码 N，用时 …` + 输出末尾
（开头 10 + 最新 30 行，stderr 另带末 20 行）+ `artifact://…/background`
```

- **只对对话会话。** 任务会话（`kind = task`）、Cron 的 Run、子 Run、codemode 里的内层调用都不转
  （`ToolContext.detach_after = None`）；配置为 0 也不转。`CallEnv.detach_after` 由 `GatewaySegments`
  按当前快照填，热生效，与 `call_timeout_secs` 同一条路。阈值不早于这次调用的时限时不转。
- **调用已结算。** 转后台那一刻调用就有结果（不是 started 而无结果），所以重启恢复不会把它判成
  `uncertain`、更不会重跑。没有新增状态值。
- **用户插话提前转后台（2026-10-20）。** 阈值之前用户在 TUI 里插话（`interject: true`、不是重发命中），
  Gateway 举起这个对话会话当前一段的 `InterjectSignal`；前台 `bash` 跑满 3 秒宽限后看到它就转后台，
  回执写「用户插话了」，于是这一轮马上收尾、插话在下一次请求模型前并进去。起因是 session `01a15358`
  seq 973→976：插话等前台 CI 轮询跑完才被吸收，等了 240.685s。规则：信号每段装配换新，只有对话会话
  登记（任务会话、Cron、子 Run 不顶掉宿主那一份）；一次插话只换一次转后台（`take`）；没给阈值的调用
  （`0`、非对话）不转；受理成的那条 Run 自己已经开跑时不举。只有 HTTP 插话举旗——`bash-done:` /
  `task-result:` 这类交回不打断前台命令。
- **完成通知能插话（2026-10-20 修正）。** `watch_detached` 用 `submit_interjecting` 交回
  `bash-done:{call_id}`，沿用已有的模型快照/对端资格检查：宿主仍在对话时，在下一次工具轮次
  收尾的边界合入并重新装配；没有合适宿主时照常排队。不打断已经在飞的模型请求或工具，最终
  回答没有工具边界时也仍排在后面。起因是 session `01a157e3`：#8b08 在 14:50:03 已结束，
  普通通知却排到 14:52:34 才跑，宿主期间仍拿旧回执调用 monitor_stop。新回归
  `a_detached_completion_interjects_into_the_still_running_host` 用可控子进程复现，确认完成通知
  以 `absorbed` 合入原 Run。完成与停止仍可能竞态；竞态时 `monitor_stop` 答「不用停了：…已结束」（见 §4）。
- **输出。** `stdout.txt` / `stderr.txt` 在结算时已按哈希封口，不能再追加；转后台之后的输出（两路按到达
  顺序混写）进同一尝试目录的 `background.log`，入口 `artifact://<run>/<call>/<attempt>/background`
  （`OutputFile::Background`）。转后台之前的输出在那次调用的结果里。
- **时限。** 原 `timeout_secs`（受 `call_timeout_secs` 收紧）从调用开始算，到点在后台终止进程组，结束消息
  报「超时」。
- **结束消息**与监听同一条投递路径（`submit_interjecting`，对端取挂它那条 Run 的 `runs.peer`）。`monitor_stop` 停掉它
  时与监听不同：**会**交回一条「已被 monitor_stop 停止」（监听是模型自己停的、不再交回）。
- **重启。** 子进程随 Gateway 停机终止，登记 `runtime/monitors/<id>.json`（带 `detached`）留着；子进程
  另在 `runtime/monitors/children/` 在册，崩溃遗留的进程组按监听同一套核对身份后终止。下一代启动给会话
  交回「Gateway 重启，后台命令中断，结果不明」+ `background.log` 里已有的末尾，登记删掉。不重跑。
- **记忆。** `monitor:` / `monitor-end:` / `bash-done:` 与 `task-result:` 一样渲染为「任务交回」，不能引为
  `user_statement`（`docs/memory.md` §9.10）。

**对话会话里禁 `sleep` 等待。** `bash` 的 `prepare` 在对话会话里拒绝命令中任何一个字面量、不短于 10 秒的
`sleep`（`sleep 20`、`sleep 1m`、`sleep 0.5h`，含循环体里的；`sleep $N` 这类非字面量不管，短 sleep 放行），
提示改挂 `monitor`（脚本只在完成或出错时打一行，间隔不短于 15 秒）并先回复用户。任务会话与 Cron 的 Run
照常能 sleep（任务会话用不了 `monitor`）。判断是词法的，引号里的 `sleep 20` 也会被拒——代价是偶尔多问一次，
换来不用解析 shell。

验收：`crates/komo-gateway/tests/detach/main.rs`（真数据目录、真子进程）——转后台立刻返回且进程跑完后交回
退出码 / 尾部 / 引用、`0` 不转、后台超时终止进程组、`monitor_stop` 停掉并说明、重启交回「结果不明」且原 Run
仍 Completed、对话里长 sleep 被拒；单元：`tools::bash`（阈值、时限从调用开始算、无接手方不转、禁 sleep 的
会话范围）、`tools::detach`（sleep 解析）、`service::segment`（谁算对话会话）、`memory::tests`。
