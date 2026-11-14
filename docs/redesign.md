# 重设计：对话、目标、记忆；komo 当作服务

> 状态：**已批准，实施中**（操作者 2026-11-13 逐条拍板，决定见 §0）。本文取代 `docs/tasks.md` 的 §4（Router - Worker）与 §4.13（群聊），
> 并改写 `docs/web.md`、`docs/goals.md`、`docs/clients.md`、`docs/komo_bot.md` §11 里与之冲突的部分；这些文档在对应的改动落地时逐个写回，
> 写回之前**以本文为准**。原型在 `.scratch/home-redesign/index.html`（不进仓库）。
> 标「待定」的是没有明确拍板的，标「未核实」的是定方案时没有读实现代码确认的——动到那一块时先核实，把结论写回这里。

## 0. 已定的产品决定

| # | 决定 |
|---|---|
| P1 | 主页是一条主对话，另有可选的支线对话；对话里只有话，复制 / 看过程 / 用上的记忆悬停才出现 |
| P2 | 图标栏只有：对话、搜索、目标、记忆，底部设置。去掉「正在做的事」「自动化（定时）」「待处理」 |
| P3 | 等你的事：对话图标上的数字 + 对话列表行上的黄点；Komo 的提问就是对话里的一句话，直接回 |
| P4 | 搜索是浮在当前页上的弹窗（⌘K） |
| P5 | 「看过程」在对话右侧滑出执行记录，不换页 |
| P6 | 目标页只显示「追踪中」，其余收在「…」后面；新建与修改走对话 |
| P7 | 没有终点的目标也是目标（不能完成，只能不再追踪） |
| P8 | 自动化并进目标：定时是目标的一项配置，一个目标至多一个；监听列在目标下；存量定时任务各成一个目标 |
| P9 | 设置是弹窗，四节：通用、模型、连接（工具 / 渠道 / 技能）、关于 |
| P10 | MCP 可整台关闭，工具可逐个开关 |
| P11 | 去掉群聊：网页不显示，服务端也不再支持 |
| P12 | 不重做存储，只调目标与定时这一层 |
| P14 | 停住的运行在它所在的对话里问：在支线对话里的就在那段支线里发消息，在主对话里的就在主对话里发；没有单独的待处理页。目标的定时运行停住时发到主对话 |
| P15 | 模型选择：主对话不放（要换去设置改默认模型）；支线对话收进输入框的加号菜单，上下文占用也在那里，快满时才在输入框旁提示 |
| P16 | 目标的定时运行停住：消息发到主对话（目标绑定了支线时发到那段支线，P22），同时在该目标的活动里标成「在等你」，点了跳到那句话 |
| P17 | 去掉全部聊天渠道（微信、飞书、Telegram）：网页的设置里不再有「渠道」，服务端直接删代码，不留停用的版本（§2.6） |
| P18 | 去掉 Router：主对话就是一段普通的对话，和支线同一种东西；特别之处只是它一直在，目标的定时结果与提问发到这里。主对话一次只做一件事，要并行就开支线 |
| P19 | komo 当作一个服务来做：服务端与网页分开部署；只给一个人用，部署在自己的服务器上（§2.9）。「本地优先」的定位随之改写 |
| P20 | 服务一直跑在远程机器上（现在就是一台远程虚拟机）：命令都在那台机器上执行，不做本机执行端，也不再考虑「装在自己电脑上」的用法 |
| P21 | 登录接 OIDC（自己搭的 Pocket ID），不自己做账号密码；配对码去掉 |
| P22 | 目标可以和一段支线对话绑定（一对一）：绑定后这个目标的定时结果与提问发到那段支线，不发主对话；没绑定的仍发主对话（P16）。只能在目标详情里新开一段支线来绑定，不做「绑到已有支线」和「在支线里选目标」 |
| P13 | 支线对话是用户直接开的会话（`kind = normal`），消息直通、不经 Router。改回 `docs/tasks.md`「不再有人直接开的会话」 |

## 1. 网页客户端（`apps/client`）

### 1.1 外壳与路由

- [x] 外壳从「280 侧栏 + 主区 + 右栏」改成「64 图标栏 + 264 对话列 + 主区」；非对话页没有对话列（2b-1，2026-11-14；窄屏 ≤900 先只收起对话列，5d 再做）
- [x] `/` 直接是主对话，不再跳转到 `/sessions/$id`（2b-1）
- [x] 支线对话的路由 `/chats/$id`（另有 `/chats/new`，新支线，发第一句话才建会话）；`/sessions/$sessionId` 保留为跳转（主会话跳 `/`，其余跳 `/chats/$id`），存量链接仍能进（2b-1）
- [ ] `/matters[/$id]` 改成 `/goals[/$id]`；删 `/cron[/$id]`、`/inbox[/$id]`（`/inbox[/$id]` 2b-1 已删，其余 3b 做；图标栏的目标现在仍链到 `/matters`）
- [ ] 图标集重画（对话、搜索、目标、记忆、设置、复制等），`src/components/icon.tsx`

### 1.2 对话

- [x] ~~事件 fold 认主线回复（`LineReply`）~~ 不需要了（P18）：主对话是普通会话，沿用现有的按 Run 渲染。只需认「目标的定时结果 / 提问投到主对话」这一种不属于当前 Run 的消息
  - 2b-1 补：服务端投一句话用的正是 `LineReply`（`line.reply`），`foldEvents` 现在把它排成一条普通的 Komo 消息；`data.source == intervention` 的答复按钮挂在最后一条投进来的话下面（事件负载里没有 `handle`，按「指向这段对话的未答复介入」配，见 `docs/web.md` §4）。要一一对应得让服务端把 `handle` 带进 `line.reply`。
- [x] 对话列：主对话固定在上，支线在下，在等你的排前；每行标题、最后一句、时间、黄点 / 橙点（2b-1，`GET /v1/chats`）
- [x] 新支线对话：加号新建，第一句话成为标题（2b-1：`/chats/new` 不预先建会话，发第一句时 `POST /v1/sessions` 再提交）
- [x] 顶部居中的头像与名字；去掉顶栏的标题、对话 / 轨迹切换、改动按钮、分叉、右栏开关（2b-1；改动与轨迹的入口随之暂无，2b-2 的过程面板接回）
- [x] 回复下的悬停工具：复制（图标）、看过程、用上了 N 条记忆（2b-2；投进对话的话只有复制）
- [x] 「正在输入」指示，取代轮次抬头与过程组（2b-2；旁边带停止按钮）
- [x] 输入框只留附件、文字、发送。支线对话的加号菜单里放模型、effort 与上下文占用（P15）；主对话的加号菜单只有附件（2b-2；主对话的加号直接开选文件框，不弹菜单；上下文到 80% 在输入框旁出小提示）
- [x] 过程面板：步骤、改动、轨迹三个标签；改动的文件对比与轨迹沿用现有实现（`features/changes`、`features/trace`），嵌进面板（2b-2，`features/process/`；改动卡片与调用的"查看"也接回了）
- [x] 删：`features/sessions/`（列表、视图菜单、会话菜单、分组排序筛选）、`features/panel/`（右栏各段）、`features/session/session-title.tsx`、`features/inbox/`（2b-1；`features/sessions/` 里留了 `title.ts`、`model.ts`、`actions.ts` 三个小文件，`context-usage.ts` 移到 `features/composer/`；2b-2 删了 `actions.ts`，`model.ts` 只留 `isArchived` / `isLive`（对话页用），`title.ts` 还被记忆 / 事项 / 定时页与对话页引用，随 3b 去掉）

- [x] 支线的改名与归档：对话列行悬停的「…」菜单与支线页右上角的同一个菜单，改名调 `POST /v1/sessions/{id}/name`，归档调 `POST /v1/sessions/{id}/delete`（软删除，先确认；归档当前这段回 `/`）（2c，`features/chats/chat-menu.tsx`）
- [x] 空状态与连接 / 出错提示：主对话、支线、无支线、搜索无结果各有一句话；连不上或事件流断开时输入框上方一行提示（2c，见 `docs/web.md` §7）
- [x] 已知竞态：点加号后立刻打字回车会落进旧对话——路由切换 `pending` 期间旧输入框置灰（2c；比取消懒加载更通用，换任何页都生效）

### 1.3 搜索

- [x] `features/palette/` 改成原型的样式：没输入列最近对话；输入后分「对话」「目标」两组；去掉类型标签排与快捷键说明行（2b-2；对话走 `GET /v1/session-search`，目标在前端按标题过滤）
- [ ] 记忆要不要进搜索结果：待定

### 1.4 目标

- [ ] 列表：一节「追踪中」；有终点的是勾选框 + 截止，没有终点的是循环箭头 + 节奏；「…」展开「不再追踪」
- [ ] 详情：说明、动作、定时（一行配置 + 开关）、监听、活动时间线、为它记的记忆
- [ ] 定时弹层：三行配置、运行历史、暂停 / 改时间 / 去掉定时。**三行配置能否在弹层里直接改：待定**
- [ ] 「定一个目标」入口：点了去主对话并预填一句话
- [ ] 目标与支线绑定（P22）：目标详情里一行「对话」——已绑定的点了进那段支线，没绑定的可以「绑定一段支线」（新开一段，标题取目标的标题）；支线对话页右上角显示它绑定的目标，点了回目标详情；「和 Komo 聊这件事」在已绑定时直接进那段支线
- [ ] 删：`features/matters/` 的表单（新建、各种改字段的小表单）、`features/cron/`

### 1.5 记忆

- [ ] 换成新版式：一句概况、点阵、等你过目（记住 / 不用记）、每次都带、相关才带、记一条（走对话）
- [ ] 有新记下、等你过目的记忆时，图标栏的记忆图标带一个点；过目完就消失
- [ ] 详情页、检索框、作用域筛选原型里没画：待定是否保留

### 1.6 设置

- [ ] 弹窗重排成四节，左侧带搜索
- [ ] 模型：默认模型、主对话的模型、记忆的模型、向量模型；提供商列表；编辑 / 添加改成右侧整块换页
- [ ] 连接：工具 / 技能两张表（渠道已去掉，P17）；工具行可展开逐个开关
- [ ] 关于：账号与登录状态（P19 之后取代「Gateway 地址可改」）、配置检查、环境变量入口
- [ ] 从界面拿掉：工具结果上限、各种时限、检索方式、重建索引、Gateway 的启动时间 / 数据目录 / 遥测。**重建索引要不要留：待定**
- [ ] 环境变量的管理界面原型里只有一个入口按钮，没画

### 1.7 没有落点、需要决定的

- [x] 后台在做、不在等你的事不另做入口：这类事要么是目标的定时运行（在目标里看），要么在主对话里（看「正在输入」）

- [x] 待处理页去掉后停住的运行在哪答复：已定（P14），在它所在的对话里用一句话问，你直接回。原型里对外动作的确认已是这样，「结果不明 / 前提没了」两类照同一个办法（2b-1 已接：投进来的话下面按 `verdicts` 给答复按钮）
- [ ] 窄屏与手机：原型只做了最粗的收起
- [x] 英文文案：原型只有中文（2c：`messages/en.json` 里重设计新增与改过的键按英文习惯重写，不逐字直译，中文口吻一并理顺；删了确认没人用的 `gateway_change`、`matter_col_matter`）
- [ ] `claude/komo-web-redesign-61bc08` 分支上未合并的视觉重做，与这次改动撞在外壳和对话页，要先定基线

### 1.8 手机（建议：同一套网页做成 PWA，暂不做原生应用）

- [ ] 布局：手机上一次只显示一栏。对话列表是一屏，点进去是对话全屏；图标栏变成底部的标签栏；过程面板从底部升起；设置弹窗铺满全屏；目标与记忆页本来就是单列
- [ ] 可安装：清单文件、图标、独立窗口显示；添加到主屏幕后像应用一样打开
- [ ] 登录：和网页同一套 OIDC，在手机浏览器里用通行密钥。iOS 上「添加到主屏幕」的应用与 Safari 的登录状态是分开的，要在应用里再登一次；这个流程在 iOS 上是否顺畅未验证
- [ ] 推送：Komo 找你时（问你一句、定时的结果）发系统通知。浏览器推送需要：网页的后台脚本、服务端存每台设备的订阅并负责发送。这是删掉聊天渠道之后「主动找到你」的唯一途径，服务端要新做
- [ ] 回到前台时重连事件流并补上离开期间的消息（重连已有，补消息的行为未核实）
- [ ] 以后不够用再套原生壳（`docs/clients.md` D8 的 Capacitor）：登录照桌面端的做法，系统浏览器加跳回应用

## 2. 服务端（Gateway 与下层）

### 2.1 对话

- [x] 支线对话：用户直接开的会话（`kind = normal`），消息直通、不经 Router（P13，已定）。`POST /v1/sessions` 的入口保留（建出来的带 `source = web`）；`docs/tasks.md` §0、§3.1 里「入口去掉」的说法已改（2026-11-14）
- [x] 对话列表接口 `GET /v1/chats`：主对话 + 支线，每行带最后一句话、时间、是否在等你、是否在跑，另给等你的总数（2026-11-14，做法记录见下）
- [x] 会话记录补「最后一句话的时间」`last_message_at`（现在 `updated_at` 不随消息更新；`/v1/sessions` 的 `updated_at` 没改，对话列表读新的一格）
- [x] ~~任务带上来源~~ 不需要了（P18）：「正在输入」就是这段对话当前的 Run，过程面板看的也是它
- [x] ~~网页往主对话发消息走 Router 分发~~ 反过来（P18）：主对话的消息直接进主会话，不再分发
- [x] 停住的运行（介入清单 `/v1/interventions`）接到对话里（P14）：停住时往它所在的对话投一句话——支线里的投那段支线，主对话派出的投主对话，目标定时触发的投目标绑定的支线、没绑定投主对话（3a-2 接上，`DurableCore::cron_route`）；把你的回复对应到它认的几种答复上（由那段对话里的模型调 `komo` 工具的 `intervention.answer`）。原来的接口留给排查用

- [x] 做法记录（2026-11-14，对话的服务端；代码在 `komo-gateway/src/service/durable/chats/`）：
  - **投一句话**（`say::say_in`，不起 Run）：复用 Router 时代的展示 entry `main.reply`（事件流里仍是 `LineReply`，不挂 Run），区别是这次带 `model`——
    一条 user 消息，前面一行说明"这是你（Komo）刚才对操作者说的，不是操作者说的"，后面可接只给模型看的附注（介入的 handle、可答的结论）；`data` 带
    `text`（界面上那句，不含附注）、`at`、`source`（`intervention` / `cron`）、`key` 与关联字段（介入的 `handle` / `interventionKind` / `session`，
    定时的 `job` / `firing` / `run`）。它是 write submission：对话空闲时立刻落，对话正忙时排进收件箱、下一个 boundary 落（write 的通常规则，不打断 Run）。
    选 user 角色而不是 assistant：boundary 落在工具往返中间时 assistant 消息会打乱调用与结果的配对；没有用宿主附加（`komoOrigin`）标记，因为压缩会跳过它，
    定时结果的正文就丢了。
  - **判重**：关联键就是这条 write submission 的请求键（`submissions.request_id`，落库）；写之前在同一次提交里按**目标 Session** 查已有的 submission，
    有就什么都不写（`Said::Duplicate`），所以重启后的补投、cron 看客重挂都不会重复。键：介入 `intervention:{kind}:{handle}`（崩溃停住的再加起跑时刻，放行后
    再停算新的一句）；定时 `cron:{job}@{计划时刻毫秒}`（手动 run 用 `@run-{run}`）。换了目标会话（例如 3a 绑定的支线变了）算新的一句。
  - **`last_message_at`** 在三处更新，都在已有的写会话行的那次提交里：用户输入受理（`submit::land`）、Run 以最终回复结束（`generation::answer`，回复有文字才算）、
    投一句话（`say_in`）。旧记录读成 0，`SessionRecord::last_message_ms()` 回落到 `changed_at`、再回落到 `created_at`。
  - **介入落在哪段对话**（`DurableCore::route_of`）：主对话与支线对话（`kind = main / normal`，排除 `chat:` 来源与已不收输入的）是自己；任务会话沿 `parent_session` 往上找；
    `kind = cron` 走 `cron_route`（现在主对话）；其余回落主对话。启动时在引擎调度之前订阅提交（faulted / 待确认的任务），并回放全部未答复的介入；老库第一次升级后，
    积压的未答复介入会各投一句（判重键挡住之后的重复）。
  - **`intervention.answer` 的约束**：`komo` 工具的 `intervention.list` / `intervention.answer`（`handle` + `verdict`）。调用它的 Run 必须是操作者亲自发消息开始的——
    取 Run 第一条输入放置成的 entry，kind 在 `confirm::is_trigger_input`（与 `docs/confirm.md` §3 同一张表：`cron.input`、监听、任务交回）里的一律拒绝；任务会话与定时会话也拒；
    只能答指向调用它的那段对话的介入（已答过的走原路径，返回原结论）；结论走现有的 `answer_intervention`，confirm 只认 `approve` / `reject`，blocked 只认 `abandon`（崩溃停住的还认 `resolve`），
    不认的报错并列出能答的。工具说明里写明：只有操作者在这一轮里明确回答了那个问题才能调用，不要替操作者做决定。
  - **已知的缺口（按代码推断，未实测）**：触发器起的 Run 在**同一段对话**里停在 confirm（例如支线里的监听事件起的 Run 调对外工具）时，操作者的回复是往忙着的对话里插话，要等这个 Run 过了 confirm 才会被模型读到，而这个 Run 在等答复——
    只能靠超时（默认 24 小时，不执行）或网页的 `/v1/interventions` 答。主对话派出的任务、定时触发的 Run 停住时对话是空闲的，不受影响。

### 2.2 目标与定时

- [x] 没有终点的目标：复用 `matters.kind` 的领域一类；`docs/tasks.md` D10「去掉领域」已改成「领域并入目标」（3a-1，2026-11-14）。说明统一放新列 `description`（领域也用它，`standard` 仍是「要维持的标准」，没有拿它当说明）；`komo` 工具 `goal.add` 带 `ongoing: true` 建这种目标，`goal.retire` 不再追踪
- [x] 有终点的目标加说明字段（`matters.description`；`MatterAddRequest` / `MatterActionRequest` 的 `description`，不进注入行，多行、≤ 8000 字）
- [x] 定时配置放进目标（节奏、重叠、通知、模型），到点做的事取目标的说明（`matters.schedule` JSON；字段与旧 `cron_jobs` 一一对应，提示词 / 命令也在里面；创建时没给 prompt / command 就取目标的 `description`，读时现取，说明改了下一次跟着改）
- [x] 目标表与定时任务表合成一张（3a-1）：运行状态（下一次时间、上次错误）也放在目标那一行，`cron_jobs` 不再用。调度器更新这两列时不加目标的版本号（版本号只管内容修改），否则网页编辑目标会无故冲突；「下一次时间」单独成列并建索引
- [x] 触发记录（`cron_firings`）保留，键换成目标 id（列名 `job_id` 没改，存目标 id；回填时已链到目标的任务的 firing 改挂，主键哈希随之重算）
- [x] 定时触发的运行带上目标的键（3a-2）：键就是定时会话的来源 `cron:{目标 id}`（建会话时本来就写；目标 id = 定时的 job id），不另写 `komo.task`（cron 会话不是任务会话，没有这份 document）。任务清单的 `goal` 过滤与每行的 `goals` 认它；定时运行里派出的任务（`parent_session` 是这个目标的定时会话，只看一层）也算这个目标的活动。`goal.list` 的任务计数仍只数 `komo.task.goals`，没有算定时会话
- [x] 目标与支线绑定（P22，3a-2）：**只在目标这边记**——`matters.chat_session`（附加列，老库补列），不在会话上另记（支线从不自己「选目标」，一对一天然成立；反查走 `matters.chats()` 整表一次，`GET /v1/chats` 用它给每行带 `goal`）。接口：`POST /v1/matters/{id}/chat`（新开一段 `kind = normal`、`source = web`、标题取目标标题的支线并绑定，返回会话 id 与标题；已绑着活的支线就原样返回，`created = false`；结案的目标 409）、`DELETE /v1/matters/{id}/chat`（解除，幂等，支线留着变回普通支线）。绑定与解绑不升目标的版本。定时运行仍在它自己的会话里做，只把结果与提问作为一条消息投到绑定的支线（没绑定投主对话），下一轮进那段对话的上下文。**支线被归档或删除时不去改记录，读取时视为未绑定**（`DurableCore::bound_chat` 看会话是否还是活着的支线），投递回落主对话；再绑定会覆盖旧记录。`komo` 工具没有绑定动作（只有网页做）
- [x] 运行历史接口（3a-2）：`GET /v1/matters/{id}/runs?limit=&before=`，新的在前，`before` 是游标（上一页的 `next_before`，即最后一条的计划时间）；每条带计划时间、实际时间（建出的会话的创建时刻，跳过 / 没投成的没有）、状态、错误、定时版本、会话与 Run、此刻是否在等你
- [x] 目标详情接口（3a-2，在 `GET /v1/matters/{id}` 上补，不另起）：原有的 `matter`、`schedule`（含下一次时间、上次错误、最近一次触发）之外加 `overview`——`chat`（绑定的支线：会话 id 与标题）、`monitors`（触发器里的监听对上登记的）、`activity`（带这个目标的键的会话，最新的在前，最多 20 条，每条是任务清单的一行加 `waiting`）、`memories`（为它记的，最多 50 条）。写接口的回应不带 `overview`。「近况」取 `activity[0]`，不存。`activity` 靠 `GET /v1/tasks?goal=` 同一套扫描，目标很多会话时会扫得比较久（未做索引）
- [x] 目标的定时运行停住（P16，3a-2）：问话发到绑定的支线（没绑定发主对话）；`activity[].waiting` 与运行历史的 `waiting` 带问话所在的对话与介入句柄（那句话的 `data.handle`），网页据此标「在等你」并跳过去；对话图标上的数字照旧由 `/v1/chats` 按介入所在的对话算（落到绑定支线的计在那段支线上）
- [x] `/v1/cron` 与 `komo` 工具的 `cron.*` 并进目标的接口与 `goal.*`（3a-1：`POST /v1/matters` 带 `schedule`、`POST /v1/matters/{id}/{schedule|unschedule|run}`；`komo goal|area schedule / unschedule / run`；工具 `goal.schedule / unschedule / run / retire`；`/v1/cron*`、`komo cron`、`cron.list` / `cron.run` 已删）
- [x] 存量定时任务各转成一个带定时配置的目标（数据回填：`komo_store::backfill_cron_into_goals`，Gateway 启动时跑，可重入；规则见 `repos/cron_backfill.rs` 的模块注释）
- [x] 状态映射：进行中 / 等待中 → 追踪中；搁置 / 完成 / 放弃（领域的「不再管」）→ 不再追踪。**实现时定的**（3a-1 做、3a-2 确认）：调度器只触发追踪中的目标的定时，搁置与结案的不触发，重开后恢复；已结案的目标不能新绑支线（解绑随时可以）。网页上的说法是 3b
- [ ] 监听与目标的关联现在怎么写入、目标结案时怎么清：未核实（3a-2 只读：详情的 `monitors` 按 `matter.triggers` 里的 monitor 引用对登记；写入仍是 `komo goal link --monitor`，结案不清）

### 2.3 设置

- [x] MCP 逐个工具的启停（2026-11-14，4a）：`PUT /v1/mcp/{name}/tools/{tool}`，持久在 `[mcp.servers.<name>] disabled_tools`，不重连，细节见 `docs/mcp.md` §2.8
- [x] MCP 整台的持久关闭（2026-11-14，4a）：`PUT /v1/mcp/{name}`，持久在 `enabled = false`；原来的 `disconnect` 仍是"到重启为止"的临时断开
- [ ] 网页添加 MCP 服务器：待定（现在只能改配置文件）
- [x] 技能的列表与启停接口（2026-11-14，4a）：`GET /v1/skills`、`PUT /v1/skills/{name}`，名单沿用 `runtime/skills-disabled.json`（与 `komo skills` 同一个文件），`docs/komo_bot.md` §5.6
- [x] ~~渠道的启用开关~~ 不做（P17，渠道已整个删除）
- [x] `router_model` 随 Router 删掉（2026-11-13）。已核实配置层对不认识的键只给一条警告、不判配置无效（`komo-config/src/lib.rs` 的 `read_config`），所以旧配置里留着 `router_model` 或 `stall_*` 服务照常起，`komo config check` 各报一条"不认识这个键，已忽略"，没有另做"退役"处理
- [x] 设置里「主对话的模型」的含义改成主对话这段会话用的模型（不设就跟默认模型）（2026-11-14，4a）：新键 `[models] main`，只对 `kind = main` 的会话生效，写进设置白名单（`PUT /v1/config/settings/models.main`，可清）；默认 / 记忆 / 向量模型的读写本来就有（`PUT /v1/config/default-model`、白名单的 `memory.model` / `memory.embedding`、`PUT /v1/config/embedding-models/{alias}`）
- 4a 盘点（2026-11-14）：提供商增删改、模型目录、环境变量、配置检查、重建记忆索引（`POST /v1/memory-index/rebuild`）、关于页的版本与启动时间（`GET /healthz`）都已有，没动；渠道的启用开关随 P17 不做（渠道已删）；「Gateway 地址可改」是浏览器端行为，不涉及服务端
- [ ] 浏览器里改 Gateway 地址：实际是跳到另一台并重新配对，不涉及服务端；桌面壳把已有的连接设置并进弹窗

### 2.4 去掉群聊（已被 §2.6 包含，不单做）

- [x] 飞书：群里 @机器人 的入站、群话题（`komo-channel/src/feishu/inbound.rs`）（随 `komo-channel` 整个删除，2026-11-13）
- [x] Telegram：群消息（同上）
- [x] Router：群分支（`ignore` 去向、只看本群的清单）（随 Router 删除，2026-11-13）
- [x] 分发：群的线（`source = line:{platform}:{chat_id}`）（随 Router 删除，2026-11-13；存量的群的线会话留在库里，不迁移，群消息改按 `chat:{peer}` 另开普通会话）
- [x] 配置：`groups`（上一步已不认，现在整个 `[channels.*]` 都按退役键处理）；`allow_tenant` 已核实：**不只为群聊**——它让「与机器人同一租户的人」都算操作者，飞书私聊与群的准入都看它，唯一调用方是渠道入站的准入判断（`Dispatcher::principal`），所以随渠道整体删除，没有别的用处（2026-11-13）
- [x] 群任务不带个人记忆与目标的那条规则（随 Router 删除，2026-11-13：`is_group_task`、召回里的 `from_group`、`komo` 工具对群任务的整体拒绝）
- [x] 对应测试（渠道与群聊专属的测试已删，通用行为的测试改走 HTTP 提交，见 §2.6 的做法记录）
- [ ] 计划中的 T5「代操作者回复」取消
- [x] 存量群会话留在库里，不迁移（没有任何迁移或清理代码；`source = chat:{peer}` 的会话照常能读）

### 2.6 去掉聊天渠道（P17，已定：三个渠道全删，直接删代码）

- [x] 涉及：`komo-channel`（三个平台模块与各自的插件二进制）、Gateway 的渠道进程宿主与 Dispatcher、主动消息的投递（Notifier、`deliveries`、`home_chat`）、渠道命令（`/pending` `/new` 等）、`/v1/channels` 接口、配置里的渠道段、发布包里的三个 `komo-channel-*`（`release.yml`、`install.sh`、`update.rs` 三处的约定）（2026-11-13 全部直接删代码，没有留停用的版本）
- [x] 包含了 §2.4（去掉群聊）的全部内容，§2.4 不再单做
- [x] Router 里只为渠道存在的部分一并收掉：话题的路由归属、主线最近几条消息里按平台区分的逻辑（已核实：这些都只在 Router 与分发里，网页的主对话与支线不用，随 Router 一起删，2026-11-13）
- [x] `docs/channel-plugins.md` 归档（`docs/archive/`）；`docs/komo_bot.md` §11 整节、`PRODUCT.md`「一个世界，多个入口」改口
- [x] 做法记录（2026-11-13）：
  - 删：`komo-channel` crate 与 `vendor/wechatbot` 补丁、工作区里的 `openlark` / `wechatbot` / `qrcode` 依赖；Gateway 的 `channels/`、`dispatcher.rs`、`notifier.rs`、`deliveries.rs`、`/v1/channels`、渠道看客的投递（`background/watch.rs` 只剩「首轮完成后起会话标题」与它的重启补挂，见下）、`hooks/channel.rs`（渠道篇幅附注）与 `komo-agent` 的 `context/channel.rs`、介入 / 确认卡往 home chat 的投递（`spawn_attention` 等）、`reload.rs` 里的 home chat 通知、`cron_watch` 的 `notify` 投递；`komo channel` 子命令；`komo-store` 的 `repos/deliveries.rs`；kernel 的 `Channel` / `Inbound` / `Notifier`、`ChannelPlatform` / `ChannelPeer` / `Principal` / `Outbound` / `Delivery*`、`InboundMessage` / `InboundAck` / `ChatCommand`、`ChannelsConfig` 与 `/v1/channels` 的协议类型；网页设置里的「渠道」一节。
  - 留：`PeerId`（事件里的 `by` 字段，存量事件里是渠道 peer 的 id）、`SILENT_REPLY`、`NotifyPolicy` 与 cron 的 `notify` 字段（配置与接口还在，只是不再往外投，随目标的定时一起重做）、`deliveries` 表的 DDL 与模型（老库里有这张表，只建表不读写）、网页附件收件箱（从 `komo-channel` 搬到 `komo-gateway/src/inbox.rs`）。
  - `background/watch.rs` 不是只为渠道存在：它同时负责「run 正常完成后给会话起标题」，所以留下这一部分（看客订阅会话帧、重启补挂、补起最近结算输入的标题）。
  - 配置：整个 `[channels.*]` 按退役键处理，旧配置照常加载，每个键一条警告（测试 `keys_retired_with_the_chat_channels_still_load_with_a_warning`）；`mcp.servers.*.outward` 那条「没有渠道配 home_chat」的警告随之删除。
- [ ] 后果：Komo 主动找你的话（定时的结果、问你一句）只有在网页或桌面端开着时才看得到；手机上没有入口，直到 PWA 做出来（`docs/clients.md` C3–C4）

### 2.7 去掉 Router（P18）

- [x] 删：分发判断（`komo-agent/src/router.rs`、`context/router.rs`）、分发的引擎任务与持久协议、结果直投与主线展示消息（`komo-gateway/.../dispatch/`）、话题的路由归属、`router_model`（2026-11-13 已删。网页的 `POST` 与渠道私聊主线进来的消息都直接进主会话；渠道、群的入站按 Router 之前的办法恢复：私聊主线进主会话，群与带话题的消息各按 `chat:{peer}` 开普通会话，渠道命令 `/new` `/cancel` `/status` 回到对普通会话的含义，下一步整个渠道删掉时一并走）
- [x] 已落盘的 `main.user` / `main.reply`（Router 时期主会话里写下的展示 entry）不再写，读取与事件投影保留（`MessageUser` / `LineReply`，`komo-kernel/src/durable/legacy_events.rs`）；它们没有 `model`，不进上下文。`komo.task` 里 Router 时期写的 `reply` / `thread` / `session` 与 `komo.live` 里的 `activityAt` 读到时被忽略（两处都不拒绝未知字段）
- [x] 主会话恢复成会跑模型的普通会话：有 Run、有压缩与提示缓存（`kind = main` 与普通会话走同一条受理与 Run 路径，能力面是完整的做事能力面）
- [x] 留下：模型调用的容错（T2a）、`komo` 工具（含 `goal.list` / `goal.attach` / `goal.detach`）、`task` 工具（长活可以派到后台）、任务清单接口 `GET /v1/tasks`（去留见 §2.5）、任务带目标的键、"停下"（`DurableCore::stop_task`：标已停、撤回排队输入、停监听、丢弃晚到的监听事件，任务清单里算"没有完成"）。注意 `stop_task` 现在没有生产调用方（原来只有分发的 `stop` 与渠道 `/cancel` 用它），只有测试在用
- [x] 「不许沉默」的补救、卡死检测、"用户消息派出的任务视同用户发起 / 可改承诺"这类规则：已核实，全部只对 `komo.task.reply`（分发派出的任务）生效，与任务会话共用的部分没有，整套删除（含 `[execution] stall_model_secs` / `stall_tool_secs`、`komo.live.activityAt`、`task.stall` 输入）。`task` 工具派出的任务本来就走"交回父会话"的老路，那条路的末行状态行与自动续跑一直是独立的，未动
- [x] 目标的定时结果与提问投到主对话：作为主会话里的一条消息落下，下一轮进上下文（2026-11-14：定时结果按 `notify` 投、提问由介入投；**绑定了支线时改投那段支线留给 3a**，接缝 `chats::cron_route`）
- [x] 主对话的上下文：同一个会话加自动压缩，见 §2.8

### 2.8 主对话的上下文（照 Muse 的做法，建议）

- [x] 主对话一直是同一个会话；太长时把较早的部分压缩成摘要（现有的自动压缩），不另做按时间分段
- [x] 记忆提取不用改：现在就是会话空闲满 `[memory].extract_idle`（默认 10 分钟）后领走尚未处理的 Run，有游标，适用于一直延续的会话（`docs/memory.md` §9.10，已读文档与 `komo-runtime/src/durable/memory.rs` 的领取条件）
- [ ] 可选的小优化：隔了很久再回来、上下文又已经很大时，趁提示缓存本来就凉了先压缩一次。先不做，看实际用量再定
- [ ] 更早的细节靠检索找回（`komo` 工具的 `session.search`），现有

### 2.9 部署成服务（P19：单人、自己的服务器）

- [x] 命令在哪里执行：都在远程那台机器上（P20），不做本机执行端
- [ ] 现状：通过 Cloudflare 隧道与 Tailscale 访问远程机器上的 komo（操作者 2026-11-13 说明）。入口和传输加密已经有了
- [ ] 登录接 OIDC（操作者 2026-11-13 定）：komo 作为接入方，走授权码流程（带 PKCE）
  - 配置：签发方地址、客户端 ID、客户端密钥（放 `.env`）、允许登录的账号（邮箱或 subject，单人就一个）、回调地址
  - 路由：发起登录、回调（校验签名、签发方、受众、nonce、有效期，再核对允许名单）、退出
  - 登录成功后发 komo 自己的会话 cookie（`HttpOnly`），会话要落盘，服务重启不掉登录
  - 网页：没登录或会话过期时跳去登录；设置的「关于」里显示当前账号与退出
  - 身份提供方：自己搭的 Pocket ID（操作者 2026-11-13 定）。在它那边建一个客户端，回调地址填 komo 的回调路由；komo 这边签发方地址填 Pocket ID 的地址。它对 PKCE、发现文档的支持情况未核实，落地前看它的文档
  - **配对码去掉**（操作者 2026-11-13 定）：删 `komo web` 的配对链接、`/pair` 页面与 `routes/pair.tsx`、`/v1/web/pair-code`、`/v1/web/session` 里换 cookie 的那条路、未配对时的提示组件。桌面端现在怎么鉴权未核实，若走配对要一并改
  - 机器上的命令行仍用本机令牌（`runtime/gateway.json`），不经 OIDC；这也是 Pocket ID 不可用时的唯一后路：登到机器上处理
  - 新依赖（OIDC 客户端库）要补 `docs/komo_bot.md` §13.4 的依赖表
  - 用哪家身份提供方不写死：按标准发现文档（`.well-known/openid-configuration`）接，Cloudflare Access、公司的 SSO、自建的都行
- [ ] 网页与接口放在同一个域名下：网页的静态文件发布到 Cloudflare（Workers 静态资源或 Pages），同一个域名下 `/v1/*` 转到隧道后面的 Gateway。这样仍是同源，cookie 与事件流不用改；「分开部署」指两个产物各自发布，不是跨域。**同一域名下静态资源与隧道按路径分流的具体配法、事件流经过这层是否正常，未核实**
- [ ] Gateway 不再托管网页（做成可关的开关，本机开发时仍可用）
- [ ] 传输加密已由 Cloudflare 与 Tailscale 承担；Gateway 的 Host 校验把用到的域名加进 `trusted_hosts`（现有配置）
- [ ] 部署方式：现在已经跑在远程虚拟机上，容器不是必须；先沿用现有的托管方式，要不要换成容器待定
- [ ] 版本错位：网页与服务端分开发布后，网页启动时核对服务端的协议版本，不兼容时提示刷新或升级
- [ ] MCP 的授权回调：现在听本机随机端口，要改走服务的公开地址（`docs/tasks.md` §5.3 记过）
- [ ] Codex 等登录流程搬进网页（设备码显示在网页上）
- [ ] 公网那条路上登录是唯一的墙（命令执行没有限制）：登录的失败限制由身份提供方承担；除登录相关的路由与健康检查外，所有路由都要过会话校验，不留绕过的路
- [ ] 只为「装在自己电脑上」存在的东西可以删：macOS 的 launchd 托管、桌面端的「本机模式」（自己拉起 Gateway）、本机发现文件的客户端读取路径。Linux 的 systemd 托管与更新方式按实际部署保留
- [ ] 桌面端只剩「连远程」一种用法：保留还是只用网页，待定
- [ ] 桌面端改成 OIDC 登录（已核对 `apps/desktop/src/main/`）：现在远程模式是手填地址加令牌，主进程转发请求时带上令牌；不走配对码，删配对码不影响它
  - 连接页的「设备令牌」输入框换成「登录」按钮：用系统浏览器打开 komo 的登录地址，由服务端走完 OIDC
  - 登录完成后服务端跳回 `komo://`（深链处理已有，现在只认 `komo://session/<id>`，加一种形状），带一个一次性的码；桌面端用它换一个长期的设备令牌
  - 服务端要新做：这条「登录后给桌面端发设备令牌」的路，以及设备令牌本身的签发、保存与吊销（`docs/clients.md` 里计划在 C3，代码里还没有）
  - 不用改：令牌的加密保存、`app://` 转发时带令牌、额外请求头、连接测试
  - 删：本机模式（读本机的发现文件、拉起本机 Gateway）
- [ ] 文档：`PRODUCT.md` 的定位与「本地优先」、`docs/clients.md` D5 / D6、`AGENTS.md` 的命令表与发布约定

### 2.5 可以删的旧接口（随客户端改完）

- [ ] `/v1/sessions/{id}/fork`（`docs/tasks.md` 已列为待删）
- [ ] `GET /v1/tasks` 的去留：页面没了，但「等你的数字」「正在输入」可能还要用它，待定

## 3. 数据

- [x] `matters`：加说明列、定时配置（一个 JSON 字段）、下一次运行时间（单独一列，建索引 `matters_next_run`）、上次错误（3a-1；另有 `schedule_status` 启停列与 `migrated_from` 回填标记列）
- [x] `cron_jobs`：并进 `matters`，表不再用（表与模型留着，回填之后不读写）
- [x] `cron_firings`：保留，键从定时任务 id 换成目标 id
- [x] `matters.chat_session`：绑定的支线对话（会话 id，可空；3a-2，`ColumnSpec` 带老库补列，旧行读成没绑定）。`MatterRepo` 加 `chat` / `chats` / `set_chat`，`CronRepo` 加 `firings_page`（默认实现，运行历史分页）
- [x] `sessions`：补最后一句话的时间（`SessionRecord.last_message_at`，会话行 JSON 里加一格，没有迁移）
- [ ] 配置文件：MCP 工具启停、服务器启用、技能与渠道启用；删群聊相关键
- [x] 回填：存量定时任务 → 目标（单元测试覆盖；对 `~/.komo` 副本的演练待统一验证阶段补）
- [ ] 记下不做的：把 `aux.db` 并进 `durable.db`（与这次无关，单独一笔）

## 4. 文档

- [ ] `docs/tasks.md`：§4 Router - Worker 整节作废（P18），状态从草案改为已取代；改 D10（领域）、「不再有人直接开的会话」、§4.12 U8、§4.13（群聊）、T5；补上面各项
- [ ] `docs/web.md`：§2–§6、§11–§15 基本重写（3a-2：§9 第 10 条补了 `goal`，§13 补了目标接口）
- [ ] `docs/goals.md`：没有终点的目标、定时配置、状态的对外说法
- [ ] `docs/komo_bot.md`：§11.3 群规则、定时一节、命令表
- [ ] `PRODUCT.md`、`AGENTS.md`：术语（事项、定时、任务会话）与入口描述

## 5. 建议的顺序

1. 影响接口形状的几件都已定（P13–P16）
2. 服务端 2.1（对话）→ 客户端 1.1、1.2（外壳与对话）：先让主页立住
3. 服务端 2.2（目标与定时）+ 数据回填 → 客户端 1.4
4. 设置（2.3 + 1.6）、搜索、记忆
5. 去掉群聊（2.4）可以独立做，不挡前面
6. 文档随每一步写回
