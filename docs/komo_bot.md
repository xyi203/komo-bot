# Komo：Rust 个人 Agent 框架设计

版本：v0.8 · 日期：2026-09-15 · 状态：设计稿，尚未实现。

本文定义一个全新项目。Komo 是程序名称；架构只依据本文的需求与约束。

v0.8 相对 v0.7 的变化：状态数据库改为 Turso + toasty（§8.2）；工程结构改为七个 crate（2026-09-22 抽出 `komo-agent`），编译预算列为验收项（§13.4）；飞书 / Telegram / WeChat 聊天入口与 TUI 进入首版，需要人判断的事主要在聊天里回答（§11）；新增 Skills（§5.6）；补齐 trait 清单（§13.5）；渠道的身份匹配（谁是操作者、哪个会话是 home chat）只放 config.toml / .env，不进数据库（§11.2）；配置热重载，改 config.toml / .env 不重启 Gateway（§3）。全新实现，不迁移旧代码与旧数据。

2026-10-11：删掉整套 Policy 与审批（§7）。单操作者、一直跑 `mode = "auto"`，审批不在做判断；之后模型要的 komo 就执行，唯一的硬限制是 `write` / `edit` 不碰 komo 自己的状态，真正的边界只来自执行环境（codemode 的 Monty 引擎）。旧会话与旧库里的审批痕迹照常能读，不再生效。

## 1. 已确定的范围

**一个 Rust 程序、六个基础工具、一套可恢复的 Agent Runtime。**

| 项目     | 决策                                                                                             |
| -------- | ------------------------------------------------------------------------------------------------ |
| 部署     | Fedora Server 与 macOS 均可独立运行，每个实例保存自己的数据                                      |
| 进程入口 | 单个可执行程序 `komo`，包含 TUI 聊天客户端和 Gateway                                            |
| 聊天     | `komo` 直接进入 TUI 聊天；Gateway 未运行时自动启动                                                |
| 执行方式 | LLM 原生 tool call，由 Rust 校验和执行；没有 Policy 与审批（§7）                                 |
| 基础工具 | 严格保留 `read`、`write`、`edit`、`bash`、`codemode`（外加只读的 `rg`，§4）                      |
| 能力扩展 | 可复用能力写成 Skill（`SKILL.md` + 脚本），由 `bash` 运行；没有 toolbox（2026-10-15 删除，§5.3）   |
| 程序性说明 | Skills：人写的 `SKILL.md`，模型用 `read` 读取，不增加工具                                       |
| 定时任务 | Cron 触发同一套 Agent Runtime                                                                    |
| 持久化   | 每个 Session 一个数据目录，集中保存 JSONL、大正文、tool output 和产物；Turso（MVCC）保存调度状态与索引 |
| 人工介入 | 没有 Policy 与审批（2026-10-11 删除，§7）；结果不明与阻塞在飞书 / Telegram / WeChat 里回答，TUI 与 CLI 为辅 |
| 自动记忆 | 自动积累偏好、事实和经验，区分用户陈述、工具观察、模型推断与用户确认                             |
| 主动记录 | 用户明确要求保存的内容写入现有 Memos，原文以 Memos 为准                                          |
| 记忆检索 | 关键词与向量混合检索，索引保存在本机，可重建                                                     |
| 模型配置 | 主模型、记忆整理模型、记忆向量模型分别配置；统一提供 effort 并按模型能力校验                     |
| 聊天入口 | 飞书、Telegram、WeChat 首版都在，复用 Gateway 的会话、运行与介入接口；每个渠道一个 feature       |
| 工程结构 | 七个 crate 按依赖重量与变更频率划分；冷编与增量编译时间是验收项                                  |

代码优化、HA、网页搜索、记事是工具组合的使用场景，不成为专用的 Rust 工具或 Runtime 业务类型。

首版保持单操作者、单实例运行方式，不引入实例同步、跨机器调度、外部消息队列或独立向量数据库服务。支持向量检索不要求部署额外服务。

## 2. 总体结构

```text
komo TUI / resume / 操作命令          飞书 ws · Telegram 轮询 · WeChat iLink
              │                                     │
         HTTP / SSE                          Channel → Dispatcher
              │                                     │
        Gateway 常驻进程 ◀───────────────────────────┘
              │
     ┌────────┼───────────┐
     │        │           │
 Session   Agent Runtime  Cron Scheduler
  Store       │           │
     │        └── 同一个 Run 提交入口
     │
     └── JSONL 事件 + 独立 tool output + state.db 状态 / 索引
                    │
                Agent Loop ↔ LLM
                    │
               Tool Executor
                    │
           prepare（参数 / 版本校验）
             ┌──────┴──────┐
           执行           拒绝（明确的拒绝结果）
             │
      read / write / edit / rg / bash / codemode
```

**核心里没有 Run 与 Run 之间的关系（2026-10-16 定；同日 `task` 落地，新 Run 不再产生同步委派。为升级时还没收尾的旧子 Run 留着的那几处——fold 的 `delegate`、领取豁免、回放线、`run_end` 收尾——还在，等旧数据收尾后再删）。** 核心只管一个 Session 里的事：输入受理、Run 按次序跑、模型与工具的循环、落盘与恢复。凡是"一件事派生出另一件事"——后台任务（§4 `task`）、监听、Cron、渠道——都在核心外面，只用两个入口：**往某个 Session 提交一条输入**（带请求键，幂等）与**看某条 Run 进终态**。起因是同步委派：它让父 Run 等子 Run，一路穿进 kernel 的 fold / 恢复、store 的领取豁免与 reconcile、agent 的回放线、gateway 的装配与压缩；同样是委派，异步的 `dispatch` 只占一个工具加 Gateway 里一个服务。所以同步委派删掉（§4），也不为"等另一件事"加让出、信号表或可替换的 loop；要等结果就是这一轮先收尾，结果作为一条新输入回来（运行中则并入，§6 插话）。同一 Session 里的先后次序（§8.4 的 `dependency`）与插话的并入（`absorbed`）是会话内的次序，不算这里说的关系。

MemoryManager 在 Run 开始时装配相关记忆，在 Run 完成后异步整理新证据。它管理本机记忆与检索索引；访问 Memos 通过 Memos skill 里的脚本由 `bash` 运行，不新增模型工具。

Gateway 持有模型连接、数据库、Session JSONL 写入器、工具环境、运行状态和三个聊天渠道。TUI 负责提交输入、显示进度、回答介入和恢复会话；聊天渠道做同样的事，并且是介入的主入口（§11）。

每个实例的 Gateway 自己执行任务。CLI 从 Mac 连接 NAS 的 Gateway 时，命令都在 NAS 虚拟机中执行。

## 3. 命令与 Gateway 生命周期

以下是拟定命令，不代表已经安装或实现。

| 命令                                         | 行为                                                                        |
| -------------------------------------------- | --------------------------------------------------------------------------- |
| `komo`                                       | 确保本机 Gateway 就绪，进入 TUI 聊天。**会话在第一条消息送出之前才创建**——空跑一眼就退出，不在账本和磁盘上留下空壳 |
| `komo resume SESSION_ID`                     | 连接原会话并查看进度或处理待办；恢复调度由 Gateway 自动进行，不重复创建 Run |
| `komo session list`                          | 查看会话列表和状态；`--all` 才列出已逻辑删除的会话（§8.10）                  |
| `komo session delete/purge SESSION_ID`       | 逻辑删除（`closing` → `deleted`）与回收（`purged`）。**删除只有这一条路**：`--now` 把未完成 Run 各写一条明确取消，`purge` 前先算引用（§8.10） |
| `komo run inspect RUN_ID`                    | 查看执行过程、工具结果与产物                                                |
| `komo run cancel RUN_ID`                     | 请求取消运行                                                                |
| `komo gateway`                               | 启动后台 Gateway，等待就绪后返回                                            |
| `komo gateway --foreground`                  | 前台运行 Gateway，供服务管理器及调试使用                                    |
| `komo gateway status/stop/restart`           | 管理后台进程。在 Gateway 自己的 `bash` 里跑 `stop` / `restart`（TUI 的 `!`、模型发起的都算）不当场做，推迟到这条命令结束、结果记进会话之后（见下文"在 Gateway 里重启自己"） |
| `komo intervention list/show/answer`         | 待处理清单：结果不明、阻塞两类一张表；等价于聊天里的 `/pending` 与 `/answer`（§7.5、§11.3） |
| `komo cron add/list/run/pause/resume/remove` | 管理定时任务                                                                |
| `komo memory list/search/show`               | 查看自动记忆、来源与确认状态；search 支持 hybrid / keyword / vector；list --full 一条一行不截断 |
| `komo memory add`                            | 手动记一条（§9.3）；在 Run 里（带 `KOMO_SESSION` / `KOMO_RUN`）记成未确认、引用户原话，终端里直接记成已确认 |
| `komo memory confirm/forget`                 | 确认具体版本或停用自动记忆；不删除 Memos 原文                               |
| `komo memory consolidate`                    | 离线整理（§9.8）：对已有记忆去重、取代、标冲突，不新增正文；`--scope` `--kind` `--instruction`，默认预演，`--apply` 才写 |
| `komo memory index status/rebuild`           | 查看索引覆盖率或重建当前向量索引                                            |
| `komo config check`                          | 校验各模型、effort、向量参数、渠道名单及其他配置；只读，不改变运行中的 Gateway。数据目录里还留着 `policy.toml` 时报一句"已不再使用，可以删掉"（§7） |
| `komo config reload`                         | 让 Gateway 立即重载配置（等价于文件保存后的自动重载或 `SIGHUP`）；校验失败则保留旧配置并返回错误 |
| `komo mcp reconnect [SERVER...]`             | 让 Gateway 断开重连 MCP 服务器（不点名 = 全部），等握手完报各自状态；新连上的工具从下一条 Run 起可用，不用重启 Gateway（`docs/mcp.md` §2.5） |
| `komo channel list/probe`                    | 渠道清单与连通性核对（飞书 tenant token、Telegram `getMe`、微信凭证文件）；不经 Gateway |
| `komo channel wechat login`                  | 终端显示二维码完成微信登录，凭证写入数据目录                                |
| `komo auth codex login`                      | ChatGPT 账号 OAuth 设备码登录（Codex 模型走 `auth = "chatgpt"`，§13.3）；不经 Gateway，凭证写入 `<数据目录>/codex/auth.json`（同微信登录的先例） |
| `komo auth codex status`                     | 账号、邮箱、套餐与凭证过期时间；**不打印 token**；不经 Gateway               |
| `komo auth mcp login SERVER`                 | `oauth = true` 的 MCP 服务器：浏览器授权码 + PKCE，回调听 `127.0.0.1` 随机端口；不经 Gateway，凭证写入 `<数据目录>/mcp/<SERVER>/credentials.json`（`docs/mcp.md` §2.1） |
| `komo auth mcp status`                       | 各 OAuth MCP 服务器登录与否、过期时间、能否自动刷新；**不打印 token**；不经 Gateway |
| `komo skills list/inspect/enable/disable`    | Skills 目录（§5.6）；只读文件系统，不经 Gateway                             |
| `komo update`                                | 从 GitHub release 换掉当前这个可执行文件（§13.6）。**不经 Gateway**：换的是磁盘上那份二进制，不是在跑的那个进程 |

聊天启动顺序：

1. 读取当前实例的连接配置和发现文件。
2. 检查 Gateway 健康状态与实例身份，不能只凭 PID 或端口判断。
3. 本机实例未运行时，请求系统服务管理器启动。
4. 等待服务就绪，超时则返回具体诊断信息。
5. 建立聊天连接。**新会话在这里还没有创建**：它在第一条消息送出之前才建，订阅也才在那一刻建立（`komo resume` / `komo home` 手里本来就有会话，照旧先补读历史再订阅）。

Gateway 获得数据目录进程锁并完成存储校验后，自动扫描未完成运行；不必等用户打开 CLI 或发送 resume。恢复与新请求共用调度器，恢复扫描本身不等待全部旧任务完成才提供服务。这次扫描就是 §8.9 的 reconcile：会话是否还能服务、哪条 `running` 已经没主人（租约过期或旧实例遗留）、哪条停在等待上的该放行，与 Run 自己的位置一起在下一次领取之前判定完。

**就绪不等投递补发。** 上一次没送到的投递（§11.4）在"Gateway 就绪"**之后**的后台补发，不压住第 4 步的等待：补发是网络 I/O，一条 pending 一个平台往返，积压多少条就等多久——线上十来个卡住的投递是几秒，上百条会拖过客户端的就绪超时。补发晚一步没有代价：投递记录先写后发，按 `DeliveryId` 幂等，重来一次也不会重复发。**顺序上唯一的硬要求是渠道登记之后**——渠道没登记时冲刷等于什么都没干。

Gateway 对数据目录持有进程锁。多个 CLI 同时启动时，只允许一个 Gateway 接管实例；启动失败不能通过删除仍有效的锁来强行重试。

**就绪也不等模型探测。** §9.5 那一次"省略 `dimensions` 时先探一次维度"同样是网络 I/O，同样排在"Gateway 就绪"**之后**的后台：端点慢、或者在收连接却不回话时，这一次探测要等满模型超时（本机实测一个只收不回话的端点 = 120s），压在就绪前面就是 `komo gateway restart` 看上去卡住。探测落定前检索这一侧按 §9.4 如实降级——hybrid 走关键词并带降级说明，vector-only 报**不可用**；**不是** `VectorUnconfigured`：配了 alias 只是后端还没在手上，那是"这一刻不通"，不是"配置错了"。探到维度就把向量后端装上，同一台实例的向量臂立刻可用；探不到就一直空着，与端点本来就不通时同一个行为。

**配置热重载。** `config.toml`、`.env` 改了不用重启 Gateway（`policy.toml` 2026-10-11 起不再读，文件还在时启动日志与 `komo config check` 报一句，§7）。三个触发方式落到同一个函数：文件 mtime 变化（每秒轮询一次，不引入 inotify 依赖，编辑器的临时文件与原子替换都覆盖到）、`komo config reload`、`SIGHUP`。流程固定：

1. 重新解析两个文件成一份**完整的**新快照，跑与 `komo config check` 相同的校验；任何错误 → 旧快照原样保留，错误写日志并投递到 home chat，重载命令返回该错误。**校验不过的配置永远不会被装上**，哪怕只错一个键。
   **不认识的键不算错**（2026-09-24）：`config.toml` 里多出来的键——升级后留下的退役键、拼错的键、写到了别的表下面的键（`default_agent` 落进前一个 `[agents.x]` 是最常见的一种）——被忽略，配置照常装上，但每一个都以完整路径（如 `gateway.listen_typo`）作为**警告**出现在 `komo config check`、`komo doctor` 与重载日志里。键认识、值的类型不对仍然是错误。
2. 校验通过 → 原子替换进程内唯一的 `Arc<ConfigSnapshot>`（arc-swap）。**读配置的地方按用途读当前快照，不缓存**：Dispatcher 判定 Principal 时读 `allow_from` / `guest_agent`，Notifier 解析 home chat 时读 `home_chat`，新 Run 在 `accept_input` 时抓一份模型 / effort 快照存进 `runs.config_snapshot`——所以名单改完，下一条消息就按新名单判；模型改完，下一个 Run 用新模型，**正在跑的 Run 与后台记忆任务继续用它们各自开始时抓的快照**（§13.3），不在半路换模型。
3. 逐段比对新旧快照，只重建变化了的东西：某个 `[channels.*]` 的 `enabled` 或凭证变了 → 只停掉并重启那个渠道的 `serve`（飞书重连 ws、Telegram 重新起轮询），其他渠道不受影响，未 ack 的入站消息按平台的至少一次投递重来；模型 `base_url` / 凭证变了 → 换掉对应 `LlmClient` 实例；向量模型变了 → 走 §9.5 的空间指纹与索引代次流程，不在重载里偷偷重建索引。
4. 一小组键**只在启动时生效**：数据目录 / `KOMO_HOME`、监听地址与端口、数据库路径。这些键改了，重载照常完成其余部分，然后明确报告"以下键需要 `komo gateway restart`"，不静默忽略，也不假装已生效。

重载事件带新旧快照的差异（键名，不带值，凭证更不带）写进 Gateway 日志；`komo doctor` 显示当前生效配置的加载时间与来源文件 mtime，两者不一致就是"文件改了但没装上"，把上一次校验错误一并印出来。

Fedora 使用 systemd 管理，Mac 使用 launchd；服务管理器运行前台形式的 Gateway。**单元名是全局的（`komo-gateway.service` / `dev.komo.gateway`），只有默认数据目录 `~/.komo` 拥有它。** 别的 `KOMO_HOME` 走 `komo gateway` / `stop` / `restart` 会被**拒绝**并告诉你去前台跑 `KOMO_HOME=… komo gateway --foreground`：那条路会把单元文件改写成指向这个数据目录，现役服务随即被换掉——2026-09-21 实测过一次，`KOMO_HOME=/tmp/… komo config check` 经 `connect_or_start` 起服务，把现役单元换成了一份 debug 构建 + 沙箱数据目录。`komo gateway status` 会印出单元实际在服务哪个数据目录，与当前 `KOMO_HOME` 不一致时明说。**单元文件只设 `KOMO_HOME`（与可选的 `KOMO_LISTEN`）和 `PATH`，不加载 `.env`、不含任何凭证**。`PATH` 取自写单元的那个进程（操作者的 CLI），单元在每次 start / restart 时重写，`PATH` 随之刷新；写单元的进程没有 `PATH` 或为空就不写这一项，不编默认值。写它的原因（2026-09-24）：`bash` 工具的子进程继承 Gateway 的环境，而 launchd / systemd 默认只给 `/usr/bin:/bin:/usr/sbin:/sbin`——实测一次会话里 `komo`（`~/.cargo/bin`）与 `rg`（`/opt/homebrew/bin`）都是 `command not found`，白白耗掉几轮。`PATH` 不是凭证，下面几条理由都不适用于它。凭证不进单元的理由：launchd 没有 `EnvironmentFile` 的等价物，只能把值抄进 plist，那是把密钥复制到第二个文件；走进程环境还会让 `.env` 的热重载失效，并让每个子进程与日志都可能看到它。`.env` 由 Gateway 自己读成 `Secrets`（§3 热重载覆盖它），需要凭证的 Python 模块按**变量名**声明（§5.3），Gateway 在起子进程时按名注入。Mac 若后续需要操作用户桌面应用，应按登录用户的执行环境配置。后台服务不会让睡眠中的电脑继续执行任务。[Fedora systemd](https://fedoraproject.org/wiki/Packaging:Systemd) · [Apple launchd](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)

**在 Gateway 里重启自己**（2026-10-13）。`bash` 工具的子进程带着 `KOMO_RUN`，`komo gateway stop` / `restart` 见到它就不当场做：同步做等于让 Gateway 停掉正在跑这条命令的自己——停机要等手上的调用收尾，那个调用正等着 Gateway 停下，互相等到服务管理器动手杀，这次调用的结果永远落不了盘；新 Gateway 起来时这条命令还在等它就绪，恢复扫描看见"上一代的子进程还活着"，把这条 Run 停在等人上（实测：TUI 里 `!komo gateway restart` 转了两分钟，事件订阅报 `error decoding response body`）。现在交给这条命令之外的人在它结束之后做：launchd 下起一个**自己进程组**的 `komo gateway <stop|restart> --after-pid <pid>`（`bootout` 不连它一起带走），等发起的命令退出再留 1.5 秒让结果落盘、Run 收尾，输出写 `logs/gateway-restart.log`；systemd 下后台进程躲不过 `stop`（同一个 cgroup），所以先按当前环境重写单元，再用 `systemd-run --user --on-active=2` 挂一次 `systemctl --user <verb>`。命令本身立刻返回一句"等它结束之后再重启"。`--after-pid` 是隐藏参数，只给那个后台进程用。只认 `bash`：`python` 的子进程没有 `KOMO_RUN`。systemd 这一支没有在真机上跑过（§14）。

默认监听回环地址。首版远程访问可通过 SSH 转发连接；直接开放网络监听时需要 HTTPS 和认证。选择远程实例时，不因连接失败而启动一个本机替代实例。`status` 与 `stop` 不隐式启动服务。

## 4. 六个基础工具

| 工具     | 输入与输出重点                                       | 执行约束                                             |
| -------- | ---------------------------------------------------- | ---------------------------------------------------- |
| `read`   | 路径、读取范围；返回文本和文件版本（抬头印版本哈希前 12 位） | 大文件截断，明确显示未读范围                         |
| `write`  | 路径、完整内容、可选预期版本；可选 `then_run`（写完紧接着跑的命令） | 原子替换；覆盖现有文件需检查版本（完整哈希或 ≥8 位前缀；`{}`/`null` 按没给），回执印新版本。带 `then_run` 见下文 |
| `edit`   | 路径、`edits[]`（每处：明确匹配内容、替换内容、可选 `replace_all`）；可选 `then_run` | 匹配失败返回错误，不模糊猜测；多处对原文件匹配、互不重叠，一处失败文件不动（见下文）。带 `then_run` 见下文 |
| `rg`     | 正则、搜索路径、文件 glob；返回「路径:行号:正文」    | 只读（`Operation::ReadFile`，与 `read` 同属可并发的一类）；这台机器上没有 ripgrep 就不挂这个工具 |
| `bash`   | 命令、工作目录、超时；返回退出码和输出               | 管理进程组、限制输出、支持取消；超时也交回到点前的输出 |
| `codemode` | 一段 Python 脚本；返回脚本的输出                   | 跑在 Monty 里，文件只读；脚本里经 `tools.<name>()` 调只读工具与 MCP（见下文与 `docs/codemode.md`） |

**`python` 工具删除**（2026-10-15）。模型不再有 `python`：`codemode`（Monty，文件只读，经 `tools.*` 调其他工具）覆盖聚合与过滤，需要完整 Python（第三方库、写文件、联网）就用 `bash` 跑 `python3`；Gateway 不再管理 Python 虚拟环境（`python-envs/`、`[paths] python_env_root`、`PythonHost`、`env_version`、`KOMO_ARTIFACT_DIR` 都没有了）。旧记录照常解码与回放：`Operation::PythonCode` 与 `PlanVersions.env` 只为解码旧计划保留（没有构造方、没有写入方），投影只认落盘事实、不认工具名，所以历史 `python` 结果逐字节不变；`artifact://files/…` 仍读得到会话目录 `artifacts/` 里已有的产物，只是不再有新的写入方。恢复时的通用规则见 §8.6。

**`shell` 改名 `bash`**（2026-10-15）。模型看见的工具名从 `shell` 改成 `bash`——它本来就是 `/bin/bash -c`（没有 bash 退到 `/bin/sh`）。`Operation::ShellCommand`（序列化标签 `shell_command`）不动。改名前落账的记录照旧叫 `shell`：投影（`komo-kernel::projection`）与 TUI 对 `shell` / `bash` 两个名字同样处理，回放逐字节不变；恢复时一条按 `shell` 落账、已 `started` 而无结果的调用走 §8.6 那条通用规则（工具名已不在工具表里的在途调用记 `uncertain` 交给模型），不重跑也不当"没有这个工具"。Profile / skill 里写的 `tools = ["shell"]`、`requires_tools: [shell]` 要自己改成 `bash`，没有别名。

**`edit` 一次改同一个文件的多处：`edits[]`**（2026-10-21，照 pi 的 `edit` 与 `applyEditsToNormalizedContent`）。参数是 `path` 加 `edits: [{ match_text, replace_text, replace_all? }]`。语义与 pi 一致：每一处都对**原文件**匹配，不在前一处改完的结果上接着找；每一处必须唯一，除非这一处写了 `replace_all`；`match_text` 为空串就拒；各处的匹配区间重叠（含嵌套）就报错，首尾相接不算；全部成立才从后往前替换。报错点名是第几处（`edits[i]`）。整个调用是原子的：任意一处不成立，文件一个字节都不动。预期版本照旧只有一个来源——`prepare` 读到的那份内容哈希，绑定一次；保护路径照旧在 `prepare` 拒。照 pi 的 `prepareArguments`，`edits` 编成 JSON 字符串、或只给一个对象而不是数组，`prepare` 先摆正再解析。旧的单处写法（顶层 `match_text` / `replace_text` / `replace_all`）照常解码、执行，就是只有一处的 `edits`，`prepare` 给它补上明确的 `replace_all`，计划参数与此前逐字节相同；schema 里不再给它们，两种写法混用、都没给、`edits` 是空数组，都是 `InvalidArguments`。description 与系统提示引导模型"同一文件改多处，用一次 edit 带多个 `edits[]`，挨得近的改动合成一处"；此前（2026-10-15 起）是让模型在同一轮连发几个 `edit`，执行器的屏障让那样仍然成立（§6），只是每处都要一次往返、各绑一次版本。

**`then_run`：改完紧接着跑一条命令，一次调用、一条结果**（照 SoL-Pi 的 Action Fusion，省掉"改 → 模型 → 跑"中间那一次往返）。`edit` / `write` 的参数可以带 `then_run: { command, timeout_secs? }`；`prepare` 用 `bash` 自己的计划构造把它做成第二步，挂在计划的 `then_run` 上，`execute` 从 `plan.then_run` 取它；改动那一步照样受 §7 那条硬限制（不写 komo 自己的状态）。执行（`tools::fusion`）：改动失败 → 命令不跑，结果里写 `[then_run:skipped]`；改动之后目标的内容哈希必须仍是改动留下的那一份，否则同样跳过；命令走 `bash` 的正常执行（进程组、时限收紧到活动执行时限、取消），输出流进**同一次尝试**的 stdout / stderr；非零退出照实报（`exit_code`），改动不回滚。结果是 `{ mutation, then_run: { status: succeeded|failed|skipped, result } }`，状态只有"改动成功且退出码 0"才是完成；正文依次是改动的结论、`[then_run:…]` 标记、命令的输出。这次运行的能力面里没有 `bash`（或执行器没装它）时，带 `then_run` 的调用不放行、不执行，交回一句原因；交给模型的 schema 也一并摘掉 `then_run`。组合调用是写，所以是并发调度里的屏障（§6）。恢复见 §8.6。

**搜索走 `rg` 工具，不拼 bash 命令。** 两者结果一样，计划不一样：`rg` 的计划是一次**只读**动作（`Operation::ReadFile`，目标是搜索范围），所以它能和别的读同时在飞（§6）、能被 codemode 脚本调用（脚本里只放行只读与 MCP，§4）、恢复时可以直接重读（§8.6）；`bash` 的任意命令三样都不是。（2026-10-11 之前还有一条理由：`bash` 一律 Ask，只读搜索每次都停下来等人；审批删掉之后这条不再成立，§7。）HTTP 请求通过命令完成（需要库就 `bash` 跑 `python3`）；Git、构建、测试、HA 和记录查询都组合这些基础工具。

`rg` 内嵌 ripgrep 的**书库**——[`grep`](https://github.com/BurntSushi/ripgrep/tree/master/crates/grep)（匹配、搜索、打印）加 [`ignore`](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore)（遍历：隐藏文件、`.gitignore`、覆盖 glob）——**进程内跑**：不起子进程，机器上也不需要装 `rg`。要书库不要命令，是因为外部二进制要么没装、要么各家版本不同；而遍历规则本来就该用这套经过验证的实现，不该自己再写一遍。正则语法就是 Rust regex（只连 `grep-regex`，不带 PCRE2）。遍历与搜索是阻塞的，所以跑在 `spawn_blocking` 上、匹配经队列流式写进输出存储（§8.3），取消与超时靠一个停止标志让那边收手。**默认完整保存匹配，不设 8 MiB 存储上限；给模型的视图按 50 KiB / 2000 行投影**，仍是完成；**超时（默认 30s）也交回已找到的部分**，标 `timed_out`、状态记失败（没搜完和"有读不了的"一样是不完整的搜索），不是一句光秃秃的"超时"。一行最多印 500 列（`rg --max-columns 500 --max-columns-preview`）：JSONL 事件一行动辄几十 KiB，不截的话一行就吃掉整份预算，要看全行就 `read` 那一行。停止标志被写入器看见时报的错**不能是 `Interrupted`**——`write_all` 会把它当成"再试一次"原地重试，搜索线程空转、队列不关（2026-09-24 实测：`cron|schedule` 搜 `~/.komo` 的副本，写满 8 MiB 后线程在 `write_all` 里空转，跑满 30s 只拿回"超时"、线程也不退出；修后 debug 构建 4.2s 完成，截到 500 列后只写下 2.9 MiB；把上限压到 1 MiB 时 3.3s 返回截断结果）。

**`bash` 超时也交回已有的输出**（2026-10-11）：到点终止进程组之后，结果是 `timed_out: true` 的失败、正文抬头 `timeout=<N>s`，后面照常是到点前 stdout / stderr 的首尾摘录——与 `rg` 超时交回已找到的部分同一条。起因是真实会话里的轮询循环：`for … sleep 15` 跑满时限只换回一句"超时"，模型看不到最后一次查到的进度，只能把整个循环重跑。（2026-10-19 起工具描述不再教"一条命令里循环轮询"，等状态改挂 `monitor`，见下一条。）回归测试：`tools::shell::tests::a_timeout_hands_back_what_was_printed_before_it`。

**长等待不占住对话（2026-10-19）。** 同一会话严格串行，插话只在一轮的调用全部收尾后并入（§6），所以 home 里一条跑十五分钟的 bash 轮询会让用户的话排十五分钟（会话 01a151ec，2026-10-19：子代理里的 bash 轮询单次 815–900 s）。起初只改了工具描述，模型预测不了时长，没拦住；实测（2862 次 bash 调用）224 次跑过 60 s、18 次过 600 s，65 次带 `sleep`、其中 35 次在跑过 60 s 的那 224 次里。现在两道机制（`docs/monitor.md` §8）：① **自动转后台**：对话会话里 `bash` 跑过 `[execution] detach_after_secs`（默认 120，热生效，0 = 不转）还没完，调用立刻以「已转后台 #id」结算，活子进程交给 Gateway（与 `monitor` 同一个 id 空间，`monitor_stop` 停得掉），结束时退出码与输出尾部以 `bash-done:{call_id}` 内部输入交回会话；原 `timeout_secs` 从调用开始算；任务会话、Cron 的 Run、子 Run、codemode 内层调用不转；重启时没交回的补一条「结果不明」。② **对话会话里禁 `sleep` 等待**：`prepare` 拒绝字面量 ≥ 10 s 的 `sleep`，提示改挂 `monitor`；任务会话仍可 sleep。工具描述同步重写：慢命令照常写、`timeout_secs` 按预计给足，等状态挂 `monitor`，要接着做一串事派 `task`。

**后台任务（`task`）不是第七个工具，是一条编排操作（2026-10-16 定并落地）。** 模型把一件自包含的事交给一个**独立的任务会话**去跑，自己不等：

```text
task(prompt, title, task_id?, model?, effort?, contract?)
  无 task_id → 新开任务会话（kind = task，同一个 Agent），回"已派出 #2a9c：标题"，这一轮照常往下
  有 task_id → prompt 提交进那个已有的任务会话，它带着自己的历史接着做
  只有 task_id → 查进度：回那个任务最近一条 Run 的快照（状态、已跑多久、正在执行的调用与输出尾部），不提交任何输入
  任务的 Run 进终态 → 结果作为一条内部输入交回派它的会话（请求键 task-result:{run}，只交回一次）
```

- **不是子代理。** 跑任务的是派它的会话的同一个 Agent（§6.1：Session 绑定一个 Agent，任务会话不换人格）；它拿不到父的消息历史，因为它是另一个会话，不是因为另一套提示。任务会话按主 Agent 装配（记忆、skills 照常），能力面里**没有 `task`**——深度只有一层，结果只交回一层。
- **`model` / `effort`**：`model` 填 `[model.<alias>]` 的 alias，可选值以 enum 写进工具说明；`effort` 按所选模型的档位校验（§13.3）。都在 `prepare` 里按当前配置快照校验，不认识的拒绝并列出可选值（例外：只给 `effort` 不给 `model` 时，所选模型——Agent 的默认或那个任务上一条 Run 的——只有执行侧知道，档位在派出那一步校验，不合格同样是一条落账的失败结果）。可选 alias 是 `model` 参数的 JSON-schema `enum`，按每一段装配时的当前配置渲染。省略时，新任务用 Agent 的默认；接着做（有 `task_id`）沿用那个任务上一条 Run 的。它们透传给任务 Run 的受理（`SubmitRunRequest` 本来就有这两项），冻结进那条 Run 的快照（§6.2）。**交回父会话的那条输入不带它们**，按父会话自己的默认——任务用便宜模型，父的转告不跟着降级。可选值写进 schema 的代价：改 `[model.*]` 让工具前缀变化，而且 enum **不冻结进 RunSnapshot**——在飞的 Run 从下一段装配起也看到新的列表（提示缓存从工具定义起失效一次）。`[model.*]` 很少改，先不冻结；要冻结得把 alias 列表存进快照、改 schema 的渲染路径。
- **`contract`**：结构化结果的契约（JSON Schema，校验器认得的子集，与旧 `delegate` 的 `output_schema` 同一形状）。它冻结进任务 Run 的快照（`RunSnapshot.contract`），装配时补在那条 Run 的任务后面（系统提示不变）；交回之前用**同一份校验器**（`executor::check_contract`）复验任务的最后回复，复验结论写进交回的那条输入。
- **查进度（2026-10-19）**：只给 `task_id`、不给 `prompt` 是只读查询，计划是 `Operation::TaskStatus`、`SafeReread`（中断后可直接重查），不建会话、不受理 Run。快照取自任务会话的 JSONL（`fold::progress_of`，最近一条 Run：状态、已跑多久、轮数与调用数、上次活动、正在执行的调用与参数摘要、等什么、终态的结论），渲染成一段中文（`TaskProgress::render`）；正在执行的调用再带 stdout 尾部（至多读末尾 2 KiB、留最后 8 个非空行），读 `tool-output/<run>/<call>/<attempt>/stdout.txt.partial`（已发布就读 `stdout.txt`，stdout 为空退到 stderr），读不到不是错误。起因：会话 01a151ec（2026-10-19）里用户两次问"为什么要这么久"，没人说得出任务在做什么。主动报进度（任务自己隔一阵说一句）没做，见 `docs/background-tasks.md` §7。
- **Cron 触发的 Run 不能派任务**：交回 Cron 会话的那条结果没有对端也没有看客，回复投不到任何人。`prepare` 拒绝，理由让它自己把事做完。
- **同一轮派几个就是几个在并行**，各自交回；宿主还在跑时并入（§6 插话），否则各开一条 Run。"派几个、等齐了汇总"因此不在同一轮里完成。
- 交回、重启补挂看客、"需要你判断"的投递见 `docs/background-tasks.md`。

**取代 `delegate` / `dispatch` / `follow` 三个。** `dispatch` + `follow` 就是 `task` 的两种用法；`delegate`（同步委派：在同一个 Session 里受理一条子 Run，父 Run 进 `waiting + dependency` 等它，靠领取豁免让子 Run 越过父先跑，`resume` 把几次委派连成一条线，父侧用 `Ledger::run_end` 收尾）**删掉**——不阻塞的子 Run 不能留在父的 Session 里（同一 Session 严格串行，父不等它就排在父后面），挪出去就是任务会话；它穿进核心的那些东西是 §2 那条规则要去掉的。**旧记录照常解码与回放**：`delegate` / `dispatch` / `follow` 调用按落盘时的工具名回放，不改写（与 `shell` / `bash` 同一做法）；`Operation::Delegate`、`DelegateSpec`、`runs.parent_run_id` / `runs.delegate` 只为解码旧行保留（列照写空，§8.2）；升级时还停在 `waiting + dependency{子 Run}` 的父 Run 与没跑完的子 Run，按删之前的路径收尾（子 Run 照跑完、父按 `run_end` 收那次调用），之后不再产生。

**`update_plan` 已退役（2026-10-16）。** 它曾让模型报一份整份的工作计划，唯一的消费方是在线压缩的"计划边界"；压缩改为只按上下文窗口触发之后就没有用处了，工具、提示里那句话、计划折叠一并删掉。`Operation::UpdatePlan` 留着只为旧日志里的计划还能解码：fold 不认它，旧日志里在途的那次调用不再执行，按工具不可用（或 §8.6 的结果不明）收尾交给模型。

**`codemode` 是组合，不是新能力。** 模型写一段 Python，在 Monty（pydantic 的 Python 子集解释器）里跑，跑在 komo 自己起的 worker 子进程中——语言本身没有网络、子进程，文件只读（宿主 `/` 只读挂载，komo 自己的状态除 `sessions/` 外连读也拒，写类一律拒；要写文件走 `write` / `edit`，要完整 Python 走 `bash` 的 `python3`（`python` 工具已于 2026-10-15 删除）；2026-10-15 起取代此前的 CPython + 操作系统沙箱：Seatbelt / Landlock + seccomp，以及启动自检），脚本里经 `tools.<name>()` 调其他工具——放行 `Operation::ReadFile`（`read`、`rg`）与 `Operation::McpCall`（含写操作）的计划；本地 `write` / `edit` / `bash`、带 `then_run` 的、编排操作与 codemode 自己仍要在脚本外调。只有脚本的输出回到模型。2026-10-11 之前还要"Policy 直接 Allow"；Policy 删掉之后范围外的读也放行（与当时的 auto 一致），边界是语言本身（§7）。**脚本里的子调用逐次持久化**（2026-10-13，此前只放行只读、整段重跑）：每次子调用按 intent → started → delivered 记成 `codemode.journal` 事件、正文外置到 `tool-output/`，fold 不看它，不生成顶层 `tool.result`，模型只看见外层结果。中断后整段重跑脚本：已交付的子结果按序原样接回，started 而没交付的按双 Safe 准则判（§8.6），unsafe 的那次绝不重做；脚本的时钟与随机数经宿主、记进 journal，恢复时原样还回去，所以重跑是确定的，越过已记录的前缀之后新调用照常执行（2026-10-15）；分歧（工具名 / 参数 / 读数对不上）或结果不明整段记 uncertain 交给模型。因为脚本里可能有 MCP 写，codemode 不再算只读计划（`is_read_only()` 为 false），是同一轮里的屏障（§6）。见 `docs/codemode.md` §5、§7。

**MCP 工具也不是第七个工具，是操作者接进来的外部能力。** `[mcp.servers.<name>]` 里配的服务器在 Gateway 启动时连上（要登录的，登录后下一条 Run 受理时连上，不用重启；`docs/mcp.md` §2.2），每个工具注册成 `mcp__<server>__<tool>`，与内置工具走同一条 `prepare` / 落账的路；只读与否由操作者声明，不信服务器自报——它决定恢复时能不能直接重读（§8.6）。codemode 脚本里 MCP 工具都能调，只读声明只决定中断后能不能重做（2026-10-13 之前它还决定 codemode 能不能调）。**默认不声明给模型**（2026-10-14，照 pi 的 `exposure`）：`exposure = "codemode"` 的 MCP 工具不进 schema、只在 codemode 脚本里调，脚本用 `search_tools` / `describe_tool` / `describe_namespace` 找；系统提示只给每台服务器一行摘要（配置的 `description`，没写就是服务器 instructions 的第一行），instructions 全文只在脚本里读。`direct` 照常声明，`hidden` 不挂。见 `docs/mcp.md` §3。

工具执行的公共能力放在 ToolExecutor：参数校验、执行计划生成、执行状态保存、取消和输出限制。

**每次运行有它自己的能力面（`AgentSurface`）。** 这次允许调用哪些工具是一个显式的集合：**交给模型的工具 schema 与执行时查找的表出自同一份能力面**，所以两边不可能分家。schema 是能力面的投影：`exposure = "codemode"` 的 MCP 工具在能力面里、不进 schema（只在 codemode 脚本里调，`docs/mcp.md` §3）——隐藏 schema 不等于撤权。能力面里没有的名字，执行器不认——模型自己拼出这个名字，拿到的是"这次运行的工具集里没有它"，而不是"进去试试看"。执行器手里那份全局工具目录只用于**发现与构造**（`komo skills` 的 `requires_tools` 门控也问它），**执行时一次都不回退到它**：能回退就等于能力边界只是一句建议。任务会话是这条边界的使用者之一（它的 schema 里没有 `task`，于是它也调不动 `task`；2026-10-16 之前是子代理的能力面里没有 `delegate`）。

工具**能不能**调用由能力面决定；一次**具体调用**能不能做由工具自己的 `prepare` 决定（参数、版本、目标；§7 那条硬限制也在这里）。没有第三层：能力面给了、`prepare` 过了，就执行（§7）。

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError>;

    async fn execute(
        &self,
        plan: ExecutionPlan,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;

    /// §8.6：这个工具现在声明的重放策略。started 而无结果的调用，计划里记下的与这里的
    /// 都是 Safe 才重跑；否则中断结果交给模型。默认 Unsafe，read / rg / codemode /
    /// 只读 MCP 覆盖成 Safe。
    fn replay(&self) -> Replay {
        Replay::Unsafe
    }
}
```

这是接口示意，辅助类型省略。prepare 可以解析参数和检查元信息，但不能通过运行命令等方式提前执行代码——`prepare` 没有副作用，副作用只在 `start_call` 之后（§8.5）。

`ExecutionPlan` 带 `recovery: RecoveryMode`（§8.6，决定计划里记下的重放策略）与 `plan_hash`，由 `prepare` 填写，模型参数不能指定。`execute` 按值收 `ExecutionPlan`（2026-10-11 之前收的是只能凭 Policy 允许或审批换来的 `ApprovedPlan` / `Proof`，随 Policy 一起删掉，§7）。

ToolContext 由 Gateway 创建，包含 Session / Run / ToolCall 身份、工作目录、只读根、受保护路径名单（§7）和取消信号，不能由模型参数提升。

bash 子进程使用明确的环境变量集合。取消时停止所属进程组并等待回收，而不仅仅是丢弃异步等待。Tokio 默认不会因为进程 handle 被丢弃就终止进程。[Tokio process](https://docs.rs/tokio/latest/tokio/process/index.html)

### 资源命名空间：`skill://` / `tool://` / `artifact://`

模型仍然只用现有的 `read` / `rg`，把资源 URI 当路径交给它们。**URI 不是"特殊文件路径"**：它在执行计划里是一个明确的目标类型（`TargetRef`），计划同时带着**逻辑** URI 与解析出来的**真实**目标——审计与界面显示的是"读了哪个逻辑资源"，越界与只读根的判断看的仍是真实路径。

```text
skill://<skill>/<path…>                      → 那份 skill 的目录 + <path…>（只读）
skill://                                     → 配置里的 skill 根（`rg` 的搜索根）
tool://<tool>/schema|doc                     → 虚拟入口：内容由运行时按能力面现算
tool://                                      → 这次能力面里的工具名
artifact://files/<path…>                     → sessions/<id>/artifacts/<path…>（只读；产物按 Run 分目录，所以通常是 `files/<run>/<名字>`）
artifact://<run>/<call>/<attempt>/<stdout|stderr|result>
                                             → sessions/<id>/tool-output/…（只读）
```

- **`tool://` 是说明入口，不是执行入口**：读它不改任何状态；真实动作仍走 native tool call 与 `ExecutionPlan`。
- **越权当场是错**：`..`、空段、`tool://` 里能力面之外的工具、别的会话的 `run`、没有会话目录，一律在 `prepare` 就拒绝（`ToolError::Failed` 说清原因），**不进计划就没有执行**；解析链接之后还要再核一次"仍在挂载点里面"。
- **挂载由装配决定，不能由模型参数提升**：skill 根来自当前配置快照的 `paths.skill_dirs`，会话内容目录来自这一次的 Session，`tool://` 只认这一次的 `AgentSurface`。三者随 `ToolContext` / `CallEnv` 走。
- **计划里的两种目标**：本地路径（序列化成 `{"path": …}`，与改造前逐字相同——旧计划的哈希不变）与资源（`{"uri": …, "resolved": …}`）。**没有磁盘目标的读**（`tool://`）不编假路径——给虚拟入口编一个假路径才是真的越权口子。
- **首阶段不做**：memory / session 的任意写入、bash 里的 URI 展开、通用 VFS。本地路径的用法一个字都不变。

产物（2026-10-15 python 工具删除后没有新的写入方，下面是历史记录的形状）由运行时登记：工具结果的 `artifacts` 引用记的是 `<会话内容目录>/artifacts/<run>` 里的文件（**相对会话目录的路径**，落在 `output.json` 的 `body.artifacts` 里），投影那一处把它映射成 `artifact://files/<相对 artifacts 的那一段>`（映射函数在 kernel，落盘与投影共用同一个，§8.3）。工具输出那一条的引用直接由 §8.3 的投影给出（`artifact://<run>/<call>/<attempt>/stdout`），重启之后照样读得回来——它是**按引用回放**，不是把正文再抄一遍。

## 5. 能力保存

### 5.1 / 5.2 Python 环境与 `python` 工具（已删除，2026-10-15）

`python` 工具与 Gateway 管理的 Python 虚拟环境（`python_runtime`、`python-envs/`、`[paths] python_env_root`、`PythonHost`、`env_version`）整体删除，没有迁移层。聚合与过滤用 `codemode`（Monty，文件只读，§4、`docs/codemode.md`），需要完整 Python（第三方库、写文件、联网）就用 `bash` 跑 `python3`；可复用的集成写成 Skill（§5.6）。章节号保留，是为了不让别处的 §5.3 以后的引用错位。

旧会话里的 `python` 调用照常读得出来：`Operation::PythonCode` 与 `PlanVersions.env` 只为解码保留（`komo-kernel/src/events.rs` 的测试用真实形状的旧计划钉着：解码、重新序列化一致、计划哈希不变；`projection.rs` 的测试钉着历史 `python` 结果的回放逐字节不变）。TUI 仍按名字渲染历史 `python` 调用的代码。

### 5.3 toolbox（已删除，2026-10-15）

「把 Python 模块保存进 toolbox、候选测试后启用、用 `python` 的 `call` 模式调用」这一整套已删除：`toolbox/` 目录、`komo toolbox` 命令、`/v1/toolbox` 路由、`Operation::PythonCall` 与 `call` 形式的 `PythonJob`、`PlanVersions.module`、`[paths] toolbox_dir`、`python` 子进程的凭证注入与 import 禁区钩子，都不再存在，也没有迁移层。可复用的集成改写成 Skill（§5.6）：`SKILL.md` 写步骤，脚本放在 Skill 目录里，模型用 `bash` 运行。

凭证边界不变（别处引用 §5.3 指的就是这条）：原始凭证不放进模型提示词、不进计划、不进事件、不打印到对话或日志，也不进服务单元文件（§3）；`bash` 子进程只拿显式的环境变量集合，不继承 Gateway 的环境。

### 5.4 AI 迭代流程（已删除，2026-10-15）

随 §5.3 一并删除。章节号保留，是为了不让别处的 §5.5、§5.6 引用错位。

### 5.5 Memos 作为主动记录的原文来源

用户说“记一下”“保存这段内容”时，Agent 按 Memos skill 的说明用 `bash` 运行其中的脚本，写入 Memos。脚本提供创建、查询、读取、修改等操作；成功后返回记录 ID 和原文链接。

查询用户主动保存的记录时，通过 Memos skill 的脚本查询 Memos 并读取原文。自动 Memory 可以保留相关摘要与 Memos 引用，但不建立第二份完整笔记库，也不把模型对笔记的推断视为用户确认。

Memos 使用用户访问令牌进行 API 认证；接入时核对用户部署的具体版本，不能把 latest 文档当成该实例的接口保证。[Memos API](https://usememos.com/docs/integrations/api-access) · [查询接口](https://usememos.com/docs/api/latest/memoservice/ListMemos)

写入失败要明确报告；请求已发出但响应丢失时标记 uncertain，先核对远端结果，不能直接重试导致重复记录。脚本不得在 Memos 不可用时静默改存本地并声称已完成。

### 5.6 Skills：程序性说明

Skills 是**人写的程序性说明**——"做 X 时按这几步、跑这个脚本"。脚本放在 Skill 目录里，由 `bash` 运行；komo 不另设"已保存的可执行能力"这一层（toolbox 已于 2026-10-15 删除，§5.3）。

```text
~/.komo/skills/<name>/SKILL.md   主目录
~/.agents/skills/                与其他本地 agent 共享，只读
```

只认这两处（加上配置里显式声明的目录）。`<workspace>/skills`、`~/.claude/skills` 不扫：根多了，模型拿不准 skill 在哪个根下，会把每个根下的同名 `SKILL.md` 都读一遍（真实会话里 `~/.claude/skills/sls` 是指向 `~/.agents/skills/sls` 的链接，同一份 16 KB 正文进了两次上下文）。

- `SKILL.md` frontmatter：`name`、`description`，可选 `platforms:`、`requires_tools:`（对工具名）。
- 搜索路径有序，**同名先到先得**；每次查询重扫目录，编辑或新增无需重启。
- 只有系统提示里的**目录行**（名字 + 一句描述，总量有上限）是受理 Run 时的快照，为了提示前缀稳定；`platforms:` / `requires_tools:` 只门控这份目录，不门控加载。实现上这一块是两行开头加若干目录行：先给**按序的根**（只列真的出了条目的那些，顺序就是搜索顺序——"同名先到先得"因此落得下来），再给目录行；模型按这个顺序去找 `<根>/<名字>/SKILL.md` 并 `read` 它。一条能露面的都没有时这一块整个不出现（不留空标题）。受理一条 Run 时渲染一次，正文冻结进 `RunSnapshot.skills_ref`（§6.2），这条 Run 的每一段（包括等介入、等退避之后的续跑）都逐字用它——目录行是提示前缀的一部分，不能一段一个样；`SKILL.md` 的增删改、`disable` 名单、配置重载都从下一条 Run 起生效（2026-10-14 之前每一段都读活注册表，"启动时算一次"只是这里的说法）。**整批装得下「名字 + 一句描述」就用它，装不下就只留名字**——列全比列得详细要紧：166 个 skill 的实测里，2000 字符的预算只留下字母表前 16 条，`log-diagnosis` 根本没进提示，模型为了找它去 `ls` 了整个目录；只留名字是 2752 字符、装得下全部（默认上限已按这个实测改成 4000）。名字都装不下时末尾会写「另有 N 条没列出来」：**「没有它」和「没列出来」是两件事**。
- **没有 skill 工具。**模型通过 `read` 读 `SKILL.md`——写本地路径，或者写 `skill://<名字>/SKILL.md`（§4.7，逻辑入口，审计显示的是它）；skills 目录是只读根（§4.7 的资源根也在 `ToolContext.roots` 里）。
- **目录行的门控按这条 Run 的能力面**：某个 Agent 的面里没有 `bash` 时，`requires_tools: [bash]` 的 skill 不进它的目录行；而 `skill://` 仍然读得到——门控管的是"要不要摆在眼前"，不是"能不能读"。
- Cron Job 可以声明 `skills = ["…"]`，触发时把这些 SKILL.md 正文预载进首轮上下文。
- `komo skills list | inspect <name> | enable | disable`；`disable` 只从目录行隐藏，不删文件。没有安装 / 候选 / 治理流程——Skills 由人写、由人放进目录。

## 6. Agent Loop

```text
持久化用户输入或 Cron 触发
  → JSONL 持久保存输入，state.db 提交可执行 Run 后确认接收
  → 调度器领取 Run
  → 从 Session 读取上下文
  → 固定本次 Run 的模型配置快照
  → MemoryManager 按当前任务召回有效记忆
  → 请求 LLM
      └─ provider 以上下文超长拒绝 → [有 planner] 强制压一次 → context.compacted → 按日志重新装配接着跑
                                     （刚压过还是超长 / 切不出 / 摘要没成 → 按原错误失败）
  → 接收完整一轮回复
      ├─ 有 tool calls
      │    → 将完整 assistant 回复和调用计划追加到 JSONL 并同步落盘
      │    → 逐个 prepare → execute
      │    → 先持久保存独立 tool output，再追加 JSONL 结果引用
      │    → 提交 state.db 状态与索引
      │    → [在线压缩开着] 贴着窗口：判压不压
      │         ├─ 不压（边界上记 skipped）→ 原样往下
      │         └─ 压 → 摘要请求 → context.compacted → 按日志重新装配，同一个 Run 接着跑
      │    → [插话] 排在后面、要求插话的输入 → run.absorbed → 按日志重新装配，同一个 Run 接着跑
      │    → 按 call_id 回传结果
      │    → 下一轮 LLM
      └─ 正常结束且无未完成调用
           → 将最终回复与完成事件写入 JSONL 并同步落盘
           → state.db 提交 Run 完成
           → 标记 Memory 整理任务待处理
           → 后台提取、校验、持久化并更新索引
```

工具名称和参数来自 provider 原生字段，不从自然语言或代码块推断。完整 assistant 消息与每个工具结果保持调用 ID 配对，这是原生 function calling 的执行方式。[Function calling](https://developers.openai.com/api/docs/guides/function-calling)

**同一轮的多个调用：只读的可以同时在飞，其余是屏障。** 够格的是 `Operation::ReadFile` 这一类计划（`read`、`rg`）里**没有上一世要接**的那次调用——续跑里"已经 `start` 过"的那些要走重放判定（§8.6），顺序由账本钉死；`write`、`edit`、`bash`、`codemode`（脚本里可能有 MCP 写，§4），`task`（旧记录里的 `delegate` / `dispatch` / `follow`）这些编排调用，以及任何会停下来的判定（核对 / 取消），都是屏障——屏障之前已经在飞的先收尾，再按调用顺序做它。"做它"是 `start_call` 及之后的一切：执行工具、受理子 Run、派任务、转话都等收尾之后；前面在飞的那条停下来时，屏障还没开始过，原样留在没轮到的那些里。同一轮的只读调用全部同时在飞，不设上限（2026-10-15 去掉了原来默认 4 条的上限，照 pi 与 Claude Code）。屏障按顺序做，所以同一轮对同一个文件连发的几个 `edit`，后一个在前一个改完之后才做计划、绑定版本，不会互相版本冲突（2026-10-21 起系统提示让模型改同一文件的多处用一次 `edit` 带多个 `edits[]`，§4；连发的照样成立）；互不依赖的读，系统提示让模型同一轮一起发。两条不变量不因为并发而改变：`start_call` 返回之后才允许产生副作用（§8.5）；完成事件按真实完成顺序落账，交给模型的那一份按**原始调用顺序**配对。

**在线压缩（`[compaction]`，默认关，2026-10-09）。** 移植自 SoL-Pi 的 Online Context Compact，但 loop 是 komo 自己的，所以不需要"中止再补一条隐藏的续跑消息"。**只按上下文窗口触发（2026-10-16）**：一轮的调用**全部收尾之后**、估计的上下文贴着窗口（`context_window − window_reserve_tokens` 与 `max_context_tokens` 先到的那个；没配 `context_window` 时只按后者，字节 / 4 的估计与实报用量取大）才判；段开头有没收尾的调用时不压，续跑那一轮收尾之后 loop 按同一份估计再判。此前还以 `update_plan` 完成一个计划步骤为边界、在边界上按"每步花几次请求 × 剩余步数"算账压，该工具退役后那条路一并去掉（`compaction::decide` 的按账部分留着，调用方给 `horizon = None`）。Gateway 按日志重新装配出 provider 正看着的那份视图（投影、衰减之后）估价，`find_cut` 切在一轮的开头、近期至少留 `keep_recent_tokens`，贴着窗口不看账（窗口保护），冷却、旧债照记；压完仍贴着窗口就不压。`max_context_tokens`（2026-10-15，默认 150k，与 Anthropic API 服务端压缩的默认触发点相同）的起因：1M 窗口的模型要到约 98 万才贴窗口，而实测（会话 01a13edb-37b6-7d20-ab78-375b84f2307e）一条 Run 90 轮没报计划进度，上下文从 35k 涨到 302k，这一条 Run 就读了 1940 万 input token。**决定不压时 driver 原样接着跑**——下一次请求与关着压缩时逐字节相同，提示缓存照样命中，什么都不记。决定压时发一次摘要请求（没有工具，早先那段渲染成一条纯文本用户消息，带开头那句任务；系统提示说明里面是数据不是指令），成了记 `compacted` 并把这一段交回 handler（`SegmentOutcome::Recompose`）：同一个 Run、不重新入队、不再写 `run.started`，下一段按日志装配成**任务 → 摘要 → 切点起原样**；失败、截断记 `skipped`（带着那份决策），Run 照常往下，此后窗口压力不再触发重试，直到有真正的工具工作（一次完成了的调用）或一次成功的压缩。两次压缩之间至少隔一次真正的请求；一次领取里最多重装配 8 次；摘要请求不算轮数，但算进 token 花销，已用的轮数与 token 从下一段的预算里扣掉。

**溢出恢复（2026-10-21）：provider 以上下文超长拒了请求，强制压一次再接着跑。** 估计是字节 / 4，可能比 provider 的分词低，窗口保护会来不及；此前这种拒绝让 Run 直接失败。识别只看错误文本：`LlmError::Rejected` 的 message 先排除限流类（`^(Throttling error|Service unavailable):`、`rate limit`、`too many requests`），再匹配各家的超长措辞——两张表逐条移植自 pi 的 `utils/overflow.ts`（`is_context_overflow`，`komo-runtime/src/llm`）；pi 另外认的 silent overflow（用量超窗口却没报错）与 length-stop 不做。**识别先于重试判断**：有的代理把超长包成 500，照 5xx 退避只会把同一份超长的上下文再发几遍，所以认出超长时 `retry_cause` 给 `None`。`begin_turn` 或 `next` 拿到超长、这一段又有 planner，loop 问 `CompactionPlanner::plan_overflow`：Gateway 重新装配视图，按 `keep_recent_tokens` 切，**不看缓存账、不看冷却、不判压完贴不贴窗口**，发摘要请求，成了记 `compacted`、`Recompose` 接着跑。**只救一次**：上次压成摘要之后还没有一次成功的请求（`OnlineState::requests_since_last_summary() == Some(0)`，衰减不算）就不再压，Run 按原错误失败；一次领取里的 8 次重装配上限照样兜底（到上限 handler 拿掉 planner）。切不出可收的一段（比如只剩最后一轮）、摘要不比被收的那段小，都照常失败，不拿衰减兜底；摘要请求失败记 `skipped`，Run 按原错误失败。**不受 `[compaction] enabled` 管**：不压的话 Run 反正失败，所以只要不是命令 Run、接了配置，就构造 planner；`enabled`（与衰减）只管自动判断，两者都关时 planner 的自动询问直接答"不压"、不读日志。**事件里记成 `reason: "window_protection"`，另加 `decision.context_overflow: true`**，不新增 `CompactionReason` 变体：`reason` 是封闭枚举，`event_payload!` 解得出类型、解不出正文时按损坏处理（不会落成 `Unknown`），旧二进制读到新变体就读不了整本日志；多一个字段则旧二进制照常忽略（`deny_unknown_fields` 没有用），新二进制读旧行缺省 `false`。只因输出到了 `max_tokens` 而截断的回复不在此列：那是输出上限，压缩救不回来。

**衰减共用这个决策点（2026-10-10）。**工具结果换短视图（§8.3）是对前缀的改写，与压缩同一本缓存账，由 `compaction::decide` 一并判断，时机的优先级：**缓存已冷**（距上一次 provider 请求超过 5 分钟，改写不欠债，有资格的全换）> **贴着窗口**（先换短视图，没有可换的才压缩）。其余时候前缀只追加（2026-10-16 之前还有第三种"计划边界"，随 `update_plan` 去掉）。

不同 Session 可并发，同一 Session 的 Run 顺序执行；停在等待上（核对、依赖）的 Run 保留会话顺序位置，不允许后续 Run 越过它执行。等待运行排到前面时才装配上下文，避免读到过期历史。

**插话（2026-10-15，只在 TUI）。** 顺序执行的代价是"在跑的时候说一句话要等它跑完"：一条监听事件的 Run 接着排查了两分钟，用户那句话就在后面等了九十秒（会话 01a13911，2026-10-14）。照 pi 运行中按 Enter 的做法，TUI 打的话带 `interject`（`SubmitRunRequest::interject`，记在 `run.accepted` 上；`!` 命令不带，聊天渠道不带——渠道投递按 Run 记账，§11.3 照旧排队）。正在跑的那条 Run 在**一轮的调用全部收尾之后、下一次请求模型之前**问一次账本（`Ledger::absorb_interjections`）：排在它后面、还在等领取、要求插话、模型与 effort 快照与来源对端都与它相同的顶层输入，按受理顺序并进来——每并一条先追加 `run.absorbed { into }`（写在被并走的那条上）并同步，再把那一行提交成 `absorbed`；然后这一段交回去按日志重新装配（`SegmentOutcome::Interjected`，与压缩的 `Recompose` 同一条路，但不占压缩的重装配次数），同一个 Run 接着跑。那句话在回放里**就在并入的那一刻**：上一轮的结果之后、下一轮回复之前；原来那条输入不再单独回放。前缀只追加，提示缓存照样命中。轮不上的照常排队：宿主不在跑（停在等待上、还没领取）、这一轮是最终回答（没有"下一次请求"）、宿主已被取消、命令直跑的 Run（§10）、子代理、模型或 effort 不同。**`task` 交回的结果也并（2026-10-16）**：交回的那条内部输入带插话标记（带着任务 Run 的来源对端），宿主还在跑、且来源对端与宿主相同时在同一个边界并进来——对端不同不并，否则回复会投到宿主的对端上（这条对端规则对所有插话都适用，TUI 的插话没有对端，与 TUI 宿主一致），不另开一条 Run——这对应 Claude Code 把后台完成通知在工具轮之间并入；监听（`monitor`）的事件暂不并，照常排队。交回的输入不带 `model` / `effort`（§4），快照是父会话的默认，与宿主不同时照常排队。被并走的那条是终态 `absorbed`（§8.4），不再开跑，回答在宿主那里；TUI 的状态行跟宿主，回滚区不再重印原话，只在并入处记一行"↳ 已并入当前回答"。

Gateway 设置活动执行时限、输出长度和子进程并发预算。总轮数默认不设上限：任何一个数在真活上都会撞到，撞上就是一次失败的 Run；停一条 Run 靠取消，单个 Cron Job 要上限用自己的 `max_rounds`（§10）。等待用户时释放运行名额，保留已消耗预算。

普通工具失败作为结果交给模型修正；模型回复截断或调用参数未收齐时不能开始执行。远端写入结果不明时停止自动重试，结果连同判断方法交给模型，由它核对目标后决定重做还是往下走（§8.6）。

普通追问可以作为 assistant 回复结束本轮；用户下一条输入开启同一 Session 的新 Run。没有工具审批（§7）；只有阻塞会暂停原 Run（结果不明交给模型判断，2026-10-12），待操作者回答后继续，不增加交互工具；回答通常来自聊天渠道（§11）。

**系统提示告诉模型它就跑在 komo 里（2026-09-24）。** 主 Agent 的提示在工作目录 / 工具 / 行为约束之后、skills 目录之前多一小段：cron、会话与 Run、记忆、待处理的介入、配置、skills、Gateway 都是 komo 自己的状态，问到就用 bash 跑 `komo <子命令>`（给出几个常用子命令，其余看 `komo --help`）。这一段是**常量**，提示前缀因此稳定；`komo` 靠 PATH 找到——服务单元写入了安装时的 PATH（§3），不在提示里另给可执行文件路径。只在主 Agent 且挂了 `bash` 时出现：子代理只拿任务里写的东西（与 skills 目录同理，父侧需要时把命令写进任务）；没给 `bash` 的 Profile 说了也跑不了。起因是一次真实会话：问"当前有哪些 cron job"，模型不知道自己是 komo，先读了同名的共享 skill `cron-scheduler`，再翻 `crontab -l`、`/etc/cron.*`、`launchctl`、`~/.komo`，第 8 轮才跑到 `komo cron list`，9 轮 90 秒。回归测试：`context::prompt::tests::the_main_prompt_tells_the_model_to_query_komo_with_its_cli`、`the_komo_section_needs_shell_and_the_main_agent`，golden `full_main_agent`、`plain_main_agent`。

Run 完成仅表示本轮结束。测试是否通过、性能是否改善、设备是否到达目标状态，都要依据具体执行证据报告。

### 6.1 Agent、Session 与 Run：三种身份

| 对象 | 是什么 | 住在哪 |
| --- | --- | --- |
| `AgentProfile` | 一个助手的长期定义：身份指令、模型、能给的工具、允许的 Skill、工作目录、记忆模式与作用域、要不要项目文件 | 操作者写在 `config.toml` 里 |
| Session | 一段持续对话，**绑定一个 Agent**（`sessions.agent_id`），不随消息路由改人格 | `state.db` 的行 |
| `RunSnapshot` | 某一次执行受理时冻结的身份与能力 | 账本里那次受理上 |

**没有隐含的默认助手。** `default_agent` 指名"没有归属的入口（TUI、CLI、Cron）走谁"，`[agents.<id>]` 是唯一写法：一个都不声明就是**配置不完整**，`komo config check` / 启动会拒绝，并把该写的形状印出来。只声明一个 Agent 时它就是 `default_agent`——那不是隐含默认，是没有第二个答案。

```toml
default_agent = "assistant"

[agents.assistant]
instructions = "你是家里那个助手，回答用中文。"
model = "chat"                     # [model.<alias>] 的 alias；省略 = [models].default
tools = ["read", "rg", "codemode"]   # 省略 = 目录里全部；[] = 一个都不给
workspace = "home"                 # 相对数据目录；省略 = [paths] workspaces_dir
memory_scope = "personal"

[agents.coder]
instructions = "你在仓库里干活，动手前先把计划说清楚。"
tools = ["read", "write", "edit", "rg", "bash", "codemode"]
workspace = "code/komo"

[agents.guest]                     # 群里的访客（§11.3）
tools = []
memory = "private"                 # shared（省略）/ private / off
project_files = false              # 省略 = true
```

**记忆模式与项目文件（2026-10-22）。** `memory`：`shared`（省略）= 召回 `memory_scope` + Personal，没写 `memory_scope` 就是全部作用域——**但不含任何 `agent:*`**；`private` = 这个 Agent 自己的 `agent:<id>` 作用域，召回只看它、不并 Personal，提取前的候选召回也只看它，提取结果不管模型选了什么都由 Rust 改写成 `agent:<id>`，别的 Agent 的召回与提取候选都看不见它（双向隔离，判断只有 `memory::scope_allowed` 一处）；`off` = 不召回，提取当场结案、不调模型。`private` 与 `memory_scope` 不能同写，任何 Profile 的 `memory_scope` 都不能写 `agent:...`——那一格只属于 `private`。模式冻结进 `RunSnapshot.memory_mode`（旧快照读回 `shared`），续跑与提取都按受理时那一档；带过来的旧注入按现在放行的作用域重新过一遍。`project_files = false` 不做项目文件的发现（发现会从工作目录一路往上到 `/`）。操作者看访客记忆要显式 `komo memory list --scope agent:guest`。

两个字段各带一次"省略 ≠ 空"的区分，写在类型里（§4 的能力面）：`tools` 省略 = 目录里全部，`[]` = 一个都不给；写了目录里没有的工具名**不算数**，装配时 warn 出来，模型看到的 schema 里也不会出现它（`AgentProfile::surface` 把不认识的名字单独返回，就是为了不静默采纳一份写错的配置）。

**Session 的归属创建后不变。** 换助手 = 路由到另一个 Session，而不是给已有会话换人格、同时留着原来是另一个助手的上下文。每个 Agent 的主会话（`kind = 'main'`）在数据库里唯一：两个入口并发地要它，只落一条。

**升级路径**：`sessions.agent_id` 为空 = 升级前建的会话。它按 `default_agent` **归属一次并写下来**（只写一次，之后不随配置漂移）；归属的 Agent 在现行配置里不存在时，那条会话**拒绝执行**并进操作者清单（§7.5），不静默换成别的助手。

**后台任务。** home session 用它自己的 Agent 跑，不另设分发器身份；要跑很久的事由模型调 `task`（2026-10-16 之前叫 `dispatch`）派进一条独立的任务会话（同一个 Agent），结果收尾时作为一条内部输入交回派它的会话、由它转告，见 `docs/background-tasks.md`。

**监听。** 要"每出一件事就知道"（MR 流水线的每个 job、工单的每次状态变化、日志里的每条报错）时，模型调 `monitor` 在 Gateway 里挂一个长时间跑的脚本：它的 stdout 每出一行（按批合并，同一个监听同时只有一条在飞）就作为一条内部输入交回挂它的会话，脚本退出或超时时再交回一条结束消息；`monitor_stop` 停掉它。监听默认不跨重启：停机终止脚本，下一代启动时告诉会话"中断了"，不自动重挂；模型挂的时候声明了 `resume_after_restart`（脚本只读、重跑无副作用）的，下一代按原短号、剩余时限重新起它，见 `docs/monitor.md` §5。

### 6.2 冻结的是身份

受理一条 Run 时算出 `RunSnapshot` 落进账本：身份指令**正文进 `payloads/`（存内容，不存路径）**——一条停在核对上的 Run 可能一小时之后才续跑，那时那个文件早被改过——项目规范文件（从解析过的工作目录往上找到的 `AGENTS.md` / `CLAUDE.md`，`project_files_ref`，只给主 Agent 的提示，见 §14）同样存正文；系统提示里的 skills 目录（§5.6，按这次能力面门控后渲染好的那一块，`skills_ref`）也存正文——续跑那一段的系统提示要与受理时逐字相同，不然前缀缓存从系统提示起整段作废，注册表的变化从下一条 Run 起生效（2026-10-14 之前每一段都读活注册表）；连同 Agent 的 id、Profile 的内容指纹、模型、解析过的目录、这次的能力面与记忆作用域，以及工具结果的投影设置（`[execution]` 的 `model_result_bytes` 与 `decay_*`，§8.3）——刚跑完与回放按同一份渲染，热重载只影响之后受理的 Run；这一格出现之前受理的 Run 没有它，只有它们按当前配置。`[compaction]` 不冻结：每次判断读当前快照，因为回放不重算——决定连同全部中间量记在 `context.compacted` 里。

于是**等待期间改 Profile，在飞的 Run 不受影响**（它按自己的快照恢复），下一条 Run 用新的一版。（2026-10-11 之前这里还有后半句"撤权与 Deny 不冻结，每次执行按当前 Policy 判"；Policy 删掉之后没有要每次重判的东西，§7 那条硬限制由工具在 `prepare` 里判，本来就不进快照。）

## 7. 执行与介入：没有 Policy

**2026-10-11 删掉了整套 Policy 与审批**：规则表（strict / auto 两套基表、`policy.toml`、`command_patterns` 那张网）、范围授权（本次调用 / 本次 Run / Cron Job）、绑计划哈希的审批与它的状态机、批量答复、`Proof` / `ApprovedPlan` 类型状态，全部不再存在。理由是数据，不是偏好：komo 是单操作者实例，操作者一直跑 `mode = "auto"`（`~/.komo/policy.toml` 一条 `Ask` 都没有）；账本里 152 次审批**全部**落在 2026-09-18 一天（auto 出现之前），148 次批准、4 次拒绝，等待中位 3.2 秒、p90 65 秒——审批没有在判断什么，只是一次按键。而 auto 之下本来就什么都不拦，连写 `state.db` 都放行；删掉之后的行为与当时的 auto 等价，只多一条挪进工具里的硬限制（§7.2）。

### 7.1 规则：模型要的，komo 就执行

一次调用只过两道，过了就执行：

1. **能力面**（`AgentSurface`，§4）：这次运行的工具集里没有这个名字，执行器不认。
2. **工具自己的 `prepare`**：参数校验、目标解析、版本检查（覆盖已有文件要对得上版本，`call` 模式只认已启用版本的导出函数）、资源 URI 越界（§4.7）——这些是"这次调用说得通吗"，不是"允不允许"。不过就是一次没有执行过的调用，按 `fail_call` 落账，理由交给模型。

没有第三层。委派、派任务、追问、更新计划、MCP 调用、codemode、Memory 内部变更都走同一条路。cron 命令 Job 不再在 `cron add` 时签授权（§10.1）。

`policy.toml` 不再读：文件还在时 `komo config check` 与 Gateway 启动日志**报一句**"policy.toml 已不再使用，可以删掉"——与 start-only 键同一条规则（§3），报出来，不静默忽略。

### 7.2 唯一的硬限制：不写 komo 自己的状态

`write` / `edit`（含 `then_run` 的改动那一步）在 `prepare` 里拒绝目标落在 komo 自己的状态里：数据目录下的 `sessions/`、`state.db*`、`runtime/`、`.env`。拒绝是 `ToolError::Denied`，结果状态 `denied`，理由点名那条路径并指向 §8.10（会话只有 `komo session delete/purge` 一条删法）。名单由 Gateway 算好经 `ToolContext` 传入，kernel 不读机器路径。

这条留下来的理由：Session 目录是内容权威、`state.db` 是调度权威（§8.2），模型一次手滑改了它们，坏的是 komo 自己的恢复能力——账本与 JSONL 对不上之后，§8.4 那张表只能把会话判成 `blocked`。别的写入坏了是用户的文件，这一类坏了是 komo 自己失忆。

**它只拦得住 `write` / `edit`。** `bash` 跑的是任意代码，`rm sessions/…`、`sqlite3 state.db` 都在它外面；读也不拦（线上见过一条 WeChat 会话找不到数据时去读 `runtime/gateway.json` 与 `.env`）。这里如实写：这条限制挡的是手滑，不是有意绕过。

### 7.3 执行边界：只来自执行环境

内置文件工具可以在 Rust 里执行路径与版本限制；任意 shell 代码一旦启动，就有它运行账号的全部操作系统权限，cwd 和参数检查不是进程沙箱。所以**真正的边界只能来自执行环境**，不能来自对命令文本的判断——2026-10-11 之前那张 `command_patterns` 网就是例子：`rm -r -f /`、`find -delete`、变量拼出来的命令全在网外。

今天唯一的执行环境边界是 codemode（`docs/codemode.md`）：脚本跑在 Monty 里（2026-10-15 起，此前是 macOS Seatbelt / Linux Landlock + seccomp 里的 CPython），语言没有网络、子进程，文件只读（宿主 `/` 只读挂载，每个 OsCall 先在 worker 里拦：规范化后落在 komo 自己的状态里的连读也拒，名单是 `write` / `edit` 那份去掉 `sessions/`（会话日志 `read` 本来就读得到，2026-10-20 放开），同一个 `Protected`；写类一律拒），环境变量为空；worker 子进程由宿主管墙钟、内存与取消，到限 SIGKILL；脚本里经 `tools.<name>()` 的内层调用只放行只读计划与 MCP 调用（§4），本地写入、进程与编排仍在脚本外。这层边界是语言层的，不是内核隔离——Monty 自身的漏洞不在它的承诺里。要给 bash / python 也立边界，路只有一条：把它们放进同类的执行环境，而不是加回规则。

### 7.4 旧数据里的审批痕迹

审批删掉了，旧会话与旧库里它留下的东西照样读得出来，只是不再生效：

- **JSONL**：`approval.*` 事件按未知事件解码——保留 seq，不表示任何东西（§8.3）。`run.waiting` 的 `WaitReason::Approval` 只为解码旧行保留，新代码不产生它。
- **停在审批上的旧 Run**（`runs.wait_kind = 'approval'`）：Gateway 启动时按"批准"续跑。那次调用是 planned 而没 started，按 §8.4"调用 planned，确定尚未执行 → 自动执行"那一行走，同一条恢复路径、同一个重新入队的状态转换（JSONL → db 顺序照旧），不再看审批行。`needs_a_person()` 对 `Approval` 返回 false，它不进介入清单。
- **表与列**：`approval_requests` / `policy_grants` 的模型与 DDL 删掉，老库里的两张表留着没人读，`ensure_schema` 不管多出来的表；`runs.grants` 永远写 `"[]"`，`deliveries.approval_id` 永远 NULL（退役列照写空值，§8.2）。
- **投递**：`deliveries.outbound` 里 `kind = approval_request | approval_settled` 的旧行当作已作废跳过，`control_outbox` 里旧的 `approval.*` 审计行照原样补写或丢弃——**不能让一条旧行卡死补发循环**。
- `ToolError::Denied` 保留：§7.2 与 codemode 还在用，落账时是一条 `failed` 结果（`ToolResultStatus` 只有 completed / failed / uncertain，从来没有 denied）。

### 7.5 Intervention：一切"需要人判断"的同一个入口

§8.4 / §8.6 曾经产出两类只有人能回答的问题：**结果不明**（副作用可能已经发生、没有可靠恢复方式）与**阻塞**（引用损坏、无法确认旧执行已结束、会话内容与账本对不上）。2026-10-12 起结果不明交给模型判断、不再停下来等人（§8.6），现在会产生的只有阻塞；`verify` 留着只为升级前已经停下的旧 Run。它们曾经各走各的出口——结果不明只有一条事件与一句投递，"上一次执行没收尾"在 `resume` 里被直接续跑；操作者因此会撞上最难解释的现象：**会话停着不动，而待处理清单是空的**。（2026-10-11 之前这里还有第三类 `approval`，随审批一起删掉。）

统一成一个概念：**Intervention = 一条未完成的 Run 停在某个只有人能回答的问题上**。

| 种类 | 它从哪里派生（权威） | 句柄 | 问题 | 可答的结论 |
|---|---|---|---|---|
| `verify` | 升级前（2026-10-12 之前）停在 `uncertain` 调用上、没写 `last_error` 的 Run；现在不再产生，Gateway 启动时把它们放回队列交给模型 | Run ID | 上次那个调用**到底发生没有** | `satisfied` / `not_performed` / `abandon` |
| `blocked` | 其余 `waiting + intervention` 的 Run（§8.4） | Run ID | 前提没了：引用损坏、旧执行者未确认、会话不可服务 | `resolve` / `abandon` |

三条约束把这个清单钉死：

1. **清单是派生视图，不是另一张表。** 权威仍是 `runs` 与 `tool_calls`；`GET /v1/interventions` 是对它们的查询，`kind` 由"有没有一条 uncertain 调用"当场判出。多一张 `interventions` 表等于多一处会与权威漂移的状态（§8.9）。**子代理（§4）的介入因此自动出现在同一张清单里**：子 Run 与它的调用本来就在这些表里，"谁派的"不是清单该关心的维度。
2. **挡着会话的每一条都必须在清单里，而且与"后面领不走"是同一次判定。** §8.4 的"未完成"是四个非终态；其中**停在人身上的**（`waiting + intervention`）必须逐条出现，且由同一个谓词回答"这条 Run 现在挡不挡队"——就是 `WaitReason::needs_a_person()` 与那段 `earlier` 子查询。两处判定分家就会出现"卡住但清单为空"。`waiting + retry`（等时钟）与 `waiting + dependency`（等另一条 Run）不进清单：**它们不是在等人**，但仍然挡着同 Session 后面的 Run——唯一的例外是父 Run 正等着它派出去了的那条子 Run，那条必须能跑，否则父子互等（§4）。旧的 `waiting + approval` 不在等人，按 §7.4 续跑。
3. **结论只走一条路：核对 → 按 §8.4 行事，没有"我说它发生了"。** `satisfied` 给那次调用补一条"核对后目标已满足"的结果（completed），原 Run 回到队列接着跑；`not_performed` 给那次调用补一条"确定没有执行"的结果（failed），原 Run 回到队列，模型拿到这次明确的失败——**重做是模型发起的新一次调用**，执行器不按原计划重放；`resolve` 不强行放行，它只要求**重新观察并重新决策**一次——会话、输出、旧执行者这几道前提重新看，都过了就按 JSONL 尾部接着判（2026-10-13 之前扫描见到 `waiting + intervention` 只会判"原样停着"，那是上一次决策的结果而不是这一次的观察，`resolve` 实际什么也没做）；`abandon` 是终态取消。没有"我确认副作用已发生"这种结论——操作者可能看错，账本一旦这么记就再也纠不回来。

出口有四个，语义相同：`komo intervention list | show | answer`、TUI 的待处理清单、聊天里的 `/pending` 与 `/answer`（§11.3）、`GET|POST /v1/interventions`。一次答一条，没有批量答复。投递沿用 §11.4 那一条：**没有界面在看这个会话时进 home chat**——那是任务在问，不是在报告（§10）。

## 8. 通用 Session 存储与自动恢复

### 8.1 三个核心对象

| 对象     | 含义                                                      |
| -------- | --------------------------------------------------------- |
| Session  | 连续对话、工作目录和上下文的载体；有生命周期状态（§8.10） |
| Run      | 一次用户输入或一次触发引起的持久任务；重启前后保持同一 ID |
| ToolCall | 一次有独立执行状态的具体调用                              |

每个调用有 Runtime 分配的内部 ID，同时保留 provider call_id 和模型轮次；重复的 provider ID 不能误命中其他轮次。Run 可以经历多次进程执行，但不会因为重启而创建新业务任务；ToolCall 是逻辑动作，tool_attempts 记录它实际执行过的各次尝试。

本节的“任务完成”是 Run 已有明确终态，不能根据最后一条助手消息猜测。长期 Memory、任务摘要和模型生成的计划均不能代替执行账本。

**三者的权威都在 state.db，Session 目录是内容。** Run 的状态与队列在数据库里，`sessions/{id}/` 里放的是消息、计划、输出与产物（§8.2）。这个分工的唯一后果是：**目录与数据库对不上时，以数据库为准，并且必须有人把对不上的那一半处置掉**——这就是 §8.9 的 reconcile，它取代"发现不一致就报损坏然后卡住"。Session 自身也只是一个可以被关掉、被逻辑删除、被回收的对象（§8.10），不是只能追加的。

### 8.2 存储分工

**每个 Session 的内容集中保存在 sessions/{session_id}/ 下。** JSONL 完整记录消息、调用参数、准备计划和状态；完整 tool output、stdout / stderr 分别保存在该目录的 tool-output 子目录中，并由 JSONL 引用。state.db 维护调度状态与索引，不重复保存执行内容。旧会话的 `text_ref` / payloads 仍兼容读取，但新记录不再为了行大小把消息、参数或计划外置。

| 保存位置                                                           | 内容                                                                         | 权威来源与恢复方式                                             |
| ------------------------------------------------------------------ | ---------------------------------------------------------------------------- | -------------------------------------------------------------- |
| sessions/{session_id}/events.jsonl                                 | 完整消息、调用参数与准备计划、开始 / 结束事件、工具输出引用、摘要与完成事件 | **内容**的权威来源（事件顺序、调用关系与消息正文）。**状态与生命周期不在这里**——那是 state.db |
| sessions/{session_id}/payloads/                                    | 旧格式外置的模型消息、参数或执行计划正文                                   | 只为兼容旧 `text_ref` 等引用保留读取；新 JSONL 不再产生这类外置正文 |
| sessions/{session_id}/tool-output/{run_id}/{call_id}/{attempt_id}/ | output.json，以及按需保存的 stdout.txt / stderr.txt                          | 每次执行的完整工具输出，包含错误详情；按尝试独立、完成后不可变 |
| state.db（Turso，MVCC）                                           | Session 生命周期、Run 元数据、任务队列、领取代次、工具状态与引用、投递记录、Cron、Memory | **生命周期与调度的唯一权威**；执行内容索引与派生状态可从 JSONL 补齐（§8.9） |
| sessions/{session_id}/artifacts/{run_id}/                          | 工具生成的二进制文件、脚本快照与报告                                         | 独立产物，按 Run 分目录；正文里以 `artifact://files/<run>/<名字>` 给出引用（§4.7）；不重复复制到 tool-output |

state.db 中的主要表：

| 表                          | 主要内容                                                                                                                           |
| --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| sessions                    | 标题、来源、工作目录、当前 Run、**生命周期状态与状态变更时刻**（§8.10）、JSONL 路径、applied_seq 和已应用字节位置                     |
| runs                        | 请求键与输入哈希、输入 / 最终结果事件引用、状态、来源与身份、预算、有效期、领取代次、重试时间、配置快照、Memory 处理游标 |
| session_log_index           | event_id、Session seq、Run、事件类型、文件偏移、记录长度和完整性摘要；不存正文                                                     |
| tool_calls                  | 逻辑调用 ID、参数 / 计划 / 结果事件引用、计划哈希、恢复方式、外部幂等键和状态                                                      |
| tool_attempts               | 调用 ID、尝试序号、执行实例、进程身份、状态、时间与事件引用                                                                        |
| checkpoints                 | 已覆盖的 seq、JSONL 字节位置、格式版本、上下文与记忆版本引用、执行游标                                                             |
| control_outbox              | state.db 控制事务产生、尚待补写到 JSONL 的审计事件                                                                                   |
| deliveries                  | 主动投递记录：目标、内容引用、pending / sent / deferred（§11.4）                                                                   |
| cron_jobs                   | 定时任务定义与版本                                                                                                                 |
| cron_firings                | 唯一触发记录、不可变触发快照及 Session / Run 引用                                                                                  |
| memory_items                | 自动记忆内容、作用域、确认状态、生命周期、revision 与时间信息                                                                      |
| memory_evidence             | 来源事件或外部记录引用、提取与确认依据                                                                                             |
| memory_terms                | 索引时分词的关键词列（memory_id、revision、terms），可重建（§9.4）                                                                 |
| memory_vectors              | f32 BLOB + 维度 + 代次，可重建                                                                                                     |
| memory_index_generations    | 向量空间指纹、构建状态、进度与生效代次                                                                                             |

control_outbox 只保存控制事件，例如会话生命周期的 `session.closing` / `deleted` / `purged`，不复制消息或工具结果。它和队列表都在同一个 state.db 中，不增加外部消息服务。

**2026-10-11 退役的表与列**（§7.4）：`approval_requests` 与 `policy_grants` 不再有模型与 DDL，老库里的两张表留着没人读；`runs.grants` 留在模型里、永远写 `"[]"`；`deliveries.approval_id` 留着、永远 NULL。

**state.db 是 Turso，MVCC 模式，通过 toasty 访问。** 文件同步和数据库提交各自有持久化边界，不能称为跨文件原子事务；但两边都是真的落盘。**Turso 0.7.2 已核实（2026-09-16）：MVCC 与 WAL 两条提交路径都在提交内 fsync——MVCC fsync 逻辑日志 `state.db-log`，WAL fsync `state.db-wal`——同步档位是每连接状态，默认 `SyncMode::Full`（比 SQLite 在 WAL 下默认 NORMAL 更严），`PRAGMA synchronous` 可设可读回（`turso_core` 的 `mvcc/database/mod.rs:3107` / `storage/pager.rs:4297` / `translate/pragma.rs:647,1548`；strace 与 `synchronous=OFF` 对照实验见 `.scratch/komo-v08-rewrite/spikes/store.md`）。因此原先预备的「outbox 补写到 JSONL 后才确认」这道保护不启用**：控制决定一经数据库提交即已落盘，客户端可以立刻得到确认，outbox 只承担审计补写与顺序，不承担耐久性。内容权威仍然是 Session 目录，每一步先 `sync_all` 再往下走，state.db 是调度的权威。

两条随之而来的运维事实：① `state.db-log` / `state.db-wal` 与 `state.db` 同等重要，备份和清理不能只拷主文件；② Turso 默认 `data_sync_retry = false`，此时 fsync **出错是 `panic!` 而不是返回 `Err`**（`storage/pager.rs:4330`）。Gateway 在 `Db::connect` 里显式 `PRAGMA data_sync_retry = 1`，把「磁盘写失败」变成一个可以进 `waiting + intervention` 的错误，而不是一个把整个进程带走的 panic。

引擎事实与由此而来的规则：

| 事实 | 规则 |
|---|---|
| MVCC `concurrent_writes`：**只有 `BEGIN CONCURRENT` 事务**才并发提交；冲突的提交**失败而不是等待**。toasty 只在显式 `db.transaction()` 上发 `BEGIN CONCURRENT`，autocommit 单语句走普通写事务，且驱动不设 `busy_timeout`——实测 4 个写入器并发写 200 行不同主键，autocommit 只成 6 次，包进事务 200 次全成 | **每个写操作、包括只有一条语句的，都放进 `db.transaction()`，再整个包进 `with_write_retry`**；回滚后干净重跑，绝不双重应用。重试条件是 `toasty_core::Error::is_serialization_failure()`（驱动把 `Busy` / `BusySnapshot` / 消息含 `conflict` 的错误都归到这里），不是字符串匹配。闭包只依赖入参和事务内读到的状态，里面不 `await` 模型、子进程、文件同步或用户；重试超限报 `Contended`，按 §8.4 进 `waiting + retry`（`cause = contended`） |
| MVCC 支持 `AUTOINCREMENT`（原子序列，`turso_core` 的 `test_autoincrement_works_in_mvcc`，2026-09-16 实测 CREATE / INSERT / `sqlite_sequence` 正常）——但 komo 不用它 | 每个主键仍是 `String` UUIDv7：ID 要在写库之前就存在（JSONL 先写、事件里带 ID、跨进程恢复按 ID 对账），数据库生成的自增值满足不了这个顺序。Session 内的 `seq` 是 JSONL 写入器分配的整数列，不由数据库生成 |
| MVCC 下自定义索引模块不可创建（FTS 不可用）——已实测：`CREATE INDEX … USING fts (…)` 报 `Custom index modules are not supported in MVCC mode`（`translate/index.rs:70`），`CREATE VIRTUAL TABLE … USING fts5` 报 `Virtual tables are not supported in MVCC mode`（`translate/schema.rs:1692`） | 关键词检索在索引时分词写入 `memory_terms`（§9.4） |
| toasty 的类型化 API 能表达带条件的 `UPDATE`（`Model::filter(...)` 支持非索引列、`AND`、`IS NULL`），但**拿不到受影响行数**——查询目标的 `.exec()` 恒为 `Ok(())`，命中 0 行与 1 行不可区分；`ALTER TABLE`、`instr`、`vector_distance_cos` 也不在类型化 API 里 | 这四件事全部走 toasty 自带的 raw SQL 口子 `toasty::sql::statement(..) -> Result<u64>` / `toasty::sql::query(..) -> Vec<Value>`（占位符 `?1`、`?2`；`Db` 与 `Transaction` 上都可用，因此 raw 语句照样在事务里）。**不需要第二个 `turso::Database` 句柄**；raw SQL 仍只允许出现在 store crate 的三个模块：schema、keyword index、run claim |
| 每个 db 文件由进程独占锁定 | Gateway 是唯一打开 state.db 的进程，CLI 与 TUI 永远走 HTTP（§3） |
| `turso` 的 `mimalloc` 特性是全局分配器 | 工作区只能有一个 turso 版本，且与 `toasty-driver-turso` 的 pin 同 major；其他 crate 不得声明 `#[global_allocator]`。工作区把 turso 写成 `default-features = false, features = ["mimalloc"]`，关掉 `fts`（§13.4） |
| toasty `push_schema` 只对新文件执行，且不幂等 | 见下面的 schema 演进 |
| 提交在 Turso 上确实 fsync（MVCC 写 `state.db-log`，WAL 写 `state.db-wal`），默认 `synchronous=FULL` | 控制决定（当时是审批，2026-10-11 删除）提交即持久，不需要 outbox-先-确认；`state.db-log` 与主库同等重要；连接建立时设 `data_sync_retry=1`，让 fsync 错误可报告而非 panic |

Schema 演进没有迁移脚本目录。每个 toasty 模型旁边放它的 `*_TABLE_DDL` 常量；`Db::connect` 对已存在的文件逐表 `CREATE TABLE IF NOT EXISTS`、逐列 `ALTER TABLE ADD COLUMN`，对新文件让 toasty 建表。**这一趟补列必须在连接池建起来之前、用一条普通（非 MVCC）连接做完**（实测 2026-09-20：MVCC 连接上的 `ALTER TABLE` 返回 `Ok`、日志照打，重开即无——旧库升级会静静地起不来）；建池之后 `ensure_schema` 只做核对，文件库缺列就**报错**，内存库才由它补。同一个坑还有一条：turso 的读游标拖着一条读事务，**同一条连接上"边读边改"会 panic**在 `SetCookie`，所以那趟迁移是"先读清、丢掉连接、只用一条只写连接改"。一个测试对每张表断言 toasty 为空库生成的 DDL 与常量**字节相等**——模型改了列却没改常量，测试挂。新列必须 `NOT NULL DEFAULT …` 或可空；退役的列继续写空值，不删。耐久表（`sessions`、`runs`、`cron_jobs`、`cron_firings`、`memory_items`、`memory_evidence`、`deliveries`）只允许加法变更——`sessions` 与 `runs` 也在这里，因为生命周期与队列从内容里长不出来：目录没了还能从数据库说清"它曾经是什么"，反过来不成立（§8.10）；可重建表（`session_log_index`、`tool_calls`、`tool_attempts`、`checkpoints`、`memory_terms`、`memory_vectors`、`memory_index_generations`）按行或按代次从 JSONL / 原文重建，从不删文件——一个文件里同时住着耐久表，"删掉重来"不存在。

### 8.3 JSONL 格式、写入与读取

首版每个 Session 目录下一个 events.jsonl，payloads、tool-output 和 artifacts 均位于旁边的子目录。UTF-8 编码，每条事件占一个物理行；内容中的换行由 JSON 序列化转义，禁止多行美化输出。Komo 要求每条完整记录以换行结束，便于识别写到一半的末尾。[JSON Lines 格式](https://jsonlines.org/)

每行包括 v、seq、event_id、session_id、run_id、at、type 和 data。涉及工具时带 Runtime call_id 和 attempt_id，并保留 provider call_id 与轮次。seq 在 Session 内按追加顺序严格递增；event_id 标识一次逻辑事件，重复提交同一 ID 必须幂等，内容不同则报错。控制审计补写保留原始发生时间和操作引用，不凭日志行相邻推断控制关系。

以下是简化的调用片段；输出正文不在事件中，示例省略引用大小和哈希，实际准备计划还包含资源版本、代码快照和权限引用：

```jsonl
{"v":1,"seq":41,"event_id":"evt-41","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:00Z","type":"assistant.message","data":{"round":3,"tool_calls":[{"call_id":"call-7","provider_call_id":"pc-7","name":"bash","arguments":{"command":"echo 2"}}]}}
{"v":1,"seq":42,"event_id":"evt-42","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:01Z","type":"tool.started","data":{"call_id":"call-7","attempt_id":"attempt-1","plan_ref":"evt-41"}}
{"v":1,"seq":43,"event_id":"evt-43","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:02Z","type":"tool.result","data":{"call_id":"call-7","attempt_id":"attempt-1","status":"completed","output_ref":{"path":"tool-output/run-1/call-7/attempt-1/output.json"},"preview":"result = 2"}}
```

所有工具的完整输出都单独保存，不区分大小。output.json 保存结构化结果或错误正文，并绑定 Session / Run / ToolCall / attempt、计划哈希和完成状态；stdout / stderr 如有则写入独立文本文件，在 output.json 中引用。JSONL 的 tool.result 保存完整结构化 body、状态、耗时和输出引用；preview 是界面摘要，不是正文的存储上限。stdout / stderr 的完整流保存在文件中，模型视图超限时给出可直接读取的入口。

模型消息、调用参数和准备计划无论大小都完整内联在 JSONL；物理行可以很长，读取器不能再把 1 KiB / 4 KiB 当记录上限。旧会话的 `text_ref`、`arguments_ref`、外置计划与 payloads 仍按原有大小、哈希和字段定位校验后读取，但新写入不再产生它们。output.json 内部引用 stdout / stderr 时仍使用同一个 Session 目录作为基准，不依赖进程工作目录或 output.json 所在子目录；解析引用时禁止越出该 Session 的内容目录。

**工具结果可以带图片（2026-10-22）**：`read` 读到 PNG / JPEG / GIF / WebP（按文件头认，不看扩展名）、MCP 返回 `image` 内容、codemode 里子调用读到的图片，都随这次结果交给模型，而不是一句「未展开」。图片按文件存：执行器发布结果时把每张写成这次尝试目录下的 `image-<n>.<ext>`（与 `output.json` 同样先 `.partial`、同步、改名、同步目录），`output.json` 与 `tool.result` 里只记 `images: [{mime, content: {path, size, hash}}]`，**base64 不进任何一份账**。单张上限 3.5 MiB（`IMAGE_LIMIT_BYTES`，Anthropic 单张 5 MB 按 base64 计）：`read` 超限直接失败、提示先 `sips -Z 1600` 缩小（不引入 `image`，§13.4，没法自己缩）；别的来源有一张超限或写不下来，这次调用不算失败，只在正文末尾说明少附了哪张。交给模型时三种接口各自表达：Responses 的 `function_call_output.output` 写成 `[input_text, input_image…]`，Anthropic 的 `tool_result.content` 写成文字块加 base64 图片块，Chat Completions 的工具消息只放正文、紧跟一条带图片的用户消息（照 pi）。**没有图片时请求与原来逐字节相同**，提示缓存不受影响；衰减修订（`revised`）只换正文、图片保留。回放与恢复按引用读回图片、校验哈希，读不回来的那张少附、正文照旧——它只是模型视图，不是执行事实。压缩估价每张按 1200 token 计（`compaction::cut::IMAGE_TOKENS`）。已知缺口：codemode 脚本被打断、按 journal 回放已完成的子调用时，那些子调用的图片不再附上（journal 不存图片字节），只有文字结果。

stdout / stderr 在运行时流式写入 .partial 文件，避免在 Gateway 内存里积累完整输出。进程结束并收齐输出后，同步并完成文件，再原子写入 output.json；最后才能发布 JSONL 结果引用。因中断只留下的 .partial 文件可以用于诊断，不能当作完成结果：重启后那次调用的中断结果接管它——末尾一段进正文，完整的那份随结果发布成那次尝试的 stdout / stderr（§8.6）。

读取历史或恢复调用时按引用加载完整工具事实；**只有给模型的正文受限**。模型视图由 `komo-kernel::projection` 一处纯函数生成：full 视图默认最多 `50 * 1024` 字节或 2000 行（二者先到），`bash`（旧记录里叫 `shell`，两个名字都认）像 pi 一样保留尾部，其他工具保留头部；未超限时正文逐字不变，超限时只在 UTF-8 边界切分并写明省略字节数。首行本身超过字节预算时，头部策略不返回半行；末行本身超过预算时，尾部策略返回 UTF-8 安全的尾片段。`[execution] model_result_bytes` 可收紧或调整字节预算，2000 行上限固定。

超限视图总给出可直接 `read` 的完整引用：`artifact://<run>/<call>/<attempt>/result`，流另有 `/stdout` 与 `/stderr`（§4.7）。它们映射到当前 Session 已落盘、只读的 `tool-output/` 原件；`artifacts/` 同样是只读根。即时执行与恢复都从同一份落盘事实按 RunSnapshot 冻结的投影设置渲染，因此逐字节相同。运行时预览至少保留投影所需的 50 KiB 端点，不能在统一投影前先缩成旧的约 7 KiB；shell 默认完整流式落盘，只有工具显式的安全/沙箱存储上限才会截断原件。codemode 的 1 MiB 聚合保护也属于执行保护，不是模型投影预算。摘要、预览和后续 decay 都只改变模型视图，不能代替或改写 JSONL 与 tool-output 中的完整事实。

**大结果的视图按请求选（衰减，2026-10-10 改为缓存优先）**：同一份事实投影出两个视图——full 与更短的 decayed（抬头 + 首尾各留一段**整行** + **总是**给出那条可 `read` 的完整输出引用；正文不带"给过几次"之类的数字，换不换由日志里的决定说了算）。**前缀默认只追加。**换短视图是对前缀的改写：被改处之后的提示缓存整段失效、按写价重写。2026-10-09 的首版照 SoL-Pi ObservationPack 按次数各自到期各自换，实测（2026-10-10，会话 01a12475-3ec2-71e3-a787-a527ab549d9a，magpie Responses 后端、claude-opus-5-5）10 轮的 run 里 `cache_read` 四次退回 13317（只剩系统提示 + 工具表），全价输入 85k（约 53k 是失效重写），折算成本与 Claude Code 持平，尽管上下文只有它的 1/3。算账：替换省下的是"完整 − 短视图"按读价算的 token，代价是被改处之后整段按写价重写，写读比 12.5，回本要几十次后续请求，短任务永远亏。SoL-Pi 的 ObservationPack 本身不算缓存账（它的测评是长任务），它的缓存账只在 Online Context Compact 里——所以 komo 把衰减并进那本账。**资格**（沿用 SoL-Pi，`projection::decay_eligible`）：成功、full 超过 `[execution] decay_threshold_bytes`、短视图至少省 1 KiB、且已完整给过 `decay_full_sends` 次（这条 Run 在它之后记了几条 `message.assistant`）；失败与结果不明的结果**从不衰减**——那是模型要对着改的正文。**资格不等于换。****换不换只在两种时机判断，与在线压缩是同一个决策点（`compaction::decide`）、同一本缓存账，一次换一批（所有有资格的），一次改写只失效一次**：①**缓存已冷**——距这个会话上一次 provider 请求超过提示缓存存活期 5 分钟（等审批、resume、cron 接续）：改写不欠债，有资格的全换，reason `cache_cold`（Claude Code microcompact 的做法）；②**贴着窗口**——不看账，先换短视图（不用模型请求，比压缩便宜），没有可换的才压缩（第三种时机"计划边界"随 `update_plan` 在 2026-10-16 去掉）。其余时候一个字节都不改；短任务不贴窗口，就从不改写。**改写代价按后端算**：Anthropic Messages 有显式断点（driver 在被改消息前补一个），代价 = 从最早被改那条消息到末尾；Responses / Chat Completions / 代理是自动前缀缓存，按全部消息算（实测 magpie Responses 退回到系统提示）。估高只会少换，符合缓存优先。**决定记成追加事件**：`context.compacted` 的 `outcome: "decayed"`（`calls`、`decision`、`debt`）；fold 把 `calls` 累进 `RunView.decayed`，**回放按这个集合选视图**（`projection::select`），不再按次数数——刚跑完与回放仍逐字节相同，因为决定是记下来的、不是重算的。缓存债与冷却同压缩共用（`OnlineState::record_decay`）。**活的 loop**：planner 返回 `Reshape::Decay` 时先落账、再把短视图经 `RoundInput::ToolResults.revised` 交给驱动按 `provider_call_id` 就地换；段开头的冷缓存衰减由 Gateway 在装配前落账，装配时回放自然给短视图。

**在线压缩是一条追加的事件，不改写旧行**：`context.compacted`（`run_id` 是被压的那条 Run）记一次判断的结论——`outcome: "compacted"` 带 `first_kept`（原样保留的第一条，这条 Run 某一轮 `message.assistant` 的 seq）、摘要（`summary` 内联，超过内联上限外置成 `summary_ref`，顺序照 §8.5：正文先落 `payloads/` 再追加这一行）、决策的全部算式中间量、欠下的缓存债与摘要请求自己的用量；`outcome: "skipped"` 带理由与（可选的）当时那份决策——`compact: true` 而落到 skipped 就是摘要请求没成（现在只写这一种；旧日志里还有计划边界上"决定不压"的，`compact: false`，fold 不当回事）。**决定是记下来的，不是回放时重算的**：配置改了、估价口径改了，读这一行的结论不变。它不碰 state.db 的任何表。fold 把最新一次 `compacted` 记在那条 Run 上（`skipped` 不改视图）；回放读到它，这条 Run 的协议窗口就变成**开头那句任务（`run.accepted`）→ 摘要（一条用户消息，前面一句固定的话说明这是压缩摘要、完整工具输出仍可按 `artifact://` 读回）→ 从 `first_kept` 起的原样轮次**，历史 Run 与子代理线的规则不变，重启前后从同一份日志折出、逐字节相同。切点只落在一轮的开头，调用永远不和它的结果分开；`first_kept` 不是这条 Run 某一轮的开头（或不在压缩之前）就不认，回放完整历史并记一条告警。尾巴里留下的结果，其视图仍按 `RunView.decayed` 选，与压缩互不影响。同一个 type 还带 `outcome: "decayed"`（衰减的决定，见上一段）。**摘要只是模型视图，不是恢复依据**：§8.4 的尾部判断与 §8.6 的核对从不读它，"做没做过"仍只看 `tool.*` 事件与 `output.json`。不认识这个 type 的旧二进制把它当未知事件保留、忽略，回放完整历史——只是贵，不是错。

实现约束：

- Gateway 对每个 Session 使用一个串行写入器，模型、工具、控制审计与 Memory 来源标记均经过它，不能各自追加导致行内容交错。
- 完整记录写入后清空用户态缓冲，再同步文件；同步失败就停止该步骤，不能执行后续副作用或向客户端确认持久完成。新建文件和目录还要处理目录持久化。Rust 的 File::sync_all 提供文件同步接口，关闭文件不等于已验证同步成功。[Rust File](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all)
- 完整模型回复和该轮调用计划保持为一个逻辑 assistant 事件：正文小则内联，大则引用已经完整持久化的 payload 文件，避免只恢复出半轮调用。工具输出先独立持久化，再一次提交对应结果事件，不按流式 token 写 JSONL。
- 正常修改只追加新事件；纠正、取消和摘要覆盖关系通过新记录表达，不重写旧行。
- **插话也是一条追加的事件**（§6）：`run.absorbed { into }` 的 `run_id` 是被并走的那条。fold 把它的状态记成 `absorbed`，并把那句用户消息**复制**到这一刻、归到宿主名下（`absorbed_from` 指回原 Run）；原消息留在消息面上（只追加，TUI 按前缀长度记交出去多少条），回放按 `absorbed` 跳过它。复制的那条跟在工具结果后面，两边都是用户侧，不算角色交替违例。
- 读取依据事件 ID / seq；字节偏移只是加速索引，校验不符时重新扫描并重建。每轮通过检查点与尾部增量读，避免反复解析完整会话。
- 模型请求保存所用事件范围、模型 / effort、工具与记忆版本引用；摘要正文作为 JSONL 事件保存，检查点不再复制整段历史。
- **回放给模型的是"这一段对话"，不是"这一条 Run"**：最新一个 `conversation.boundary` 之后的消息按 Run 分组、Run 之间先来后到；**正在跑的那条 Run 带完整协议**（`provider_blocks`、工具调用与结果），**历史 Run 只发布两件事——用户说了什么（`run.accepted`）、它最后答了什么**。三条理由：① 历史那半轮在取消 / 失败时合法地停在"有调用、没结果"上（§8.4），照搬进新请求就是 provider 400；② 几十轮工具往返会随对话长度线性涨上下文，而对"接着聊"没有增量；③ 子代理跑过的那几轮不属于父的对话，父的窗口里没有它的过程，子代理也拿不到父的历史（§4）。**子代理的"这一段对话"就是它那条线**（同步委派的 `resumes` 链；2026-10-16 委派删掉之后只有升级时还没跑完的旧子 Run 走它，§4）：顺着 `resumes` 往回找到的那几条子 Run 加上正在跑的这条，规则与主对话一字不差——正在跑的带完整协议，线上更早的那几条只发布任务正文与最后的回答（`failed` 的那条没有最后回答，就只剩任务正文，外加一句它怎么结束的）。续跑因此不会把上一次停在半轮的调用带进新请求，而过程细节要的时候按引用 `read` 回来（`tool-output/` 是只读根）。线是从 JSONL 里 fold 出来的（`run.accepted.delegate.resumes`），不查 state.db：它决定模型看见什么，属于内容权威（§8.2）。还没被领走的 Run 整个不进（§8.4 的不越过）。**上一轮说的话因此必须还在**：少了它，"去查一下"指的是哪件事就只能靠猜。正文超限外置时按引用读回来——少读一次，模型看到的就是一条空的 user 消息，而它该看到的是原话。
- Memory 的证据引用使用 Session ID + event_id / seq，稳定定位原始消息或工具结果。
- **读事件不得创建或修改内容。** `Ledger::read` 与 `session_log::read_events` 是只读路径：文件不在就返回空（新会话本来就没有日志，`GET /v1/sessions/{id}/events` 依赖这一点），**绝不 `create`**。写路径（受理、`conversation.boundary`、挂起、终态）才建目录与文件。少了这条界线，"内容被删掉"会被一次读**静默地补成一个空日志**——于是 §8.9 的观察永远看不到它本该报出来的那件事（线上冒烟抓过一次：对账读会话就把目录建回来了）。
- JSONL、state.db 和引用产物均为 Gateway 的受保护状态；`write` / `edit` 拒绝修改它们（§7.2）；bash 拦不住，§8.9 的 reconcile 把改坏的那一半报出来。回放只能接受 Gateway 写入的合法记录，不能把模型生成的 JSON 当作执行事实。

尾部恢复时，先校验数据库已经引用的记录和检查点，再处理尚未应用的后续记录。只有末尾不完整、且不被已提交引用覆盖的行，才能先隔离保留原始字节再截断；完整有效行不能因为索引落后被删除。文件中间损坏、已提交范围缺失或哈希不匹配时停止受影响会话的自动执行，不能跳过坏行或退回旧检查点继续。

未知事件保留原始内容与序号；影响执行或调用配对的未知版本必须暂停恢复并要求兼容版本，不能按无关日志忽略。2026-10-11 退役的 `approval.requested` / `approval.decided` 等审批事件不在此列：它们是已知的**退役**类型，按未知事件解码、保留 seq、不表示任何东西，也不暂停恢复（§7.4）——那些会话照常加载与续聊。

### 8.4 重启后的默认行为

**Gateway 自动接续能够确定恢复位置的任务。用户只处理时效或阻塞等确实需要判断的情况；结果不明交给模型核对（§8.6）。**

**状态回答"能不能跑"，`WaitReason` 回答"为什么不能跑"。** 这两件事分开，是因为它们混在一起时的症状正是"排队二十分钟不知道在等什么"：一个 `waiting_approval`、一个 `needs_attention`，都答不出在等谁、等到什么时候。所以状态只有九个，等什么有三类（外加一个只为解码旧行保留的 `approval`）：

| 状态 | 含义 | 可领取 |
|---|---|---|
| `accepted` | 已受理：请求键预留了 ID，输入正文可能还没写完（§8.5） | 否 |
| `queued` | **现在就能跑，只缺 worker** | 是 |
| `running` | 正在执行。`claimed_by IS NULL` 的是刚被回收、还没判完的孤儿（§8.9） | 否 |
| `waiting` | 当前不能跑，`WaitReason` 说得出在等什么 | 只有 `retry` 到点之后 |
| `completed` / `failed` / `cancelled` / `abandoned` / `absorbed` | 五个终态 | 否 |

| `WaitReason` | 在等什么 | 谁放它出来 | 进 §7.5 清单 |
|---|---|---|---|
| `approval` | **2026-10-11 起不再产生**，只为解码旧行（§7.4） | Gateway 启动时按"批准"续跑 | 否 |
| `retry` | 一次有界退避到点（`attempts` / `not_before` / `cause`） | 时钟：`wake_at <= now` | 否——不是在等人 |
| `intervention` | 一条 Intervention 的答复（§7.5：结果不明、前提没了） | 操作者答复 | 是 |
| `dependency` | **另一条 Run** 进终态：同 Session 里更早的那条输入（本节的次序规则）。2026-10-16 之前还有"它派出去的那条子 Run（同步委派）"，委派删掉之后只为收尾升级时还在等的旧行（§4） | 那条 Run 进终态，reconcile 把它放回 `queued` | 否——不是在等人 |

**退避只用于可以安全重试的失败**：限流、连不上、5xx、本地写争用，`RetryCause` 把它们分得开（`RateLimited` 要尊重服务端给的 `Retry-After`，`Contended` 退几毫秒就够——混成一个数字就只能取最保守值，每次 429 都白等）。工具那一侧"结果不明"**不能**用它：那是副作用有没有发生都不知道：执行当中报的与重启续跑时核对不出的，都作为 uncertain 结果交给模型判断（§8.6），都不自动重试。两者混起来就是"重试成功"掩盖掉那个窗口，正是 §8.6 要禁止的事。

**`Queued` 与 `Waiting` 是这个模型的验收口径**：`Queued` 的 Run 一定有活干，`Waiting` 的 Run 一定说得出在等什么。查"它怎么还不跑"时，这两句话各回答一半。

重启后逐行：

| 重启前停在哪里 | Gateway 启动后怎么处理 |
| --- | --- |
| 输入已在 JSONL 持久保存，Run 尚未开始（`accepted`） | 补齐 state.db 索引后自动入队 |
| 输入请求只预留了 ID，正文尚未完整写入 | 保持 `accepted`；等待同请求键重传，不能执行缺失输入 |
| 正在请求 LLM，完整回复尚未保存 | 丢弃未完成输出，以已保存上下文重新请求；可能再次产生模型费用 |
| assistant 回复和调用计划已保存 | 沿用原计划，从未完成的 ToolCall 继续 |
| JSONL 已有结果引用且完整输出校验通过，state.db 可能落后 | 先补齐结果索引和状态，复用原输出，不重放动作 |
| 调用 planned，确定尚未执行 | 校验原计划（`prepare` 那一层）后自动执行 |
| 调用有结果、**没有 started**（一次没执行过的调用：工具名不认识、参数准备不出来、`prepare` 拒绝） | 按已有结果收口，不重跑；回放时把那条 `tool.started` 缺席的尝试行补上（`fail_call` 写下的就是它） |
| 调用 started，没有结果 | 按 §8.6 的重放策略：计划与工具现在的声明都是 safe 才带原计划重跑一次（新的尝试）；否则记 uncertain，连同中断前已落盘的部分输出作为这次调用的结果**交给模型判断**，Run 接着往下走 |
| `waiting + approval`（2026-10-11 之前的旧 Run） | 按"批准"续跑：那次调用 planned 而没 started，走"调用 planned，确定尚未执行"那一行，同一个重新入队的状态转换，不看审批行（§7.4） |
| `waiting + retry` | 沿用已保存的次数与到点时刻，到点再尝试 |
| `waiting + intervention` | **原样保留**：它是清单里的一条，等操作者答复——恢复扫描不替人答，也不偷偷往下跑。两个例外都是"重新看前提"：操作者答了 `resolve`（§7.5），或它停下是因为旧执行者没确认结束（最后一行） |
| `waiting + dependency` | 原样保留；谁放它出来由 reconcile 判（它等的那条 Run 进终态 → 回 `queued`） |
| 已保存最终结果，但客户端没有收到 | 补发或补读原结果，不重新执行任务 |
| 五个终态 | 保持终态，不因重启自动开启新一轮 |
| `run.absorbed` 已同步，state.db 还没提交（被并走的那条还停在 `queued` / `waiting + dependency`） | 恢复扫描不单独替它补（它不在跑）；补宿主时顺带派生 `run.absorbed`，提交成 `absorbed`——它不会在宿主之后再被领走、把那句话答第二遍 |
| Session 在 `closing` / `deleted`，Run 还没跑完 | 照跑或停在等待，但不新开；`--now` 的逻辑删除已把它们各写一条明确的取消（§8.10） |
| Session 内容缺失或被回收（`state` 不是 `purged` 却读不出目录 / JSONL / 引用的输出） | 不许领这条 Run，停成 `waiting + intervention`，理由里说清缺什么（§8.9） |
| 无法确认旧执行已结束 | 不许重复启动，停成 `waiting + intervention` 进清单，**领取者留在行上**；对账每一拍拿它重新探一次子进程，结束了就按 JSONL 尾部接着判，不必等人答（§8.7） |

内部状态示意：

```text
Run（状态 × 理由）:
                       ┌─────────────┐
                       │  accepted   │  已受理；正文可能还没落全
                       └──────┬──────┘
                              │ 正文落盘 + queued 提交
                       ┌──────▼──────┐
                       │   queued    │  现在就能跑，只缺 worker
                       └──────┬──────┘
                              │ claim（带租约）
                       ┌──────▼──────┐
                  ┌────│   running   │────┐
                  │    └──────┬──────┘    │
              停在外因         │ 正常收尾   │ 出错 / 取消 / 放弃
                  │           │           │
                  ▼           ▼           ▼
            ┌───────────┐  completed   failed / cancelled / abandoned
            │  waiting  │
            └─────┬─────┘
                  │ 条件满足：答复 / 到点 / 前一条 Run 进终态
                  ▼
                queued

插话（§6）: queued / waiting+dependency ──被正在跑的那条并走──▶ absorbed

WaitReason: retry{attempts, not_before, cause} · intervention · dependency{run}   （approval 只解码旧行）

ToolCall:
planned → started → completed / failed
                  └─ uncertain → 交给模型（重启后 safe 的才重跑，开新尝试）

Session（§8.10）:
active → closing → deleted → purged
```

**五个终态各自是什么**：`completed` 正常结束；`failed` 有明确错误；`cancelled` 用户明确取消（不自动复活）；`abandoned` 是**操作者在清单上放弃**——与取消分开记："不是用户不想跑了，而是这件事不会再有下文了"，事后统计要分得开；`absorbed` 是**插话被并进了当时正在跑的那条**（§6）——它自己不开跑，回答在宿主那里，等它的后续 Run 改指到宿主。

**同一 Session 后面的 Run 不越过前面的**：前一条没进终态时，后一条是 `waiting + dependency` 而不是 `queued`——它写得出在等谁，而不是一句"排队中"。**判"谁在前"用的是输入事件的 `seq`**（§8.3：seq 在 Session 内按追加顺序严格递增），不是 Run ID 的字典序：UUIDv7 同一毫秒内的低位是随机的，拿它当先后会把两条几乎同时到达的输入排反，而次序判错意味着后一条会越过前一条（provider 400 的来源）。这条次序在三个地方用同一份判据：受理那一刻（写 `queued` 还是 `waiting + dependency`）、`due` 的候选、`claim` 的守卫。唯一的例外是插话（§6）：它不越过前面那条，而是**并进**前面那条，自己不再开跑。**停在人身上的那一类（`intervention`）逐条出现在 §7.5 的清单里，而且"挡住队列"与"进清单"是同一个判定**——两处分家就会出现"卡住但清单为空"。

用户明确取消的 Run 不自动复活；普通错误达到重试或执行预算后进入 `failed`。Gateway 关闭或系统重启属于中断，**不等于用户取消**，也不再留一个叫 `interrupted` 的状态：领取权交还之后，那条 Run 由 reconcile 当场判成 `queued`（安全）或 `waiting + intervention`（副作用不明）。有时间限制的动作，先检查有效期；过期不能按旧指令直接产生新的外部影响。

### 8.5 JSONL 与 state.db 的写入顺序

消息和调用事件遵循“先持久化外置正文（如有），再持久化 JSONL，最后提交 state.db 状态与引用”。每个 Session 的协调入口串行完成追加与索引推进；applied_seq 只能推进连续、已校验的事件前缀。数据库事务中不等待模型、用户、子进程或文件同步。

```text
收到输入
  → state.db 用请求键预留 Run ID，状态 `accepted`，仅存输入哈希与来源
  → JSONL 追加 run.accepted（包含完整输入和绑定 Run ID），同步文件
  → state.db 事务写入事件引用、queued 与 applied_seq
  → 向客户端确认已接收
  → 调度器领取 Run

收到完整模型回复
  → 大消息或大参数先持久保存到 payloads（如有）
  → JSONL 写入完整 assistant 事件与全部调用计划的内联内容或引用，同步文件
  → state.db 事务建立该轮 planned 调用与事件索引

执行一个调用
  → 校验计划、代码与资源当前状态
  → 需要外置的准备计划先持久保存，再将计划引用与 tool.started 追加到 JSONL 并同步
  → state.db 事务提交 started、执行尝试与事件索引
  → 确认文件与数据库两步都完成后，才执行真实动作
  → 持久保存 stdout / stderr（如有）、结果正文和 output.json，完成文件与目录同步
  → JSONL 追加只含元信息与 output_ref 的 tool.result 并同步
  → state.db 事务更新结果引用、调用状态、事件索引和 applied_seq
  → 继续下一调用或下一轮模型

结束任务
  → JSONL 追加包含最终回复的 run.completed 并同步
  → state.db 事务更新终态、最终事件引用和 Memory 待处理标记
  → 通过 SSE 通知客户端
```

同一请求键重发时返回原 Run，内容哈希不同则拒绝。`accepted` 的完整输入若已在 JSONL 中，启动修复就能补为 `queued`；如果正文从未写完，则等待客户端重传，不能凭输入哈希补造用户指令。Cron 在创建 firing 与预留 Run 的事务中保留不可变触发快照，可以据此完成尚未写入的触发输入。

恢复先处理已同步但未索引的 JSONL 尾部，再判断任务是否需要执行。例如 tool.result 已经完整持久保存且引用输出校验通过、数据库仍显示 started，必须先补齐为已有结果，不能直接按 uncertain 重试。若结果引用存在但对应输出缺失或哈希不符，停止受影响任务，不能重跑来掩盖数据损坏。输出文件已写完但 tool.result 尚未提交时，只有完整校验文件内的调用身份、计划、完成状态与内容后才能补记结果；仅凭路径存在不足以宣告成功。

回放只补索引与派生执行状态，不调用工具、不发送外部请求，也不能覆盖 state.db 已记录的用户取消。**派生写入只有一处**（2026-10-13，`komo-store/src/repos/derive.rs`）：一条事件在 `tool_calls` / `tool_attempts` / `runs.rounds` 上留下什么，实时提交（上面每段的最后一步）与回放调的是同一个函数，所以"可重建表是 JSONL 的投影"由测试钉住——清掉派生行再回放，补出来的与当初写下的逐列相同，只差 JSONL 里本来没有的执行实例与时刻（`backfill_rebuilds_exactly_what_the_live_path_wrote`）。以前两边各写一份，回放漏了轮次计数。tool.started 本身不能证明已产生副作用；缺少 tool.result 的调用按 §8.6 的重放策略处理。

会话生命周期等控制操作的权威在 state.db，采用相反方向的审计补写：

```text
state.db 事务提交控制决定与 control_outbox（固定 event_id）
  → Session 写入器将审计事件追加到 JSONL 并同步
  → state.db 标记 outbox 已交付并更新日志索引
```

重启后重发同一 outbox 事件先按 event_id 去重，已写入就复用原事件位置。控制决定生效不依赖审计补写成功；客户端恢复时可直接查询数据库中的原决定。JSONL 中的审计副本无法反向生成状态，outbox 也不承载完整工具结果。旧库里残留的 `approval.*` 审计行照原样补写或丢弃，不能卡住补写器（§7.4）。

**补写由写入触发，周期只是兜底。** 上面那三步是顺序，不是节拍：`control_outbox` 一有新行就叫醒补写器，否则"随后补写"会变成"最多晚一分钟补写"——**这一分钟是看得见的**（当时的实例是 TUI 的审批弹窗等 `approval.requested` 补写进来才弹，审批 2026-10-11 删除，道理照旧）。周期（60s）留着兜底：漏叫的、别处写进去的，一拍之内照样补上。

完整模型块、工具调用 ID、原始参数、结果和 provider 回放所需字段通过 JSONL 及其引用文件完整保留。未完成的流式模型输出没有执行权限；只有完整计划持久保存且执行前置状态已提交后才能调用工具。

普通模型超时可按有界退避重试，次数、已用预算和下次时间都持久化。重启不重置预算；模型请求结果与用量都未知时，保留预留额度和未知标记，不能将这次消耗当成零。

**预算管一串连续失败（2026-10-22）。** 2s 起步指数退避、单次封顶 60s、第 10 次失败才 failed，连续退避约 5 分钟（2+4+8+16+32+60×4）；等退避时 Run 让出执行名额。模型成功回了一轮（`message.assistant` 推进轮次）就清零，账本与内存同时清。此前是 5 次、无封顶、整条 Run 累计不清零：实际只退 30s，长任务里早先的零星失败还会攒下来——任务 `01a15db2`（2026-10-21）跑了 15 分钟、19 轮后遇到中转站 503「No available accounts」，第一次退避已从第 2 次数起，28s 用完就失败（SR-007）。`Retry-After` 尚未读取，429 也按同一条指数退避。

### 8.6 工具恢复：先判断是否发生，再决定是否重试

Runtime 无法对任意 shell 和外部服务共同提交一个原子事务。副作用已发生、完整输出与结果事件尚未持久保存时，必须保留 uncertain，而不是用“重试成功”掩盖这段窗口。

**重启续跑时只问一件事：这次调用能不能重放**（2026-10-13，规则借自 pi-durable 的 `replay`）。一次 started 而没有结果的调用，盘上若有一份身份齐全、哈希对得上的 `output.json`，按 §8.4 补记结果、复用原输出，不重做；没有的话看两份重放策略：

| 计划里记下的（`plan.recovery.replay()`） | 工具现在声明的（`Tool::replay`） | 怎么办 |
| --- | --- | --- |
| safe | safe | 带原计划重跑一次：同一个 ToolCall，新的一次尝试 |
| safe | unsafe | 不重跑——现在的声明能否决记下的 safe（升级改了声明） |
| unsafe | 任意 | 不重跑——现在的 safe 不能把记下的 unsafe 升级 |

不重跑的那次给上一世的 `tool.started` 配一条 uncertain（**中断结果**），作为这次调用的结果交给模型，Run 接着往下走。正文写明"上一次执行被 Gateway 重启打断、重启后不会自动重跑"，附一段判断方法（先核对目标的实际状态；已生效就接着往下，没生效或只做了一半就继续或重做，只读命令直接重跑；发消息、扣款这类重复做会出问题又核对不了的先问用户），再附**中断前已落盘的部分输出**：stdout / stderr 在运行时流进那次尝试的 `.partial`（§8.3），这里各取末尾至多 2 KiB 放进正文；完整的那份由这条结果接管，随它发布成那次尝试的 `stdout.txt` / `stderr.txt`，模型可以按引用 `read`。一个字节都没落下也说一句。正文在收尾时固化进 `output.json`，回放逐字相同。部分输出只是"做到哪儿了"的证据，不是完成结果。

理由：停下来问人并不比模型更有把握——人多半也得去看一眼目标才答得上来，而模型手里就有 read / bash。**执行器本身不重跑 unsafe 的调用**，重做是模型发起的新一次调用，账本里看得见。**执行当中**工具自己报"结果不明"（如 unsafe 的调用超过时限、远端没回话）也一样：结果正文附同一段判断方法交给模型，Run 不停。账上找不到承载那次调用的尝试（账本自相矛盾）时补开一次 `tool.started` 记下结论，同样交给模型。仍交给人的只剩模型拿不到上下文、或放出去可能重复执行的：引用损坏、会话读不出来、无法确认旧执行已结束（§8.4）。升级前已经停在 `verify` 上的 Run（停在介入上、带 `uncertain` 调用、`last_error` 为空），Gateway 启动对账之后放回队列交给模型。

**工具名后来没了的在途调用**（2026-10-15，替掉 `shell` → `bash` 那条别名；2026-10-16 删掉的 `delegate` / `dispatch` / `follow` 例外：它们是编排调用，不问工具也不看重放策略，带着落账计划回来的照下面两段收尾，不当成中断）。恢复时一条调用已经有落账的计划、上一世接着往下做，而它的工具名不在这次的工具表或能力面里（`shell` 改名、`python` 删除，以后别的工具也一样）：执行器不看重放策略——没有工具可问——直接按中断处理，记 `uncertain`（带上它留在盘上的部分输出）交给模型，既不当"没开始"、也不停 Run。判据是"带着落账计划，且上一世至少 `started` 过"：只落了计划、没 `started` 的确定没执行过（§8.4 第 6 行），与模型这一轮新发的调用一样，叫到不存在的工具名照旧被拒，并列出这次能用的工具（`executor::tests::fused::a_planned_only_call_to_a_retired_tool_is_refused_not_uncertain`）。

`RecoveryMode` 由 `prepare` 填写，模型不能随意填"可重试"。它进计划哈希，四个变体都保留，旧计划照原样解码、哈希不变：

| `RecoveryMode` | 重放策略 | 谁写它 |
| --- | --- | --- |
| `SafeReread` | safe | `read`、`rg`、`codemode`（safe 只表示可以重新进入子调用 journal：已交付的子结果原样接回，unsafe 子调用不重做，`docs/codemode.md` §7）、服务器标了只读的 MCP 工具 |
| `IdempotencyKey` | safe | 外部接口确实保证去重、所有尝试复用同一逻辑操作的键；目前没有工具产出它 |
| `VerifyTarget` | unsafe | 2026-10-13 之前的 write / edit / 带核对函数的 python 计划（python 工具 2026-10-15 已删除）；新计划不再产生 |
| `NoSafeRecovery` | unsafe | `write`、`edit`、`bash`、带 `then_run` 的组合调用、其余 MCP 工具 |

`Tool::replay` 默认 unsafe，只有重跑一遍确实无害的工具覆盖它，就是上表 safe 的那几个。

**2026-10-13 之前这里是一条核对梯子**：write / edit 按修改前后的内容哈希判"已满足 / 确定未执行 / 冲突"，python 调模块用 `__komo_verify__` 声明的核对函数，判出"确定未执行"由执行器重做、"已满足"直接按完成收尾。删掉的理由：它只覆盖 write / edit 与极少数带核对函数的模块，却撑起了 `Tool::verify`、`Verification` 与一套核对函数协议；而核对判不出结论时早已交给模型（2026-10-12）。现在一律交给模型。模块里写着的 `__komo_verify__` 不再被读取；内置 `memos` 仍导出 `verify`，它是模型可调的普通函数，被打断的 Memos 写入可以用它去核对。

**带 `then_run` 的组合调用是 unsafe**：计划写 `NoSafeRecovery`，与单独的改动一样，`start` 过而没有结果就补一条中断结果交给模型，执行器不重跑任何一步；只 `planned` 过（确定一次都没执行）的照常跑一次。

**`task` 按请求键幂等重放**：派出（建会话 + 受理第一条输入）与接着做（受理一条输入）都只是"往某个 Session 提交一条输入"，同一个请求键重放只拿回原来那条，不会派出第二个任务；交回同理（`task-result:{run}`）。

**旧的 `delegate` 调用（2026-10-16 删掉之前落账的，§4）照下面这段收尾。** 委派不看重放策略，它的结果在我们自己的账本里。一条 `delegate` 调用的结果不在别处，就在它派出去的那条子 Run 的终态里（`Ledger::run_end`）：子 Run 正常结束 → 那次调用按已完成收尾，结果交给父 Run（有契约时按**父侧计划里那份**契约复验，与子代理自己用的是同一份校验器）；子 Run 失败 / 取消 / 放弃 → 按失败收尾，理由里写明它怎么结束的；子 Run 还没有终态 → 继续等。**这里不会出现"结果不明"**：副作用发生过没有，是我们账本里的一条事实，不是对外部世界的猜测。子 Run 自己是普通 Run，它的恢复照 §8.4 逐行走。续跑（§4）不改这一段：父侧看的永远是**这次调用派出去的那一条**，线上更早的几条早已各自收过尾，不再看它们。`dispatch` / `follow` 按请求键幂等重放。

典型情况：

- 已保存某次测试的完整结果：恢复后直接使用，不重复跑。
- 测试进程中断且结果丢失：bash 不重跑；中断结果带着已经打出来的那段测试输出交给模型，由它决定重跑。
- HA 的“设为关闭”：模型可以读取当前状态核对目标；“切换一次”不能据此重复调用，且恢复前要检查时效。
- 创建 Memos 记录或 PR：优先保存远端 ID，或使用接口实际支持的幂等键 / 可验证关联 ID。只有内容相似、时间接近，不能证明就是原调用创建的记录；无法确认时请用户处理。

一次重跑沿用 ToolCall ID，新增 tool_attempts。已完成或已失败且结果已保存的调用不重做。重复尝试与新模型调用都计入原任务预算。

### 8.7 Gateway 启动与进程回收

```text
取得实例锁，校验数据库与协议版本
  → 创建本次启动身份（旧的 running 从此都算"别人的"）
  → 校验受影响 Session 的 JSONL，修复未索引尾部和派生状态
  → 补写控制审计 outbox（旧的 `approval.*` 行不卡住，§7.4），核对尚未接收完整的输入
  → 2026-10-11 之前停在审批上的旧 Run 按"批准"放回队列（§7.4）
  → **回收没有主人的 running**：交还领取权（不改状态），再按 §8.4 判成 queued 或 waiting
  → 放掉到点的 retry、放掉依赖已终态的 dependency
  → 检查旧子进程与未完成调用
  → 按 Session 顺序恢复原 Run
  → 可继续的提交统一队列
  → 需要人答复的保留 waiting（它们已经在 §7.5 的清单里）
  → 周期扫描（reconcile），补齐内存通知丢失的任务
```

**就绪（HTTP 监听起、发现文件有效）之后**才做两件不影响调度事实的事：渠道起来后按平台补发它名下积压的投递，以及启动时那一次整体补发（§11.4）。两件都在后台，就绪不等它们——它们是网络 I/O，积压多少就等多久；顺序上唯一的硬要求是**渠道登记之后**（§3）。

启动扫描就是 §8.9 的对账，它和 Cron、介入答复和手动 resume 共用同一个领取入口。领取通过数据库条件更新与递增代次保证同一 Run 只被一个执行者接管；旧代次不能继续提交新状态。单实例使用进程锁与数据库事务即可，不引入跨机器选主或分布式队列。

领取就是下面这条语句，`rows affected == 1` 是接管成功，`== 0` 是别人先到（表名 / 列名以 store 的模型为准）：

```sql
-- RunQueue::claim：领取 = 条件更新 + 递增代次 + 起租约
UPDATE runs
   SET state            = 'running',
       claimed_by       = ?1,   -- 本次启动身份（执行实例 ID）
       claim_generation = claim_generation + 1,
       claimed_at       = ?2,
       lease_until      = ?3    -- ?2 + 租约；跑的过程中由心跳续租
 WHERE id               = ?4
   AND claimed_by IS NULL
   AND (state = 'queued'
        OR (state = 'waiting' AND wait_kind = 'retry' AND wake_at <= ?2))
   AND EXISTS (SELECT 1 FROM sessions s
                WHERE s.id = runs.session_id AND s.state IN ('active','closing'))
```

候选由一条普通查询给出，领取一个一条。**"现在就能跑"这件事只有一处定义**，两份查询同一个谓词；同 Session 的次序也在这里（`earlier` 那段，§8.4）：

```sql
-- RunQueue::due
SELECT r.id FROM runs AS r
 WHERE r.claimed_by IS NULL
   AND (r.state = 'queued'
        OR (r.state = 'waiting' AND r.wait_kind = 'retry' AND r.wake_at <= ?1))
   AND EXISTS (SELECT 1 FROM sessions s
                WHERE s.id = r.session_id AND s.state IN ('active','closing'))
   AND NOT EXISTS (
       SELECT 1 FROM runs AS earlier
        WHERE earlier.session_id = r.session_id
          AND earlier.id         < r.id
          AND earlier.state NOT IN ('completed','failed','cancelled','abandoned'))
 ORDER BY r.wake_at
 LIMIT ?2
```

领取之后，这个执行者写账本的每一条状态提交都带同一道代次围栏；`rows affected == 0` 意味着自己已经是旧代次，**停止这个任务的一切写入**，不重试、不降级。状态与"在等什么"一次写完——它们本来就是同一条事实的两个维度：

```sql
-- 代次围栏（每次状态提交；wait_kind / wait_ref / wake_at 只在 state='waiting' 时有值）
UPDATE runs
   SET state = ?1, wait_kind = ?2, wait_ref = ?3, wake_at = ?4, updated_at = ?5
 WHERE id = ?6 AND claim_generation = ?7 AND claimed_by = ?8
```

回收**只交还领取权，不判状态**：

```sql
-- 启动回收：旧实例遗留的 running，按实例逐个交还；
-- 名下还有在册子进程活着（或核实不了）的实例不交还（keep）
SELECT DISTINCT claimed_by FROM runs
 WHERE state = 'running' AND claimed_by IS NOT NULL AND claimed_by <> ?1
UPDATE runs SET claimed_by = NULL
 WHERE state = 'running' AND claimed_by = ?1   -- 不在 keep 里的每个旧实例各一次

-- 租约过期：同一个实例里 handler 任务死了/卡住了
UPDATE runs SET claimed_by = NULL
 WHERE state = 'running' AND claimed_by = ?1 AND lease_until < ?2
```

交还之后那一行的形状是 **`running` 且 `claimed_by IS NULL`**——一个可查询的孤儿（§8.9 的 `unowned_running`），它**不可领取**，所以不存在"调度器抢在 reconcile 前面把它领走"的竞态；同一次对账立刻按 §8.4 把它落成 `queued`（尾部安全）或 `waiting + intervention`（副作用不明）。**判断在这里，不在 SQL 里**：那条 UPDATE 看不见 JSONL 尾部，也看不见上一条调用停在哪。

**存活判定在交还之前**（2026-10-12 修）：旧实现先一刀交还、再由扫描问"上一个执行实例停了吗"，可那时 `claimed_by` 已经是空的，答案永远是"停了"——旧子进程还活着也拦不住重复执行。现在按 `runtime/children` 的子进程登记先判出停不下的旧实例，它们名下的不交还，扫描看见领取者还在，停成 `blocked`。名单只读登记文件、不读库：对账的第一个库操作得是写（见 `service::start` 第 7 步那条旧快照的注释）。

**停在旧执行者上的会自己接着跑**（2026-10-13）。"旧子进程还在"多半是暂时的——它跑完就没了（实测那一次是 `komo gateway restart` 自己：它在等新 Gateway 就绪，新 Gateway 的恢复扫描看见它还活着，§3）。以前停成 `blocked` 时一并清掉 `claimed_by`，之后谁也答不出"它停了没有"，周期对账只会判原样停着，`resolve` 也一样（§7.5），只剩 `abandon`。现在这一种单独一个动作（`RecoveryAction::WaitForPreviousExecutor`）：停成 `waiting + intervention` 进清单，理由写明"它退出后会自动接续"，**`claimed_by` 留着**——等人的行本来就不在领取谓词里，留着不会被领走，放回队列时一并清掉。对账每一拍见到"等人 + 领取者还在"就重新探那个实例的子进程：还在 → 一个字不写；停了 → 按 JSONL 尾部接着判（多半是 started 而无结果，按 §8.6 交给模型），并叫醒调度器。

**租约只用来发现"没人管了"，不用来抢活。** handler 每 `TTL/3` 续一次 `lease_until`；对账见到过期只是**信号**，还要过一道"这个持有者真的不在（`claimed_by <> self`）或我自己内存里已不再持有它"才回收。少这一道，一次二十分钟的调用会在主人还活着的时候被第二个执行者抢走——那不是恢复，那是真的重复副作用（§8.6）。

四条都通过 `toasty::sql::statement(...).bind(...).exec(&mut tx).await?` 执行，`u64` 就是受影响行数。**竞争失败的第一手信号是错误而不是 0 行**：并发竞争同一行时，败者拿到 `serialization failure`（MVCC 事务里是 `Write-write conflict`，autocommit 是 `database is locked`），只有在胜负已分之后再跑才会拿到 `0`。因此 `claim` 也包在 `with_write_retry` 里——它是少数几个「可以安全重跑的写」，因为守卫写在 `WHERE` 子句里：重跑一次要么还是没人领（赢），要么已经有人领了（`0`，判为输）。实测 120 轮（raw）+ 100 轮（走 toasty）、每轮 2–4 个竞争者，恰好一个赢家，没有 0 赢家也没有多赢家（`spikes/store.md`）。

JSONL 追加和状态提交都校验领取代次。领取代次只防止旧执行者写账本，不能撤销已经发出的外部动作。因此必须先确认旧执行已停止，再考虑启动替代执行。

首版不承诺让 shell 进程本身跨 Gateway 重启继续存活。正常停机时停止接收新执行，给正在完成的工具短暂收尾时间，持久化可用结果；超时则回收所属进程及子进程，剩余任务交还领取权、由对账判成 `queued` 或 `waiting + intervention`。关键文件原子修改完成后再停，不在半次替换中主动取消。

异常退出可能遗留子进程，不能仅凭一个 PID 判断是否为原进程；结合平台监督信息、启动身份和进程启动时间核对。无法确认旧执行已结束时，阻止该任务重复启动并显示原因。Tokio 的 Child handle 被丢弃并不默认终止进程，因此 kill_on_drop 不能取代异常退出和进程树回收方案。[Tokio process](https://docs.rs/tokio/latest/tokio/process/index.html)

长脚本要拆成多个可观察的工具步骤，或由脚本把阶段进度写进文件。首版恢复到工具调用边界，不恢复脚本的内存、调用栈或某一行。脚本只有“进度标记”而没有副作用核对，仍不足以保证安全续跑。

### 8.8 恢复所需的上下文与用户体验

未完成 Run 引用的 Session 目录及其 JSONL、payloads、tool-output、产物，还有外部工作目录、代码与环境快照和配置，都不可被普通清理策略删除，**也不许直接 `rm`**：删除只有 `komo session delete` / `purge` 一条路，走 §8.10 的状态阶梯。tool-output 不作为临时日志按天直接清扫；已结束会话清理时也先处理恢复、Memory 和审计引用，显式清理后不得将缺失输出伪装成仍可回放。目录丢失、脚本版本不可用、权限被撤销或模型协议不再可回放时，保存具体原因并等待处理，不能换到另一个目录或新版脚本自动重来；**目录丢了而 Session 状态不是 `purged`，是"数据库与内容对不上"的一种，由 reconcile 报出来（§8.9）**，不是让下一次运行对着空上下文说话。

原始请求来源与操作者身份也要保存。Cron 恢复后仍是 Cron，不能因为重启后由本机 Gateway 发起，就取得交互操作者的额外权限。Memory 的过期、遗忘和版本检查照常进行。

CLI 不承担恢复调度。打开 komo 时可以看到“2 个任务已接续，1 个等你核对”——**真有事才说这一句**，没有未完成的任务是默认情况，不占一行（同理，补读了几条事件不用报：正在补读状态行上说过了，读完屏幕上就是那段历史本身）；komo resume SESSION_ID 连接到原任务，多个客户端同时连接不会开启多个执行。自动恢复无需再次询问是否继续，只有过期动作、冲突或无法核实的效果才打断用户。

**退出 TUI 就是暂停（`Ctrl-C`）**：Run 留在 Gateway 里照跑，退出时印出 `komo resume <会话>`——会话 Id 平时只在开场那条横幅上闪过一次（裸 `komo` 那条路上会话还没铸出来，由第一条消息之后的一条提示补上），退出这一刻正是需要它的时刻。暂停在任何界面状态下都有效，模型菜单开着也一样（菜单里的 `Esc` 是关掉菜单，不是退出）。一个字没发就退出时还没有会话，什么都不印。终端本身没了（崩溃、重启、升级重开）时同一行由终端替人敲：有会话就用 OSC 88（Terminal Resume Protocol，参考实现 Otty）声明 `komo resume <会话>`（home 会话是 `komo home`，带 `self_repaint=1`，历史由补读自己画），人自己退出时发 `clear` 撤回；进程被杀来不及撤回，声明留着，正是该被拉回来的时候。不认 OSC 88 的终端整段忽略（2026-10-20）。

SSE 从已同步且索引完成的 JSONL 事件补读；介入当前状态可直接查询 state.db，不依赖用户一直在线。所以**事件流里看不见恢复扫描停下的 Run**（那一笔只在 state.db 上）：TUI 断线重连成功时说一句"事件订阅已恢复"并问一次待处理清单；清单上有当前那条 Run 而事件折出来还没结束，状态行按"等待中"说，不对着一条停住的 Run 一直转；清单上那条 Run 再来任何状态帧就再问一次清单，免得已经接着跑的那条一直挂在状态行上（2026-10-13）。答复同样可以来自这个界面之外——另一个终端的 `komo intervention answer`、聊天里的 `/answer`、模型在 shell 里替人答的那一条——它只改 state.db，本会话的事件流里没有它：清单上挂着东西且订阅连着时，TUI 每 5 秒悄悄再问一次；本会话的 Run 结束而清单非空时立刻问一次。这两种问回来的清单只换掉状态行的条数与提示行，不印全文（要看的人有 `/pending`），取失败只记日志、下一拍再问（2026-10-13）。事件流读到一半断掉（对端进程没了）报"Gateway 断开了连接，可能正在重启"，原话留在括号里。之后接入 Telegram 等主动推送渠道时，应为待发送结果持久保存投递记录，不能为补发一条结果消息重跑任务。

用户处理 uncertain 时，应先看到原操作、已有证据和待确认事项。如果决定终止，则补齐明确的取消 / 未知结果；若允许继续，也不能伪造原调用成功。任何后续模型请求都保持完整的调用与结果配对。

### 8.9 reconcile：DB 是权威，其余一切都是派生

权威只有一处（§8.1）：**state.db 说"应该是什么"，Session 目录说"内容是什么"**。两者之间没有跨文件事务——先 `sync_all` 再提交数据库（§8.5），所以任何时刻崩掉都可能留下"半个事实"。今天处理半个事实的办法是**报损坏、然后停下**；它对真正的损坏是对的，对**没有主人的状态**（orphan）是错的：一条 handler 任务自己 panic 掉的 `running`、一个被手工 `rm -rf` 掉的会话目录、一条永远不会有人来答的 `waiting + intervention`，都会让整个 Session 从此停在那里，而原因只落在日志里。

reconcile 是一次**只读观察 + 只写状态**的对账，它回答三个问题，且只回答这三个：

| 观察 | 结论 |
|---|---|
| 这条未完成的 Run，它的 Session 还在服务范围里吗（§8.10：只有 `active` / `closing` 服务）？内容读得出来吗（目录、JSONL、被引用的输出）？ | 能 → 交给 §8.4 决策表逐行走；不能 → **不许领**，把这条 Run 停成一个 `blocked` Intervention（§7.5），理由写清是"会话已删"还是"内容缺失" |
| 这条 `running` 的 Run，它的 `claimed_by` 还活着吗（租约没过期，或过期了但持有者确已结束）？ | 不是 → 按 §8.7 **只交还领取权**（状态仍是 `running`、没有主人 = 一个可查询的孤儿），再由 §8.4 决定续跑还是核对 |
| 这条停在等待上的 Run，条件满足了吗？ | `retry` 到点了 → 放回 `queued`；`dependency` 的前一条进终态了 → 放回 `queued`；都还没 → 原样不动 |

**它永远不做三件事**：不调用工具、不发送外部请求、不改写任何内容——§8.5 对回放的限定原样适用。它写下的每一条状态都走正向顺序（先 JSONL 后数据库，或纯数据库的状态提交），并且幂等：同一批输入跑十遍与跑一遍结果相同。

**本进程自己领着的 Run 不进这一趟**：`claimed_by` 是当前实例（实例身份每次启动新生成）的行，要么正在本进程手里跑，要么 handler 已经不在；后者先由上表第二问按租约与存活判定交还领取权、再判。对"上一个执行实例停了吗"它只能答"没停"，拿它去判会把一条刚领走、handler 还没开跑的 Run 停成 `blocked`。

**写回是条件更新**：观察与写回之间隔着好几次读，这一行可能已经被领走（领取代次涨了）、受理落定或被放行；写之前核对状态、等待、`claimed_by` 与领取代次仍是观察时的样子，不是就不写，下一拍按新样子再判。

**触发点三个**：Gateway 启动（§8.7 那次扫描就是它）、`AUDIT_TICK` 那一拍的周期兜底（§8.5 的补写周期顺手做一次，代价是几十条 `SELECT`）、以及显式的 `POST /v1/reconcile`（`komo doctor --reconcile`）。首版**不做**异步 GC、不做引用计数、不做跨 Session 死链自动修复——那些不是当前的主要矛盾。这里要求的就是三件事：**逻辑删除可靠、不重复副作用、orphan 能被自动发现并说清**。

一条硬约束顺带落在这里：**领取一个 Run 之前必须确认它的会话还在服务**。今天的 `DUE_SQL` / `CLAIM_SQL`（§8.7）只看 Run 自己的状态与同 Session 的先后次序，于是"目录被手工删掉、Run 还在队列里"会被**照常领走并按空上下文执行**——那是把一个已经不存在的对话续上一轮，比停下来更糟。两条 SQL 都要加"Session 存在且状态可服务"的守卫；reconcile 负责把这种情况**说清楚**，守卫负责让它**不发生**。

### 8.10 Session 生命周期：Closing → Deleted → Purged

Session 不是只能追加的对象。删掉一个会话今天等于 `rm -rf sessions/{id}`：数据库那一行还在、未完成的 Run 还在队列里、下一轮模型请求会带着空上下文跑起来，而操作者手上没有任何一条命令能做对这件事——这正是 §7.5 开头那个现象的另一种形态。给出三个状态，**删内容只能是最后一步，而且必须有人明确下令**：

| 状态 | 谁能进 | 新输入 | 队列 | 内容 | 列表 / resume |
|---|---|---|---|---|---|
| `active` | 默认 | 接受 | 正常 | 在 | 列出，可 resume |
| `closing` | `komo session delete`（逻辑删除的受理） | **拒绝** | 未完成的 Run 照 §8.4 跑完或停在等待；**不新开** | 原样不动 | 列出并标注"正在关闭"，可 resume 看最后一程 |
| `deleted` | `komo session delete --now`，或 reconcile 判定"已无未完成 Run" | 拒绝 | 空 | 仍在——逻辑删除**不碰内容** | 默认不列出（`--all` 可见），resume 拒绝并说明 |
| `purged` | `komo session purge <id>`，且前置检查全过 | 拒绝 | 空 | **已删除**，只剩墓碑行 | 不列出；`komo session show <id>` 仍答得出它曾经存在、何时被回收 |

四条规则：

1. **状态权威是 `sessions.state` 一列**（`ALTER TABLE ADD COLUMN`、`NOT NULL DEFAULT 'active'`，§8.2 只允许加列），外加一个"状态变更时刻"。JSONL 里追加 `session.closing` / `session.deleted` / `session.purged` 作为**审计副本**——走 §8.5 的反向补写：数据库先提交，事件随后补写；读事件**不产生状态**。**所属会话已经 `purged` 的 outbox 行不补写，直接标成已交付（`seq` 记 0）**：那份 JSONL 按第 3 条已经删了，审计副本没有地方落，硬写只会每拍报一次"已提交范围缺失"、永远留在 outbox 里，攒到补写器一拍取的上限（128）还会把后面所有会话的审计事件挡在外面。实测（2026-10-15）：`purge` 排完 `session.deleted` / `session.purged` 就删目录，补写器跑在删之后的那 7 个会话留下 17 行（含一个会话残留的旧 `approval.decided`），每分钟各报一次 WARN 报了三周；同一代进程里写入器还握着已删文件的句柄、补写会"成功"写进 unlink 掉的 inode，所以只在重启后的那一代卡死。
2. **`closing → deleted` 是 reconcile 的判定，不是时钟**：只有这个 Session 再没有非终态 Run 时才推进。在那之前它一直是 `closing`，而"还有谁没跑完"正好是 §7.5 清单答得出的。操作者要立刻走完，`komo session delete --now` 把未完成的 Run 全部按 `abandon` 处置（各写一条明确的取消），再进 `deleted`。
3. **`purged` 之前先算引用，而且状态先落、内容后删。** `memory_evidence` 指向这个 Session 事件的行、`checkpoints`、`deliveries` 里未送出的行、`cron_firings` 的 Session 引用都要先处置——停用无法再验证的记忆条目、把投递标成终止——然后：**数据库先提交 `purged` 这个墓碑，再删内容**，最后对账把"标了 `purged` 而内容还在"的会话收尾（幂等，删一半被杀也不会留下说不清的状态，因为 `purged` 已经声明了"这个目录要没了"）。**删不掉就说清楚，不假装成功**（§8.8 的原话照旧）；引用检查不过就 409 并列出要先处置什么。`purged` 之后 `jsonl_path` 这类列保留原值，但那个目录不该再被创建或读取：任何试图往 `purged` 会话写内容的路径都是 bug，不是"重新开始"。
4. **禁止直接 `rm`。** 三层：命令层——删除只有一条路（`komo session delete` / `purge`），没有"手工删目录"这条经验路径；工具层——`write` / `edit` 拒绝写数据目录下的 `sessions/`、`state.db*`、`runtime/`、`.env`（§7.2，komo 唯一的硬限制），bash 拦不住（2026-10-11 之前还有一层 `command_patterns` 手滑网，随 Policy 删掉，§7.3）；reconcile 层——真正的保证在这里：目录没了而状态不是 `purged`，对账会把它**报出来**，而不是等下一轮模型对着空上下文说话。

## 9. Memory：自动积累、可信来源与混合检索

Memory 的规格在 [`docs/memory.md`](memory.md)（2026-10-19 从本节迁出），小节号不变，本文其他地方的 §9.x 都指那里：

| 节 | 内容 |
|---|---|
| §9.1 | 两类内容分别保存：用户主动保存的笔记在 Memos，自动记忆在 state.db 的 `memory_items` / `memory_evidence` |
| §9.2 | 数据模型：kind、scope、provenance、confirmation、state、证据引用、revision |
| §9.3 | 自动积累：会话空闲后后台按批提取、校验、去重与冲突整理，只收翻不到的东西 |
| §9.4 | 混合检索：关键词（`memory_terms`）∪ 向量 → RRF → 可选重排；注入按对话段钉住 |
| §9.5 | 向量索引的空间指纹、代次切换与自动补齐 |
| §9.6 | 冲突、失效与遗忘 |
| §9.7 | 与 resume 和权限的关系 |
| §9.8 | 离线整理（设计，未实现） |
| §9.9 | 判关系时候选用编号（设计，未实现） |
| §9.10 | 按会话空闲后提取，提取与判关系一次调用（部分实现：按会话提取已落地，合并判关系未做） |

同一文档的附录收着外部记忆仓库调研（graphiti / mem0 / OpenViking）与此前的改进方案、差距分析。

## 10. Cron

Cron Scheduler 只负责产生 Run，复用 AgentLoop、工具和存储。

```text
发现到期
 → state.db 事务插入 cron_firing、触发快照并预留 Session / `accepted` Run
 → JSONL 持久保存触发输入，state.db 提交 queued
 → 普通队列执行
 → 保存结果和产物
 → 更新本次触发状态
```

每个 Job 包含名称、五字段 cron 表达式、时区、prompt、工作目录、enabled、执行预算、重叠策略、`notify` 与版本。`notify` 三档 `always`（默认）/ `on_error` / `never` **只过滤结果的投递**；Run 停在待处理 Intervention（阻塞，§7.5）上时三档都投 home chat——那是任务在问，不是在报告，一条没人看见的提问等于这个 Job 从此停在那里。每次触发是一条带状态的记录（`queued` / `running` / `ok` / `error` / `waiting` / `skipped`），重叠跳过与错过太久（超过 Job 自己的间隔）都留一条 `skipped`，`@at` 一次性永不过期。可指定该 Job 的主模型与 effort；覆盖按完整模型配置解析，不能影响记忆整理或向量模型。下面示例所需的搜索与 Memos 操作仍须匹配具体的已启用模块版本。

```bash
komo cron add --name morning-summary \
  --schedule "0 9 * * *" \
  --timezone Asia/Shanghai \
  --prompt "搜索今天关注的技术动态，整理后保存到 Memos"

komo cron list
komo cron run JOB_ID
komo cron pause JOB_ID
komo cron resume JOB_ID
komo cron remove JOB_ID
```

模型需要管理 Cron 时通过 bash 调用这些命令；CLI 连接 Gateway，不直接访问数据库，也不增加专用 cron tool。修改调度属于持久副作用，执行计划展示完整调度与指令。

默认行为：

- 唯一键 `job_id + scheduled_at_utc` 防止同一计划时间重复创建运行。
- 同一 Job 上一次仍未结束（含等待重试或结果核对）时，跳过本次并记录原因。
- Gateway 停机期间错过的触发不集中补跑；停机前已经持久创建、尚未完成的 Cron Run 则按原 Run 自动恢复，这是两种不同情况。
- 时区明确保存（IANA 名，`TimeZone` 只存名字）；同一日历时间因夏令时出现两次时，两次不同 UTC 时刻分别视为计划时间；不存在的本地时间跳过。**这两条与 croner 的内建策略相反**（它把缺口里的固定时刻抬到缺口之后、把重复的只算较早一次），所以由 kernel 的 `cron::search_from`（跳缺口）和 `cron::repeated_twin`（补后一半）自己实现，换 croner 版本时要一起看。时区解析走 `ZoneResolver` port（§13.5）：kernel 不带 tzdb，真 tzdb 实现在 runtime；没有 tzdb 时 `FixedOffsetZone` 是**明确的降级**（答不出重复与缺口，等于当这个区没有夏令时），不是默认。
- 手动 run 使用独立请求幂等键，不冒充定时触发。
- 无人值守不改变执行规则：Cron Run 与交互 Run 一样，没有 Policy 与审批（§7）；需要人判断的只有阻塞，照常投 home chat；结果不明交给模型。
- Cron 的结果去原 Session 查看，也可以用 resume 接手。

### 10.1 命令直跑模式：`command` 与 `prompt` 二选一

Job 加一个 `command: Option<String>` 字段，与 `prompt` 二选一——CLI（`--command`）与
HTTP（`CreateCronRequest.command`）两处都校验：不能同时给，也不能都不给。命令 Job 的
`prompt` 列写空串（退役字段照写空的惯例，反过来给两种 Job 共用一张表用，§8.2）。

```bash
komo cron add --name nightly-backup \
  --schedule "0 3 * * *" \
  --timezone Asia/Shanghai \
  --command "tar czf /backups/$(date +%F).tgz /data" \
  --workdir /srv/app

komo cron list   # 命令 Job 那一行下面多印一行 `[命令] tar czf ...`（过长截断）
```

**不另开一条执行路径，只是把"模型"换成一个固定出牌的驱动。** `AgentLoop` 平时靠
`LlmClient::begin_turn` 拿一个 `TurnDriver`（§6）；命令 Job 触发的 Run 用
`CommandDriver` 代替它：第一轮固定给一个 `bash` 工具调用（命令 = Job 的
`command`，工作目录不必显式传——它就是这个 Run 的执行环境 `cwd`，受理时已经从 Job 的
`workdir` 落到 Session 上，§8.5），第二轮拿到工具结果后收尾。选哪个驱动在
`AgentLoop::run` 里按 Run 的来源判：`self.llm`（真正的模型客户端）一次都不会被摸到，
`Segment` 上的 `command: Option<CommandSpec>` 就是那个判据。于是 JSONL / tool-output /
恢复 / 投递全部原样复用——那一次 `bash` 调用照样要过 `prepare` → `execute`，跟模型
发起的调用走的是同一条路。**命令
Run 全程零模型请求**，也不触发记忆提取（`AcceptInput.skip_memory`：命令 Job 触发的
Run 落账时直接记 `MemoryWork::Done`，不进后台处理队列——没有对话可提取，提取本身还要
发一次模型请求，会打破"零模型请求"这条硬约束，§9.3）。

**恢复安全是 `CommandDriver` 自己的责任，不是免费的。** 普通模型驱动不怕"上一次的调用
结果已经落盘、收尾事件还没写"这种重启：它靠 `TurnRequest.messages`（这一段的回放窗口）
里那条已经落盘的调用与结果认出"这件事做过了"。`CommandDriver` 不读 `messages`（它压根
不构造模型请求），所以在构造时自己查一遍回放窗口：已经有那条 `bash` 调用的结果就直接
用它收尾，没有才第一次发那个调用——不然重启一次会把同一条命令再跑一遍。

语义（触发的 firing 状态与投递正文，都由 `service::cron_watch` / `run_watch` 现读那次
`bash` 调用的落盘结果决定，不信 `CommandDriver` 收尾用的投影正文）：

| 情况 | firing 状态 | 投递 |
| --- | --- | --- |
| 退出 0、stdout 非空 | `ok` | stdout（trim）原样作为结果投递，受 `notify` 过滤 |
| 退出 0、stdout 为空 | `ok` | 不投递——看门狗"没事不说话" |
| 非 0 退出 | `error` | 投错误摘要（退出码 + stderr 末尾若干行），受 `notify` 过滤（`on_error` / `always` 投，`never` 不投） |
| 超时 | `error` | 与任意 shell 命令超时同一条路（§8.6：`NoSafeRecovery` 的调用超时是"副作用是否已经发生未知"，执行器不自动判定成功或失败、不重跑，记一条 uncertain）。命令 Job 没有模型可交，`CommandDriver` 照常收尾，这次触发记 `error` 并投一句"结果读不出来、去原 Session 看"，不再停着等人——命令 Job 不为此另开一条自动归类的路径，那会绕过 §8.6 的安全不变量 |

**`cron add` 不签授权**（2026-10-11 起，§7）：触发时那一次 `bash` 调用与任何调用一样直接执行，
没有授权要签，也就没有"模型经 bash 调 `komo cron add` 给自己签一条将来免问的许可"这个口子要堵。
此前那套（`cron add` 落一条 `GrantScope::CronJob` 授权并挂一条已决定的审批记录、strict
基表对 `komo cron add` 单加一条只能批一次的 Ask、`komo cron list` 的授权列）随 Policy 一起删掉；
老库 `policy_grants` 里那些行留着没人读（§7.4）。

目前没有 `cron edit`：命令改了要 remove 重建。`remove` 只删 `cron_jobs` 那一行
（§13.1「移除后续调度，已有执行历史保留」）。

触发输入在 JSONL 持久保存且 queued 已提交后再通知内存队列；启动与周期扫描负责补齐未完成接收、索引落后和通知丢失的情况。

## 11. 聊天入口与介入

聊天渠道复用 Gateway 的会话、运行和介入接口，并且是**介入的主入口**——操作者大多不在终端前。TUI 与 CLI 是人在终端前时的辅助入口。三个渠道首版都在。

### 11.1 渠道与分发

```text
飞书 ws 长连接 ─────────┐
Telegram long polling ─┤
WeChat iLink (DM) ─────┼─→ Dispatcher::handle(InboundMessage)
HTTP API (TUI / CLI) ──┘      1. request_key 去重（durable，§8.5 的请求键）
                              2. 解析 Principal（config.toml 的 `allow_from`）
                              3. 解析 Conversation → SessionId
                              4. 冲刷该会话 pending 的投递（§11.4）
                              5. 命令？→ 直接处理并 ack
                              6. 普通文本 → Ledger::accept_input → 排队
                       回复：订阅该 Session 的事件流，Run 终态时把最终 assistant 消息作为
                             `Outbound::RunFinished` 投回来源会话（飞书更新回复卡片，§11.3）；
                             待处理 Intervention（结果不明、阻塞）走 Notifier（§11.4）
```

- `request_key`：飞书是 `feishu:{event_id}`（`header.event_id`，消息事件与卡片回调同一个字段；官方对 2.0 事件的去重建议就是「通过事件结构中的 `event_id` 字段判断事件唯一性」），Telegram 是 `telegram:{update_id}`（long polling 重投的单位是整个 `Update`，`offset` 未推进就原样再取一次，普通消息与 `callback_query` 因此共用这一个键），WeChat 是 `wechat:{from_user_id}:{client_id}`——iLink 的 wire 消息**没有 `msg_id`**，字段只有 `from_user_id` / `to_user_id` / `client_id` / `create_time_ms` / `message_type` / `message_state` / `context_token` / `item_list`，唯一的逐消息标识是 `client_id`（**发送方**微信客户端生成的 UUID，不是服务端投递 id），所以加 `from_user_id` 前缀，`client_id` 为空时回退到 `(from, 内容哈希, 60s 窗口)`；微信的重投有两个来源——拉取游标只活在进程内存里、重启从空游标开始，以及服务端回了消息却回空游标时 SDK 把同一批原样再交一次——所以微信的去重是必需的，不是以防万一。飞书 / Telegram 不带 `chat_id` 前缀：两个 id 在单个应用 / 单个 bot 内已唯一，而 Telegram inline 模式的按钮回调根本没有 chat，硬拼会逼出占位值。三个平台都是至少一次投递（飞书官方原话「即使成功接收，仍会收到重复消息」；未在 3 秒内响应会按 15s / 5min / 1h / 6h 重推最多 4 次，ws 长连接同样有超时重推），重发命中同一 Run；**命令也去重**——重发的 `/approve` 不会批准两次。这正是 §8.5 请求键的语义，不另做 inbox 表。
- **去重键只管平台重投，不管用户连点**：两次真实点击带着两个不同的 `event_id` / `update_id`，是两条合法输入。挡住它的是介入答复本身的幂等（§11.3：已决定的返回原决定）。两层分开，不要用组合键去兼职。
- 飞书 ws 在独立线程上跑 openlark 的运行时，事件通过 channel 交给 tokio 侧的 `Channel::serve`；ack 在 Dispatcher 返回 `InboundAck` 之后发，不是收到就 ack。
- 渠道交给 Dispatcher 的是 `InboundMessage { peer: ChannelPeer, is_private, sender, text, request_key }`，**不是 Session ID**。哪个会话属于 Dispatcher 与存储。

### 11.2 身份与会话

**渠道的身份匹配信息全部在配置文件里，不进 state.db。** 谁是操作者、哪些会话可以说话、哪个会话是 home chat，都是操作者对自己机器的一次性声明——和模型配置是同一类东西，改动就是编辑文件，Gateway 热重载（§3）后立即生效，没有配对码、没有 `komo pair`、没有 `/sethome`，也没有对应的表和 HTTP 接口。原因：这份信息只有操作者一个人写，写它的场合一定是人在终端前；放进数据库只会多出一套 CLI、一组接口和一张耐久表来维护一个本来就该是静态的名单，而数据库丢了还要靠这张表决定谁能下指令。

```toml
# ~/.komo/config.toml —— 行为键
[channels.feishu]
enabled    = true
allow_from = ["ou_xxx"]            # 操作者的 open_id；为空则没人能说话
home_chat  = "oc_xxx"              # 主动投递（介入通知、Cron 结果等）的目标会话
guest_agent = "guest"             # 群里非操作者 @机器人 时用的 Agent；不写 = 非操作者一律拒绝

[agents.guest]                     # 访客 Agent：tools = []、memory 为 private/off、project_files = false（见下文「群里的访客」）
tools = []
memory = "private"
project_files = false
instructions = "你在一个群里回答同事的问题……"

[channels.telegram]
enabled    = true
allow_from = [123456789]           # 操作者的 user id
home_chat  = 123456789

[channels.wechat]
enabled    = true
allow_from = ["wxid_xxx"]          # iLink 的 from
```

```dotenv
# ~/.komo/.env —— 只放凭证；config.toml 里没有任何 secret
FEISHU_APP_ID=cli_xxx
FEISHU_APP_SECRET=...
TELEGRAM_BOT_TOKEN=...
# 微信凭证由 `komo channel wechat login` 写到 ~/.komo/wechat/credentials.json，不在 .env
```

- **Principal**：发送者在该渠道 `allow_from` 里就是操作者，否则**拒绝**——回一条固定提示，带上发送者在该平台的 id（`ou_xxx` / `123456789` / `wxid_xxx`），操作者把它抄进 `allow_from` 即可；消息不进入 Run，也不留任何记录。`allow_from` 为空的渠道等于只出不进：还能作 `home_chat` 收投递，但没人能通过它下指令。**`/answer` 等命令只接受操作者。**
- **怎么知道自己的 id**：任何人对机器人说 `/id`，机器人回 `{platform}:{chat_id}` 与发送者 id——被拒绝的提示里也带着同样的信息。这是唯一的"发现"手段，没有别的准入流程。
- **Conversation**：操作者的**私聊**（Telegram DM、WeChat、TUI）落到同一个 **home session**——早上在微信说的话，回到终端接着说；飞书私聊例外，与飞书群一样一个话题一个 Session（见下文「飞书群里的话题」）；飞书 / Telegram 的群聊按 `{platform}:{chat_id}` 各自一个 Session（飞书群里改为一个话题一个 Session，见下文「飞书群里的话题」）。**没有群名单**（`groups` 2026-10-22 删除，旧配置里的这个键只报"不认识、已忽略"）：机器人所在的群里只响应 @机器人 的消息并剥掉提及；发送者在 `allow_from` 里就是操作者、照常全能力，不在的走「群里的访客」（下文）或被拒。已删除（或正在关闭）的会话不再接收平台消息，同一来源的下一条另开新会话。WeChat 只有 DM。要跑很久的事派给独立的任务会话在后台跑，结果交回 home session 由它转告，home 不被一件慢事堵住，见 `docs/background-tasks.md`。
- **改名单不改数据库，也不重启**：`allow_from` / `home_chat` / `guest_agent` 每条消息、每次投递都从当前配置快照读（§3 热重载第 2 步），文件保存后下一条消息就按新名单判定；`komo config check` 与重载共用同一套校验（id 形态、`home_chat` 所属渠道必须 enabled 且有凭证），校验不过则旧名单继续生效并在 home chat 报错。`komo doctor` 把"某渠道 enabled 但 `allow_from` 为空"当作警告列出。
- Cron 触发的 Run 来源仍是 Cron（§8.8）；在聊天里 `/answer` 它的介入，不会让它获得交互操作者的身份。
- **聊天里发来的图片存成本地文件，正文只给路径**（2026-10-22，`channels/inbox.rs`）。入站消息只有文本，所以图片不进消息结构：渠道把它下载下来，按内容哈希存成 `{workspaces_dir}/inbox/{平台}/<哈希前 16 位>.<扩展名>`，在正文**最前面**加一行 `[用户发来一张图片，已存到 <绝对路径>，用 read 查看]`，用户的文字（附言）在后——附言是 `/pending` 时不会被当成命令。模型自己用 `read` 看（§8.3 的工具结果带图），代价是每张图多一轮模型往返，换来 Dispatcher、受理、回放、记忆提取一行都不用认识图片。下载失败不丢消息，正文改成 `[用户发来一张图片，但没能下载：原因]`。先判断这条要不要处理（群里没 @、机器人自己回流的）再下载；同一张图重投落在同一个文件上；单张上限 20 MB（Telegram `getFile` 的上限）；认不出格式也照存（`.bin`，HEIC 之类让模型先 `sips` 转）。放在工作区而不是数据目录：`read` 与 codemode 都读得到。各渠道：微信用 SDK 的 `CdnClient` 下载再 AES 解密；飞书单发的 `image` 与带图的富文本 `post` 都收，走 `GET /im/v1/messages/{id}/resources/{key}?type=image`（**要应���开 `im:resource` 权限**）；Telegram 取 `photo` 里最大的尺寸、附言一起带上，按文件发的 `image/*` 也收。语音、视频、其他文件仍不处理。

### 11.3 介入在聊天里

聊天里需要人回答的只有 §7.5 的两类：**结果不明**（`verify`，2026-10-12 起不再产生，只剩升级前停下的旧 Run）与**阻塞**（`blocked`）。2026-10-11 之前这里的主角是审批——短 ID、批准 / 拒绝按钮的交互卡片、`/approve` 的三种范围、`y` / `n` 快捷答复、`/approve all` 批量——随 Policy 一起删掉（§7）；`/approve`、`/reject`、`y`、`n` 现在不是命令，`/approve`、`/reject`、`/deny` 得到一句固定的帮助（"已经不是命令了"，并列出 `/pending` · `/answer` · `/status` · `/cancel` · `/new` · `/id`），其余未知斜杠命令照旧交给模型；`y` / `n` 就是普通消息，哪怕此刻有待处理的介入。

一条介入通知是 `Outbound::NeedsAttention`（会话、Run、理由），三个渠道都渲染成文本：写清是哪条 Run、为什么停、这一条该怎么答（`/answer <run_id> <结论>`）。没有按钮：结论里有"确定没执行"这种分量的选项（它决定模型下一步会不会重做），打一行命令不是摩擦。

命令（三个渠道相同）：

| 命令 | 行为 |
|---|---|
| `/answer <handle> <结论>` | 打 `POST /v1/interventions/{handle}/answer`，答复 §7.5 的两类：`/answer <run_id> satisfied`（核对后目标已满足，原 Run 继续）、`not_performed`（确定没执行，重新入队）、`resolve`（前提已处理，重新观察并重新决策）、`abandon`（取消这个 Run）；已答复的返回原结论，不报错 |
| `/pending` | 列出**全部**待处理 Intervention（§7.5）与各自的句柄（Run ID），并写清这一条该答什么 |
| `/new` | 当前 Session 追加 `conversation.boundary`，不切 Session |
| `/cancel` | 取消该 Session 当前 Run |
| `/status` | 当前 Run 状态、待处理介入数 |
| `/id` | 回显 `{platform}:{chat_id}` 与发送者 id，供抄进 config.toml 的 `allow_from` / `home_chat`；任何人可用，也是唯一不要求操作者身份的命令 |

**TUI 不接管整屏。** 它只占终端**最底下**那一小块**视口**（还在动的那一小段 · 状态行 · 命令面板 · 输入框），会话正文一条条**交还给终端自己的回滚区**：滚轮、选中复制、退出之后留在屏幕上的那段记录，都是终端本来就会做的事，不必在这里重做一遍，而且每条正文只画一次，不随每一帧重算。交出去的行**从此不再改**（例外只有整段重画：下面的 `Ctrl-T`，以及终端被拉大拉小——照 pi，尺寸一变就擦屏、擦回滚区、按新宽度从横幅起重交一遍，否则旧视口被终端折行挪位留下残骸，正文也停在旧宽度上；2026-10-19。整段重画就是把整段会话再写一遍，拖动时只画两次：变的第一下当场画，之后尺寸还在变就不画，停下 100ms 再按最后的尺寸补一次——伪终端里 0.34 秒改 20 次尺寸，原来整段重画 19 次、写出 4.7 MB，现在 2 次、0.5 MB），所以"定型"要判得保守：用户那条消息、助手那段回答、跑完（或所属 Run 已结束）的一次调用、一条提示才交；还没收到 `run.accepted` 的提交、草稿里正在写的那一段、还在跑的调用留在视口里逐帧重画。两边不重叠——定型的那一帧它从视口消失、同时出现在回滚区上。**草稿边生成边交**（2026-10-13）：停在代码块之外的空行之前的段落已经定型——渲染器一块一块出行，这一段单独渲染与整段渲染的开头逐行相同——当场交给回滚区（块与块之间空一行，照 pi，2026-10-15：空行只插在后一块之前、头尾不留，所以接缝上那一行由接着交的那一段补；紧跟在一段话下、原文没隔空行的列表不空；标题一、二级不写 `#`，一级加粗加下划线、二级加粗，三级起才留 `###` 前缀），视口只留正在写的那一段；正式的 `message.assistant` 到了只补余下的部分。否则一段几十行的回答生成期间只露出视口里几行尾巴，往上滚也滚不到开头。定稿与交出去的那段对不上（断线后草稿接错）就照实给出整条定稿并说明；Run 没等到定稿就结束了，交出去的半段留着，补一句没写完。**终端模式（输入以 `!` 开头）不给文字提示**：输入框标题不写说明，跑完也不补"为什么"，洋红色的边框与 `!` 已经说清这一条不经模型；只有发出去之前就注定跑不起来的命令（编辑器、分页器、`sudo`……）仍然当场拦下并说一句，否则按了 Enter 什么都不发生。`Ctrl-T` 因此是一个**模式**而不是对某一条的展开，管的是整段会话（2026-10-14，照 pi 的 Ctrl-O）：切换时擦掉屏幕与终端的回滚区（先 `ESC[2J` 擦屏、回左上角、再 `ESC[3J` 擦回滚区——反过来，有的终端擦屏时推进回滚区的那一页会留下），从开场横幅起按新模式把整段会话重新交一遍，与平常的一帧一样包在同步输出里。代价是进 TUI 之前终端里的内容一并擦掉。提示没有自己的 seq，记下它出现时已经收到的最后一条事件，重交时排回那条之后（同一个 seq 上排在消息与调用后面），不挤到最后；提示只留最近 50 条，更早的重画后就没有了。

视口的位置由客户端**自己记账**，不用 ratatui 的 inline 视口：后者每次定位都要向终端问一次光标位置，而 crossterm 明写着「`event::read` / `event::poll` 正在进行时这一问会阻塞并超时」——TUI 有一个常驻读键线程，两者不能共存。所以只在**读键线程起来之前**问一次，之后视口的顶边与"缩高度腾出来的那几行空档"全靠自己那两笔账；空档在下一次往回滚区写正文时正好被填回去，输入框从三行缩回一行不会在历史与它之间留下一道再也填不上的缝。整帧包在同步输出（DECSET 2026）里，支持的终端等整帧到齐才上屏。

**视口贴着终端底边**：输入框永远在人手指落下去的地方，不随会话长短上下跑。正文不够高时空出来的那几行就是上面说的空档，下一段正文落地时填回去。

**"在跑"用动的东西说，不用字。** 工具调用行首那一格与状态行前那一格，跑着的时候在动，跑完了换成定住的符号。两格**动法不同**（2026-10-15，照 Claude Code）：状态行那一格说的是模型这一摊在动，是一颗来回变形的星（`✢ ✳ ✶ ✻ ✽` 往返八帧，约一秒一圈）；工具行那一格说的是某一次调用在跑，是一个一明一暗的圆点 `●`（亮暗各 480ms）——两处同时在动时一眼分得出哪个是模型、哪个是工具。每帧一列，换帧不会让后面的字左右跳。定住的符号：`✔` 完成 · `✖` 失败 · `?` 结果不明（§8.6 的 uncertain 既不是成功也不是失败，所以它不共用失败那个符号）· `↗` 已转后台（`bash` 跑过阈值交给 Gateway，结果里带 `detached`：事件上是 completed，命令却还在跑，成败看交回来的那条消息，所以也不画 `✔`）· `·` 已计划未开跑。一块静止的"运行中"只在它变的那一刻有信息，之后就分不清是还在跑还是卡住了。**状态行同理，而且一切正常时它干脆不说话**：跑着有转圈、跑完了屏幕上就是那段回答，"空闲" / "运行中" / "已完成" 都是让人读一件他正看着的事；剩下那几个状态照旧写出来（已受理 · 排队中 · 等待中 · 已失败 · 已取消 · 已放弃），它们说的是转圈与回答都说不出的事，"等待中"后面那句"在等谁、等到什么时候"（§8.4）一并留着。

**一次工具调用按工具分开画**（2026-10-14，照 pi）。头一行说这次调用动了什么，跑完了在末尾补耗时（`340ms` / `4.2s` / `2m05s`），按宽度折行、不截在一屏宽：`bash` 是 `$ 命令`，`read` 是 `路径:起-止`，`rg` 是 `模式 · 目录 · glob`，`edit` / `write` 是路径。头一行下面是它带的那段内容：多行命令与 `codemode` 脚本的续行（历史 `python` 调用同样渲染）、`write` 写进去的内容、`edit` 每一处由 `match_text` → `replace_text` 逐行比出来的 diff（没改的长段只留挨着改动的 4 行，只改了一行里的一截就把那一截反白；客户端看不到文件，所以没有行号）。`edits[]` 改了几处就画几段，每段前面一行灰色的 `@@ edits[i]`（与工具报错点名的下标一致，这一处带 `replace_all` 就跟一句"（全部替换）"），头一行路径后面补"（N 处）"；只有一处的 `edits` 与旧的单处参数画得一样，不标 `@@`，旧记录照常渲染；`edits` 编成 JSON 字符串的也拆开画。再下面以 `⎿` 起头的是结果，取 `tool.result` 的 `body.preview`（工具写给模型的那段完整正文，没有就取 `body.error`），不是顶层那份 ≤1 KiB 的预览；旧事件没有 body 才退回后者。`bash` 的结果留尾、其他留头；成功的 `read` / `edit` / `write` 不印结果——前者就是文件本身，后两者头一行已经说了——失败了照印，那是原因。折叠时结果最多 3 行、头一行（折出来的续行）与正文各最多 4 行（2026-10-15 从各 10 行收紧，照 Claude Code：一轮并发五六次调用，每次十几行，一下子就往回滚区倒几十行），超出的说一句还有几行、按 `Ctrl-T` 展开（照 pi 的 `… (N more lines, ctrl+o to expand)`），每行截在一屏宽；**`edit` 的 diff 不折**，改了什么就是这次调用的全部内容。`Ctrl-T` 打开就全给、长行折行，并以 `键: 值` 补上头一行与正文没用到的参数；已经印出去的调用跟着整段重画（见上）。工具输出里的颜色转义、制表符、进度条回车在进终端之前拿掉。工具调用之后的下一块（正文、提示或另一次调用）先空一行，否则它紧贴在结果下面，分不清哪几行是这次的输出；视口里还在动的那几块按同一条规则空行，定型那一帧不往下跳。codemode 的子调用（`codemode.journal`）还不画。

**连着的读与搜收成一行**（2026-10-15，照 Claude Code 的 `Read 3 files, searched for 2 patterns (ctrl+o to expand)`）。同一条 Run 里连着的 `read` / `rg`——中间没有说出来的话（用户消息、带正文的助手消息），也没有别的调用——在回滚区上只占一行：`✔ 读了 3 个文件、搜了 2 次（Ctrl-T 展开）`（文件按路径去重）；跑着时在视口里是 `● 读取 3 个文件、搜索 2 次…`。只有调用、没有正文的几轮接着收进同一组。失败与结果不明的那次不进组，单独一块印出原因。一组要**全部跑完而且不会再长**才交给回滚区——后面已经有了别的块、有了说出来的话、下一段草稿开始了，或者 Run 结束了；否则先交"读了 2 个文件"、下一轮又读一个，就只能再起一行。一组只有一次时照单次调用的样子画。`Ctrl-T` 打开时不收，逐条全给。

状态行**左右两摊**：左边是"此刻在发生什么"（转圈 · 本轮耗时 · 真有事时的状态与理由 · 有人在等回答时的待处理条数），右边贴着右边沿对齐、位置不动的三格——**模型 · 连接 · 这一段对话花掉的 token**（照 pi 的页脚写成 `↑38 ↓7.6k R1.4M W108k CH99.5%`，由 `message.assistant` 每轮报的用量累加，补读历史时一并算上，所以 resume 回来看到的是整段的数。`input_tokens` 是含缓存读写的整个提示，所以 `↑` 是减掉缓存读写之后没走缓存的部分，`R` / `W` 是缓存读 / 写，四个加起来才是模型吃进去的；`CH` 是**最近一轮**的 缓存读 ÷ 整个提示——累计比例会被前面几十轮摊平，看不出刚才那一下缓存断了。`↑↓` 总在；`R` / `W` 是零就不出现，这一段对话从没报过缓存或最近一轮没报时不给 `CH`，未知不写成 0%。2026-10-14）。右边三格位置固定，眼睛不用每次重新找；窄终端上截的是左边那句话的后半段，不动右边。模型名在第一条消息之前就印得出来——网关开机时报过清单，默认是哪个它知道。帧由**时钟**算而不是帧计数器：状态机不读时钟，同一时刻画出来永远是同一帧，测试给一个固定时刻就能断言它转到哪了；交给回滚区的行不给钟，因而不会定格在某一帧随机的样子上。

**状态行上的"已连接"说的是网关可达**，不是"事件流开着"。事件流要有会话才订得了，而裸 `komo` 在第一条消息之前没有会话——启动时问模型清单那一问答上了，就已经证明网关在那儿，没有理由让状态行挂着"连接中"直到人开口。会话铸出来之后，订阅自己的状态接管这一格。

**TUI 是同一个介入接口的第四个界面。** `/pending` 印出的待处理清单列出每一条、它的问题与可答的结论（`verify`：`satisfied` / `not_performed` / `abandon`；`blocked`：`resolve` / `abandon`），用 `/answer <句柄> <结论>` 一次答一条；待处理条数同时出现在状态行上，输入框从不因介入而锁住。2026-10-11 之前这里是一块审批弹窗加一张答案菜单（`r` 本次任务默认通过 / `y` / `n` / `a` 全部批准），随审批删掉。

聊天里的答复是普通的文本命令，走 `Dispatcher::handle`，去重键与普通消息同源（§11.1）；同一条介入被答两次（两条真实消息、两个不同的 `event_id` / `update_id`）由介入答复自己的幂等挡住——第二次得到"已决定"。两层各管一件事：去重键挡平台重投，幂等挡人重复回答，不用组合键去兼职。（审批卡片的按钮回调 `card.action.trigger` / `callback_query` 已经不再产生。）

**飞书卡片是 JSON 2.0（`schema: "2.0"`），而 2.0 去掉了 `note` 组件与 `action` 模块。** 官方的不兼容变更写明：2.0 不再支持 note 与 action（`tag` 为 `action`），且 2.0 对不认识的组件是**整张卡打回**而不是忽略。所以"卡片备注"（原因、有效期、结论那一行）是普通文本组件加 `notation` 字号与灰色；一行按钮是 `column_set`，每列一个 button，那一块带固定的 `element_id`——决定之后那张无按钮的卡靠它整块摘掉。踩过的坑在 §14：带 `note` 的卡片被平台拒（`230099 / 200861 unsupported tag note`），于是当时的审批请求**一条都到不了聊天里**，而失败只落在网关日志的一行 WARN 上。今天发卡片的只剩 Run 回复卡片（下文），这条约束照旧。

**决定后的原地更新用 `PATCH /open-apis/im/v1/messages/{message_id}`，不用回调响应体。** 飞书官方给了三条路：回调响应里直接回传新卡片（须在 3 秒内）、用回调 `token` 延时更新（30 分钟内、最多 2 次、且必须在响应回调之后）、以及无条件 PATCH（仅 `interactive` 消息、仅 14 天内发送的消息、单条 5 QPS、更新前后 `config` 均须 `update_multi:true`）。komo 取第三条：结果先落 Ledger 再回写界面（当初是审批的决定，今天是 Run 回复卡片的终态），这件事跨越了回调的 3 秒预算；而且 PATCH 是普通 REST 调用，与「ws 长连接还是 HTTP 回调」无关，不必赌 ws 客户端能不能回传响应帧。回调本身只需尽快返回，必要时带一个 `toast`。Telegram 侧对应 `editMessageReplyMarkup` 去掉按钮：官方的 48 小时编辑限制只约束「非机器人自己发送且不含 inline keyboard 的 business message」，机器人自己发的卡片不受时限；编辑失败按**非致命**处理——决定已在 Ledger 里，界面回写失败不改变结论，也不要去匹配错误文案（官方明示 `error_code` 内容将来会变）。

**飞书的 Run 回复卡片（2026-09-25 实现，未真机验证）。** 一条起了 Run 的飞书消息（Dispatcher 回 `Queued`）：渠道先给原消息加一个 `Get` 表情（`POST /open-apis/im/v1/messages/{message_id}/reactions`），再**回复原消息**发一张"处理中"卡片（`POST …/{message_id}/reply`）；Run 终态（完成 / 失败 / 取消 / 放弃）时把这张卡片 PATCH 成结果——正文放在 markdown 组件里，不再另发一条文本。命令回执（`Replied`）只加表情、回执照旧是文本；`Duplicate` / `Rejected` / `Ignored` 与卡片回调什么都不加。原消息的 `message_id` 只留在飞书渠道里，不进三个渠道共用的 `InboundMessage`。

- **终态投递统一用 `Outbound::RunFinished`**（不是 `Text`）：聊天来源的 Run 因此与 Cron 一样落 `deliveries` 的 `RunFinished` 行，§8.4 第 10 行"结果没送达 → 补发"对它们同样成立。Telegram / WeChat 对它与 `Text` 渲染相同。
- **卡片在哪是进程内小账**，与当初的审批卡片同一种性质：按 `(run, chat_id)` 记，有上限，是缓存不是权威。查不到（重启后账没了、Cron 投 home 的 `RunFinished`、卡片没发出去）就发文本；PATCH 失败也退化成发文本——那一次必须送到，`deliveries` 那一行靠它结算。表情或卡片失败都只记 WARN，不影响后续。
- **正文过长**：卡片只放前 3000 个字符（飞书卡片请求体上限 30 KB，PATCH 时卡片 JSON 还要再转义一层，取一个保守值）并注明"其余见下方消息"，余下的按普通文本分段发。
- **竞态**：终态投递来自 Run 的看客任务，与发卡片并发，可能先到。所以拿到 `Queued` 后、任何网络调用之前先同步登记一个 pending 槽位，卡片发出去后填上 `message_id`（或标记失败）；终态遇到 pending 槽位时最多等 5 秒。等不到就把槽位标成"已放弃"改发文本，卡片之后才到的由发卡片那一侧自己把它收尾成"结果见下方消息"——两边在同一把锁里比较并交换，谁先到都只有一方认领那张卡片，不会出现"文本发了、卡片停在处理中"。剩下的窗口只有"Dispatcher 返回 `Queued` 到登记槽位"之间（同步代码、没有 await），Run 要在那里面跑完一整轮才会漏。

**飞书群里的话题（2026-10-22；私聊已真机验证，群未验证）。** 群里 @机器人 与私聊里的每条消息一律以话题回复，**一个话题一个 Session**（做法同 OpenClaw 飞书渠道的 `group_topic` 会话范围与 `replyInThread`）。话题按根消息 id（`om_*`）认：消息带 `thread_id` 且有 `root_id`（已在话题里，飞书文档：话题内消息的 `root_id` 与 `parent_id` 都是话题根）就用 `root_id`；否则这条消息自己就是新话题的根——主流里的 @ 和主流里的引用回复都从当前这条开新话题，不把话题挂到被引用的老消息上。peer 写成 `feishu:{chat_id}#{om_root}`（`ChannelPeer.thread`），`runs.peer`、`sessions.origin`、`deliveries.thread` 都带着它，所以这个 Session 的一切出站——处理中卡片、终态的跟随分段与文本退路、命令回执、介入、后台任务与监听的通知、重启后补发——都进同一个话题。飞书发消息接口不能按 `thread_id` 发（`receive_id_type` 没有这一项），进话题只能回复话题里的消息：komo 一律回复话题根并带 `reply_in_thread: true`，处理中卡片回复触发的那条（主流里它就是根，带上这个参数就开出话题）。`/id` 与拒绝提示回显的会话不带话题，它是抄进 `home_chat` 的串。话题里没 @机器人 的消息照旧不响应（私聊不看 @）。**飞书私聊因此不再进 home session**：Dispatcher 对带话题的私聊走 `session_for_peer`，没有话题的私聊（Telegram、WeChat、TUI）照旧进 home；home 的主动投递（`home_chat`、后台任务通知）仍发到私聊主线。实测（2026-10-22，`home_chat` 那个 `chat_mode=p2p` 的会话）：回复根消息带 `reply_in_thread: true` 返回 `thread_id`，再回复同一根消息落在同一个 `thread_id`——私聊支持话题。`/cancel`、`/status`、`/new` 作用于所在话题的 Session。此前的整群 Session 与飞书私聊在 home 里的历史不再收到新消息，不迁移。平台对不支持话题回复的群报 `230071`，没有样本，不做退回主流的兜底。

**群里的访客（2026-10-22，未真机验证）。** 群里不在 `allow_from` 的人 @机器人，渠道配了 `guest_agent` 才答，否则照旧回拒绝提示；私聊里的非操作者一律拒绝；非操作者的斜杠命令（含 `/answer`）一律拒绝，`/answer` 自己另有一道操作者检查。komo 没有审批、`bash` 拦不住，所以访客是**无工具的纯对话**，而且隔离按会话类型强制、不靠配置：
- **会话**：`kind = guest`、`agent_id` = 访客 Agent，来源 `guest:{peer}`（飞书 `guest:feishu:{chat}#{om_root}`，Telegram 没有话题就是整群一个）。与操作者在同一话题里的 `chat:` 会话互不相见；回复仍落在同一个话题里。`guest_agent` 改了，下一条另开新会话。
- **能力面恒为空**（`freeze_run` 按 kind，Profile 写了什么都不算）：`read` / `rg` 没有根目录限制，MCP 工具用的是操作者的凭据，现有的工具没有一样能交给陌生人——这是按 kind 强制的唯一一条，其余都由 Profile 表达。配置校验同样要求访客 Agent 显式写 `tools = []`（不写 = 整个目录，写了任何工具都报错），WeChat 上配 `guest_agent` 报错（只有私聊）。
- **记忆与项目文件按 Profile**（§6.1 记忆模式）：校验要求访客 Agent 写 `memory = "private"` 或 `"off"`、`project_files = false`——`shared` 会召回操作者的 Personal，项目文件的发现会读到 `~/AGENTS.md`。`private` 让访客记住群里常问的事：作用域 `agent:<id>`，整个访客 Profile 一份（所有群、所有访客共用），操作者的 Agent 看不见它，它也看不见操作者的。访客会话的 Profile 从配置里删掉时，这个会话的下一条消息被拒，不退回默认 Agent。skills 目录只在有 `read` / `bash` 时出现，访客没有。
- **没堵上的**：系统提示固定的「工作目录：…」一行带着操作者的绝对路径；访客用操作者的模型额度、不限流——要省钱就给访客 Agent 配便宜的 `model`。

一条普通消息在 Run 停在介入上时到达：Run 不会被越过（§6），消息排在它后面；不做"插话替换答复"这类特殊路径。聊天渠道的消息也不插话（插话只在 TUI，且只并进**正在跑**的 Run，§6）：渠道的回复卡片与"Run 结束时回复"按 Run 记账，一条消息被并走就没有自己的那次回复了。

### 11.4 主动投递

§8.8："应为待发送结果持久保存投递记录，不能为补发一条结果消息重跑任务。"

- `Notifier::deliver` 先在 `deliveries` 表写一行（目标、内容引用、状态 `pending`），再发送，成功后标 `sent`。重启后 `pending` 的行补发，按 `DeliveryId` 幂等。
- **补发在就绪之后跑，而且同一时刻只跑一趟。** 就绪（§3 第 4 步）不等它：它是网络 I/O，一条一个平台往返，积压多少就等多久。三个入口——渠道起来时按平台冲刷、启动时整体冲刷、微信入站前按会话冲刷——共用一把锁：两趟并发地在同一批 `pending` 行上跑，同一行会被送两次（发送在结算之前）。
- **待处理 Intervention（§7.5）的投递目标**：Run 的来源会话，**加上** home chat（若不同）。两处都能回答，第二个答复得到"已决定"。来源是 Cron 或已断开的 TUI 时只有 home chat。结果不明、阻塞两类共用这一条——"需要人判断"不该因为种类不同而有不同的到达率。
  补充一条（2026-09-19 实现并按线上反馈收窄）：**屏幕前有人在看这个会话时（HTTP 的 SSE 订阅在）不投 home chat**。TUI / HTTP 来源的 Run 没有 chat 对端，照上面那句它只剩 home chat 一个出口，而那条卡片是一次网络往返（实测几秒到几十秒），人正盯着屏幕的时候它只会晚到、答完还多一条回写。判据是**客户端订阅数**（`EventHub::watch` 的守卫，HTTP SSE 处理器持有），不是广播的订阅者总数——Run 的看客自己也订阅同一个广播。人走掉之后仍挂着的介入由周期（与审计补写同一拍）补投，投过的不重复：§10 的"不能因为无人值守就没人知道它在等"没有被这条放松。
- home chat 解析：只看配置——每个 enabled 渠道的 `home_chat`（§11.2），没有运行时覆盖；一个都没配时返回错误给调用方，**不静默丢弃**。多个渠道都配了时的默认顺序是**飞书 > Telegram > WeChat**：前两者能通过 API 对任意已加入的会话主动推送，微信不能（下一条）。
- **WeChat 的平台约束**：DM 回推依赖回复令牌 `context_token`——每条入站消息自带一个，SDK 按 `user_id` 存在**进程内存**里，源码里没有过期逻辑，唯一的失效路径是会话过期（`errcode -14`）清空整张表；真正挡住主动推送的是**进程重启即全丢**，不是令牌到期。没有令牌时 `send` 直接返回 `NoContext`，不会联网去补——这就是 `Deferred` 的精确触发条件（按错误**变体**匹配，不按字符串）。因此进程启动后用户没发过消息时**无法主动推送**。对应处理：该渠道的 `deliver` 在没有令牌时把行留在 `pending` 并返回 `Deferred`；用户下一条消息到达时 Dispatcher 先冲刷该会话的 `pending` 投递（§11.1 第 4 步），再处理新消息。介入通知因此不会丢，只会晚到；`home_chat` 里排在它前面的飞书或 Telegram 会先送到。待验证：服务端是否接受跨进程的旧 `context_token`——若接受，把每个 `user_id` 的最新令牌持久化到 state.db 即可让重启后的主动推送直接成功，`Deferred` 退化为「这台机器从没收过这个人的消息」一种情形。另：SDK 的 `message_type` / `message_state` 枚举没有 unknown 兜底，服务端多一个值就整批反序列化失败并无限退避重试，渠道会静默卡死——komo 的 wechat 渠道把连续 N 次 JSON 错误升级为 home chat 告警。
- TUI 的 SSE 是另一个 Notifier 实现，不持久化——连接断了从事件流补读。

### 11.5 渠道实现的约束

每个渠道一个 **feature 门控的模块**，SDK 依赖只在该 feature 下进入构建；渠道模块只能依赖 kernel 的类型和 `Inbound` / `Notifier` 两个 trait（§13.5），不能碰 runtime。`cargo tree -d` 有新重复版本时必须在 PR 里按渠道归因。`komo channel list | probe` 对每个已配置渠道做一次连通性核对（飞书拿 tenant token、Telegram `getMe`、微信检查凭证文件），`komo doctor` 汇总。

## 12. 数据目录与记录习惯

```text
~/.komo/
├── config.toml          # 行为配置：模型、渠道的 allow_from / home_chat / guest_agent 等
├── .env                 # 凭证：飞书 app secret、Telegram bot token、模型 key
├── state.db             # Turso；调度状态、投递记录、Cron、Memory 与内容索引
├── sessions/
│   └── <session-id>/
│       ├── events.jsonl # 调用、状态、输出引用、简短预览与消息
│       ├── payloads/    # 大参数、大模型回复和大计划正文
│       ├── tool-output/
│       │   └── <run-id>/<call-id>/<attempt-id>/
│       │       ├── output.json # 完整结果或错误正文、执行身份、文件引用
│       │       ├── stdout.txt  # 按需保存
│       │       └── stderr.txt  # 按需保存
│       └── artifacts/  # 当前会话的报告、脚本快照和生成文件
├── runtime/             # 发现文件、锁等
├── skills/              # 人写的 SKILL.md（§5.6）
├── wechat/              # 微信登录凭证
├── workspaces/          # 项目副本和普通工作文件
└── logs/                # Gateway 自身的运行日志
```

Session 内容以目录为单位管理和归档，不再使用顶层 tool-output 或 artifacts 分散存放会话数据。自动 Memory 保存在 state.db，用户主动记录保存在 Memos；可复用能力保存在 skills，实际项目副本保存在 workspaces。会话 artifacts 中需要长期保留的报告和文件，不因清理聊天历史而自动删除；不能直接删除整个 Session 目录代替引用检查。**会话的删除只有一条入口**：`komo session delete` 进 `closing`、对账判定后进 `deleted`、`komo session purge` 才真的删内容（§8.10）——`rm -rf sessions/{id}` 不在流程里，它今天造成的正是"数据库说有、内容说没有"的那种对不上（§8.9）。会话来源需要清理时，先为仍引用它的记忆保留必要的、经脱敏的证据摘录与来源元信息，或停用无法再验证的条目，不能留下伪装有效的引用。重建内容索引不删除 JSONL；清理已结束 Session 的 JSONL 需要同时处理索引、检查点和引用关系。

记忆正文、来源和确认记录需要持久保留；关键词与向量索引可从原文重建，禁止为了重建索引删除整个数据库。Memos 原文由 Memos 自己的备份策略覆盖。

数据目录可通过配置或 KOMO_HOME 改变。配置项中的相对路径按配置文件所在目录解析；JSONL、payloads 与 output.json 中的正文 / 输出引用统一按对应 Session 目录解析。每个实例独占自己的数据库；Turso 对 db 文件持进程独占锁，文件放在运行机器的本地磁盘，不能作为多机器共享数据库。

备份在明确的一致性水位上取得数据库快照，以及各 Session 目录内的 JSONL 已同步前缀、payloads、tool-output 和被引用产物。可短暂暂停持久化协调入口来建立水位，再按固定长度复制追加文件；不能只备份 state.db，也不能把不同时间点的 JSONL 和数据库随意拼接。state.db 的调度与投递数据不能单靠会话 JSONL 重建；渠道的身份匹配（`allow_from` / `home_chat`）不在 state.db，备份 config.toml 与 .env 即可。

## 13. 通信、技术栈与项目结构

### 13.1 通信

CLI 通过 HTTP 发命令，通过 SSE 观察运行。Gateway 内部采用函数调用，无需在本机模块之间再发 HTTP。

最小接口：

| 接口                             | 行为                                           |
| -------------------------------- | ---------------------------------------------- |
| GET /healthz                     | 最小健康检查与实例标识                         |
| POST /v1/sessions                | 创建会话                                       |
| GET /v1/sessions                 | 列出会话（默认不含已逻辑删除的，`?all=1` 才列） |
| GET /v1/sessions/{id}            | 会话详情与运行状态                             |
| GET /v1/sessions/{id}/events     | 按游标获取或订阅事件                           |
| POST /v1/sessions/{id}/runs      | 提交新输入；会话不在 `active` 时 409 并说清是 `closing` / `deleted` / `purged` |
| POST /v1/sessions/{id}/resume    | 检查恢复位置，恢复可继续的运行或返回待处理状态 |
| POST /v1/sessions/{id}/boundary  | 追加 `conversation.boundary`（聊天里的 `/new`）  |
| POST /v1/sessions/{id}/delete    | 逻辑删除：进 `closing`（`{"now":true}` 立刻把未完成 Run 各写一条取消并进 `deleted`）。**不碰内容**（§8.10） |
| POST /v1/sessions/{id}/purge     | 回收内容进 `purged`；引用检查不过则 409 并列出要先处置什么（§8.10） |
| POST /v1/reconcile               | 立刻跑一次对账（§8.9）；幂等，返回这次判定了什么 |
| GET /v1/runs/{id}                | 执行详情                                       |
| POST /v1/runs/{id}/cancel        | 取消                                           |
| GET /v1/interventions            | 待处理清单：结果不明、阻塞两类（§7.5）。每条带句柄、种类、问题与可答的结论 |
| GET /v1/interventions/{handle}   | 单项详情：问题、理由、那次调用的计划与可答的结论 |
| POST /v1/interventions/{handle}/answer | 答复：`satisfied` / `not_performed` / `abandon` / `resolve`；已答复的返回原结论，不报错 |
| GET /v1/cron                     | 列出定时任务                                   |
| POST /v1/cron                    | 创建定时任务                                   |
| POST /v1/cron/{id}/run           | 手动触发                                       |
| PATCH /v1/cron/{id}              | 更新定义或启停                                 |
| DELETE /v1/cron/{id}             | 移除后续调度，已有执行历史保留                 |
| GET /v1/memories                 | 按作用域、状态和查询条件列出记忆               |
| POST /v1/memories                | 手动记一条（§9.3）；拒绝后台任务会话           |
| GET /v1/memories/{id}            | 内容、版本、证据与确认记录                     |
| POST /v1/memories/{id}/confirm   | 操作者确认指定 revision                        |
| POST /v1/memories/{id}/forget    | 停用指定 revision 并失效索引                   |
| GET /v1/memory-index             | 当前空间、进度、覆盖率和错误                   |
| POST /v1/memory-index/rebuild    | 幂等提交重建任务                               |
| GET /v1/models                   | completion alias 菜单：显示名、上游 model、provider、协议、上下文与 effort 档位 |
| GET /v1/config/check             | 当前配置的校验结果（与热重载同一个函数）       |
| POST /v1/config/reload           | 热重载；校验不过 422 并保留原配置               |
| POST /v1/mcp/reconnect           | 点名的 MCP 服务器（空 = 全部）断开重连，等握手完交回各自状态；名字不在配置里 400（`docs/mcp.md` §2.5） |
| GET /v1/home-session             | 操作者的 home session（§11.2）                 |

除最小健康检查外统一认证。提交输入、介入答复、Cron 与 Memory 变更都支持幂等请求键；同一键对应不同内容则拒绝。

聊天渠道不经 HTTP：它们在 Gateway 进程内通过 `Inbound` / `Notifier`（§13.5）调用与这些接口相同的函数，Dispatcher 是两边共用的入口（§11.1）。

SSE 事件带 Session 内递增序号，断线后按游标补读。JSONL 事件是内容补读来源，state.db 索引负责定位，内存通知仅提示有新数据。CLI 退出或连接中断不取消后台运行，取消通过明确操作发起。

### 13.2 技术选择

| 用途                  | 选择                                                                     |
| --------------------- | ------------------------------------------------------------------------ |
| 异步与进程 IO         | Tokio、tokio-util                                                        |
| HTTP / SSE            | Axum                                                                     |
| HTTP 客户端与模型调用 | Reqwest                                                                  |
| CLI                   | Clap                                                                     |
| 状态数据库            | Turso（MVCC，关 `fts`）+ toasty；raw SQL 走 `toasty::sql`，限于 schema / 关键词索引 / 领取三处（§8.2） |
| 对话与执行记录        | Session JSONL，由 Serde 完整序列化；旧外置正文仍可读取                               |
| 工具完整输出          | 按执行尝试保存 output.json / stdout.txt / stderr.txt，流式写入后持久发布 |
| 关键词检索            | state.db 的 `memory_terms` 列：索引时 CJK bigram + ASCII 词分词，查询 `instr`，IDF 加权（§9.4） |
| 向量存储与检索        | state.db 保存向量（f32 BLOB + 维度 + 代次），Rust 在过滤后执行精确余弦检索               |
| 向量生成              | 独立 EmbeddingClient，使用 memory.embedding 配置                         |
| 配置与结构化数据      | TOML、Serde；进程内唯一 `Arc<ConfigSnapshot>` 用 arc-swap 原子替换，mtime 轮询触发热重载（§3） |
| Schema 与异步工具接口 | Schemars、async-trait                                                    |
| 日志                  | tracing                                                                  |
| Cron                  | 支持指定五字段语义及时区的 Rust 调度库，落地时做语义验证                 |
| TUI                   | ratatui + crossterm（不进备用屏：视口只占底下几行，正文交给终端回滚区），Markdown 用 pulldown-cmark；仅 komo-client |
| 飞书                  | openlark，仅 `websocket` 特性；事件自己解析，回复走 reqwest（feature `feishu`）           |
| Telegram              | reqwest 手写 Bot API 长轮询，无 SDK                                                      |
| WeChat                | wechatbot（iLink，DM）；feature `wechat`                                                 |
| TLS                   | reqwest `rustls-no-provider` + rustls `ring`，启动时安装 provider；不引入 aws-lc          |

Axum 已提供 SSE 响应；toasty 的 turso 驱动提供连接与事务，MVCC 冲突重试由 komo 自己包（§8.2）。实现时锁定实际依赖版本并验证 Fedora/macOS 构建。[Axum SSE](https://docs.rs/axum/latest/axum/response/sse/index.html) · [toasty](https://docs.rs/toasty)

生成模型有三个独立协议适配器：OpenAI Responses API（`api_backend = "responses"`）、Chat Completions（`api_backend = "chat_completions"`）与 Anthropic Messages API（`api_backend = "anthropic_messages"`）。三者各自处理请求形状、流式终态、tool call 拼接和原生历史回放；切换协议时只用标准化的文字与 tool call 历史，不把一种协议的私有块直接发给另一种协议——Anthropic 的 `thinking` / `redacted_thinking` 块和 Responses 的 `reasoning.encrypted_content` 一样，只在同协议内原样回放，跨协议历史一律从文字与调用记录重建。Anthropic 一次请求 `POST {base_url}/messages`，鉴权是 `x-api-key` 请求头（不是 Bearer），`max_tokens` 必填；工具调用是 assistant 内容里的 `tool_use` 块，结果放进**下一条 user 消息**的 `tool_result` 块，同一轮多个结果并进同一条 user 消息；协议要求 user / assistant 严格交替，相邻同角色消息（例如两个历史 Run 之间缺了中间那句答复）合并成一条。向量协议是 OpenAI-compatible `/embeddings`（`embeddings`）与 Ollama `/api/embed`（`ollama_embeddings`）两种。`model_provider` 只表示一组可继承的连接默认值，`api_backend` 才决定适配器；因此 OpenAI、OpenRouter、Anthropic 官方端点与自建网关可以复用协议实现，但不会被误认为同一个供应商。

### 13.3 模型角色与统一 effort 配置

模型先进入统一目录，再由角色引用：

| 配置                    | 职责 |
| ----------------------- | ---- |
| `model_providers.<name>` | 一组可复用的 `base_url`、`api_backend`、`env_key` 与超时默认值 |
| `model.<alias>`          | 一项模型；`type = "completion"` 或 `"embedding"`，可覆盖 provider 的任一连接字段 |
| `auth`                   | 凭证不走 `.env` 变量，走另一种机制；目前只有 `"chatgpt"`（ChatGPT 账号 OAuth，给 Codex 模型用）。与 `env_key` 互斥，只允许 `api_backend = "responses"`（下方单独一段） |
| `models.default`         | 默认对话模型 alias；Session / Cron 的 `model` 参数也使用 alias |
| `memory.model`           | 记忆提取模型 alias；省略时使用 `models.default` |
| `memory.embedding`       | 向量模型 alias；hybrid / vector 检索时必填 |

解析优先级固定为“模型项 > model provider > 非敏感默认值”。进入 `ConfigSnapshot` 前，每个 alias 已经展开成完整配置；运行时、Cron 持久化和热重载不再临时拼接字段。completion 项的 `context_window` 也在这时并进它那份模型配置，随 `run.accepted` 的快照冻结——每个 Run 用它开始时的窗口，热重载改窗口只影响新 Run。**窗口的来源（2026-10-15）：配置写了用配置；没写就问 provider 的 `/models`**（与真实请求同一份凭证；条目按 `id` / `slug` 精确匹配 `model`，取 `max_input_tokens`，没有再取 `context_window`、`context_length`——优先输入上限，因为 `window_reserve_tokens` 只预留了一小段，容不下 128k 的输出上限）；**都没有就是未知**，窗口保护只按 `[compaction] max_context_tokens` 算。窗口归 (provider, model) 而不归模型名：同一个 Claude 走 magpie 是 1M，走别的代理未必，所以不内置"模型名 → 窗口"表。探测在 Gateway 启动、模型配置重载、受理 Run 时发现缺项或上次失败时后台进行，结果只在内存（按 `(base_url, model)` 缓存），从不挡就绪路径；受理时把查到的值并进这条 Run 的模型快照，随 `run.accepted` 冻结——探测还没回来时受理的 Run 就是未知，回放照旧逐字节相同。`komo config check` 列出每个 alias 的窗口与来源（`config` / `provider` / `unknown`），未知的提示去配置里写上。`env_key` 是凭证所在的环境变量名（兼容旧拼法 `api_key_env`），凭证值仍只存在 `.env` / 进程环境中。

以下是配置模板，模型名称与服务地址为占位值，effort 示例需要替换为目标模型支持的值：

```toml
[model_providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_backend = "chat_completions"
env_key = "OPENROUTER_API_KEY"
timeout_secs = 120

[model.union]
type = "completion"
model_provider = "openrouter"
model = "stealth/union-alpha"
name = "union-alpha"
# 省略 = 问 provider 的 /models；provider 不报（比如只回 id 的代理）就得写上。
context_window = 200000
effort = "medium"

[model.memory]
type = "completion"
model_provider = "openrouter"
model = "openai/gpt-5.4-mini"
# 单项可覆盖 provider 默认值：
api_backend = "responses"
effort = "low"

[model.embedding]
type = "embedding"
api_backend = "ollama_embeddings"
base_url = "https://embedding.example.com/v1"
model = "YOUR_EMBEDDING_MODEL"
env_key = "OLLAMA_API_KEY"
# dimensions 省略时使用模型返回维度，校验后固定到索引代次。
# 这一次探测在 Gateway 就绪**之后**的后台跑（§3），不压住就绪；落定前向量臂按 §9.4 降级。
# revision 可用于标识服务端同名模型的权重版本。

[models]
default = "union"

[memory]
enabled = true
model = "memory"
embedding = "embedding"

[memory.retrieval]
mode = "hybrid"
candidate_limit = 40
top_k = 8
max_tokens = 1500

[execution]
# 交给模型的工具结果正文上限（字节）。完整输出在 sessions/<id>/tool-output/ 下；这里
# 只限模型视图：bash（旧记录里的 shell 同）超限留尾，其他工具留头，另有固定 2000 行上限，并给出可
# read 的 artifact:// 引用（§8.3、§4.7）。改动只影响之后受理、冻结新 RunSnapshot 的 Run。
# 省略 = 51200。
model_result_bytes = 51200
# 单次工具调用的活动执行时限（秒）。bash 的 timeout_secs 会被收紧到它以内：到点是工具自己
# 杀掉进程组、报超时（可以再发）；只有工具不守时限时执行器才兜底，那一刻报的是"结果不明"。
# 热生效，对下一轮调用立即生效。省略 = 1800。
call_timeout_secs = 1800
# 对话会话里的 bash 跑过这么久还没完，就自动转到后台：这次调用先返回「已转后台 #id」，跑完时
# 退出码与输出尾部作为一条消息交回会话（docs/monitor.md §8）。任务会话与 Cron 的 Run 不转。
# 热生效。0 = 不转。省略 = 120。
detach_after_secs = 120
# 投影超过这么多字节的成功结果才有资格换成稳定的短视图（抬头 + 首尾整行 + 完整输出引用，§8.3）。
# 有资格不等于换：换不换由缓存账决定（缓存已冷 / 贴着窗口，§6），一次换一批。0 = 不衰减。省略 = 4096。
decay_threshold_bytes = 4096
# 资格之一：至少完整给过模型几次之后才可能换短视图。至少 1。省略 = 2。
decay_full_sends = 2
# 短视图开头最多留多少字节的整行。省略 = 2048。
decay_head_bytes = 2048
# 短视图结尾最多留多少字节的整行；head + tail 要小于阈值。省略 = 1536。
decay_tail_bytes = 1536

[compaction]
# 在线压缩（§6）。默认关：真实会话上核对过触发频率与省下的量再开（§14）。每次判断读当前快照。
# 只管自动判断：provider 以上下文超长拒了请求时的溢出恢复（强制压一次）不受它管，关着也照做。
enabled = false
# 缓存写与读的价格比（Anthropic 1.25 / 0.1）。省略 = 12.5。
cache_write_read_ratio = 12.5
# 切点之后至少原样留这么多 token 的近期轮次。省略 = 20000。
keep_recent_tokens = 20000
# 离 model.<alias>.context_window 还剩这么多 token 时不看账，直接压。省略 = 16384。
window_reserve_tokens = 16384
# 上下文到这么多 token 也算贴着窗口（与模型窗口 − 预留取先到的）。省略 = 150000。
max_context_tokens = 150000
# 第一次压缩把剩余请求估计放大的倍数。省略 = 2.0。
first_compaction_scale = 2.0
# 之后的压缩要求 回本请求数 × margin ≤ 剩余请求。省略 = 1.5。
subsequent_margin = 1.5
# 上次压缩之后至少过这么多次请求才按经济账再压（窗口保护不受它限）。省略 = 2。
cooldown_requests = 2
# 摘要本身的 token 估计。省略 = 1000。
memo_token_estimate = 1000
```

`memory.model` 省略时使用默认 completion alias 的完整配置，包括 effort；它不继承某个 Session 或 Cron 的临时覆盖。显式引用时必须指向 completion，`memory.embedding` 必须指向 embedding。若明确选择 keyword 模式，可以不配置 embedding。`GET /v1/models` 只列 completion alias；选择 alias 会携带完整端点、协议和凭证引用，而不是只替换上游 model id。

**`auth = "chatgpt"`：用 ChatGPT 账号 OAuth 调 Codex 模型，而不是 `.env` 里的一个 key。**

```toml
[model_providers.codex]
base_url = "https://chatgpt.com/backend-api/codex"
api_backend = "responses"
auth = "chatgpt"

[model.codex]
type = "completion"
model_provider = "codex"
model = "gpt-5-codex"
```

- Codex 走的协议就是 OpenAI Responses API 的 SSE 流，复用同一个 Responses 适配器（`store: false` + `include: ["reasoning.encrypted_content"]`、原样回放 `provider_blocks`）——不是一个新协议，只是凭证来源不同。请求体不带 `max_output_tokens` / `temperature` / `top_p` / `stop` / `prompt_cache_retention`。
- Responses 适配器（API key 与 Codex 两条路）的请求体都带 `prompt_cache_key: <当前 Session id>`（64 字符封顶），照 pi：同一个键的请求路由到同一批缓存机器。起因（2026-10-14）：不带时同一会话里 GPT 系模型的 `cache_read` 常落后一轮甚至归零，`codex/gpt-6.1-sol` 命中 87%、`gpt-6-sol` 81%，同期 Claude 93.5%。
- 请求头额外带 `Authorization: Bearer <access_token>`、`ChatGPT-Account-ID: <JWT 里的 chatgpt_account_id>`、`originator: komo`、`session_id: <当前 Session id>`。
- 凭证文件是 `<数据目录>/codex/auth.json`（权限 0600，原子写），**不是** `~/.codex/auth.json`——那是 Codex CLI 自己的文件，`refresh_token` 单次有效会轮换，写它一次就可能把 Codex CLI 挤下线。
- `komo auth codex login`：设备码登录，终端显示配对码与网址，轮询直到确认或超时（约 10 分钟）；不经 Gateway，直接落盘（同 `komo channel wechat login` 的先例）。`komo auth codex status` 给账号、邮箱、套餐与过期时间，不打印 token。
- 刷新：每次请求前取 token，快过期（剩余 < 5 分钟）时刷新并写回，新的 `refresh_token` 会轮换；429 是额度问题，不是登录失效；400/401（`invalid_grant` / `refresh_token_reused` 一类）需要重新登录，报错里指名 `komo auth codex login`。
- `komo config check`：配了 `auth = "chatgpt"` 但凭证文件不存在 → 报错并指出该跑的命令；`auth` 与 `env_key` 同时配、或 `api_backend` 不是 `"responses"` → 报错。

**`api_backend = "anthropic_messages"`：Anthropic 官方 Messages API 或兼容它的中转。**

```toml
[model_providers.anthropic]
base_url = "https://api.anthropic.com/v1"
api_backend = "anthropic_messages"
env_key = "ANTHROPIC_API_KEY"

[model.claude]
type = "completion"
model_provider = "anthropic"
model = "claude-sonnet-4-6"
effort = "medium"
```

- 鉴权是 `x-api-key` 请求头（协议原生鉴权），不是 `Authorization: Bearer`；`auth = "chatgpt"` 只支持 Responses，`anthropic_messages` 配了 `auth` 一律拒绝，和 Chat Completions 的口径一样。
- effort → thinking 的映射是一张固定表，`budget_tokens` 之外再留一段响应预算（32000 tokens，要装得下 `write` 一整份文件的工具参数；按实际输出计费，给大不多花钱）给最终答案与工具调用 JSON，凑成必须发送的 `max_tokens`（协议要求 `max_tokens` 大于 `budget_tokens`）：

  | effort | `thinking` | `budget_tokens` | `max_tokens` |
  | --- | --- | --- | --- |
  | 未配置 / `none` | 不发送这个字段 | — | 32000 |
  | `low` | `{"type":"enabled"}` | 4096 | 36096 |
  | `medium` | `{"type":"enabled"}` | 10000 | 42000 |
  | `high` | `{"type":"enabled"}` | 24000 | 56000 |
  | `max` | `{"type":"enabled"}` | 32000 | 64000 |

  开着 thinking 时不发自定义 `temperature`（本来就不发）。同一次请求可能没有 `thinking` 块（模型判断不需要）——这不是错误，正常收尾。
- **回放**：assistant 这一轮的全部 content 块（`text` / `tool_use` / `thinking` / `redacted_thinking`）原样存进 `Round::provider_blocks`，下一轮原样放回同一条 assistant 消息；`thinking` 块的签名有两种到达形态都要接：中转在 `content_block_start` 里就给出完整签名，官方 API 则留空、末尾用 `signature_delta` 补上。没有存过块的历史（跨协议来的，或 §8.3 只发布用户正文与最终答复的历史 Run）从文字与调用记录重建，重建出的块里不会有 `thinking`。
- **提示缓存**：`system` 最后一块、`tools` 最后一个、当前请求最后一条 user 消息的最后一块各打一个 `cache_control: {"type":"ephemeral"}`（3 个断点，协议上限 4 个）；只加在**这一次要发**的请求体副本上，`self.messages` 自己保存的历史不带这个标记，避免它被当成正文的一部分存进 `provider_blocks`。

effort 的行为统一，取值按协议和具体模型校验：

- 未配置：不发送 effort 字段，由服务端采用默认行为；不等同于显式 none。
- 显式配置：适配器确认目标模型支持后转换为该协议参数。low / medium / high / max 等值不是所有模型共享的固定集合。
- 不支持：komo config check 和配置启用流程返回具体错误，指出模型、配置位置及支持值；不静默忽略或强行映射为另一档。
- 能力未知：需要显式能力声明或在实际启用前完成最小探测。无法确定时拒绝该显式参数；运行中收到不支持参数的响应，也不能删掉 effort 自动重试。
- embedding：配置结构保留可选 effort，但普通向量接口没有该参数时必须省略；dimensions、批大小和输入长度是另一组控制，不用 effort 冒充。
- 后续新增辅助模型、重排模型等角色，也复用同一配置解析与校验入口，不另藏固定 model 或 effort。

不同模型的 effort 可用档位确实不同；例如 Claude 的官方文档按模型列出支持档位，Ollama embed 的请求则列出输入、维度等参数，没有通用 effort 字段。[Claude effort](https://platform.claude.com/docs/en/build-with-claude/effort) · [Ollama embed](https://docs.ollama.com/api/embed)

配置热重载（§3）：模型 / effort / 凭证改动对**新** Run 立即生效；当前 Run 与后台记忆任务保留各自开始时抓取的配置快照，跑完才换。resume 默认沿用原运行的模型 / effort；若操作者明确切换，记录配置变更事件并重新验证协议历史可回放性。任何情况下都不能通过重放已完成工具来适应新模型。

每次生成与向量请求记录角色、模型身份、实际 effort 或 provider_default、耗时和错误；不记录密钥。每轮用量随 `message.assistant` 落盘：`input_tokens` 是**整个提示**（含缓存读写的部分——Anthropic 报三个互斥的数，适配器加总；OpenAI 两种协议本来就是总数），`cache_read_tokens` / `cache_write_tokens` 是其中由前缀缓存供给 / 这次写进缓存的部分（只有 Anthropic 报写入）；provider 没报的一律留空，不记成 0。具体服务可能更改默认值，所以未设置 effort 时只记录“服务端默认”，不能虚构当时采用的强度。索引任务还记录向量空间、代次与条目版本。遥测（OpenTelemetry 链路与指标）、日志与运维视图的设计在 [`docs/observability.md`](observability.md)。

### 13.4 Crate 划分与编译预算

编译速度是架构约束，不是事后优化。基线：对一个同类 Rust 工作区（9 crate、约 97k 行，同样依赖 Turso / toasty / axum / reqwest）做冷编 `cargo build --timings`，总墙钟 64.7s，关键路径 `turso_core` 13.5s → `toasty` → 持有 toasty 模型的 crate 13.5s → 二进制 9.0s。三个结论：**toasty 派生宏展开极贵**（11k 行编得和整个 Turso 引擎一样久），持有模型的 crate 必须最小、最少被重编；**二进制 crate 在关键路径末端**，任何下层改动都要再付一次，它必须薄；最重的三方单元（`aws-lc-sys` 17s 构建脚本、`tantivy` / `zstd-sys` 14s、`rusqlite`）全部来自默认特性而非需求。

划分原则：按**依赖重量**和**变更频率**切，不按 DDD 层。重依赖（toasty/turso、axum、reqwest、ratatui、渠道 SDK）各自只出现在一个 crate；高频改动的代码所在的 crate 不依赖任何派生宏重的东西；被所有人依赖的 crate 不含 I/O、不含 tokio；二进制只有 clap 分发。

```text
komo-kernel    值类型、状态机（Run / ToolCall / Session 生命周期）、事件与 fold、
               cron 到期计算、§8.4 恢复决策表与 §8.9 reconcile 的判定（都是纯函数）、
               Intervention 的类型与派生规则、协议线格式、全部 trait（§13.5）。
               deps: serde, serde_json, uuid, time, thiserror, async-trait, sha2, croner（`default-features = false`，
               无 chrono；它自己仍拉 derive_builder / darling / strum，接受）。不依赖 tokio，不带 tzdb。

komo-store     session_log（JSONL 写入器 / 范围读 / 尾部校验）、payloads、tool_output、
               Turso Db + toasty 模型 + ensure_schema、repositories、Coordinator（impl Ledger）、checkpoint。
               唯一依赖 toasty/turso 的 crate。deps: kernel, toasty(turso only), turso, tokio(fs, sync), sha2

komo-runtime   agent loop、executor、tools/{read,write,edit,rg,bash}、
               codemode（Monty worker 与协议，`komo __codemode-worker` 的函数体也在这里）、
               memory（MemoryManager、索引、代次）、llm 与 embedding 适配器、
               scheduler、recovery、config。日常改动落在这里；它从不重新展开 toasty 宏。
               deps: kernel, store, reqwest, tokio, grep + ignore（`rg` 工具）、arc-swap、monty + monty-fs（codemode 的脚本引擎与只读文件挂载）。
               不依赖 komo-agent。

komo-agent     Agent 的身份、能力与上下文装配（`docs/bot.md` 批次 1，2026-09-22 抽出）：
               Profile → 能力面选择（`surface_of`，目录里没有的名字不算数）、skills 发现与
               目录行（`SkillRegistry`，§5.6）、编排操作名（`DELEGATE_TOOL`）、上下文装配
               （`context`：`ContextInput` → `AgentContext`，系统提示、回放、记忆段）。
               只描述"要做什么"，
               不执行；值类型（`AgentProfile` / `AgentSurface` / `RunSnapshot`）留在
               kernel（事件、协议与 store 模型按它们落盘，搬出来会把依赖指反）。
               deps: kernel, serde_json, tracing。

komo-gateway   axum 路由（§13.1）、SSE、认证、进程锁与发现文件、launchd/systemd 集成、
               Dispatcher、飞书 / Telegram / WeChat 渠道、Notifier 实现、deliveries、介入通知渲染。
               组装 runtime 与 agent（两者互不依赖）。
               deps: kernel, runtime, agent, axum, tower-http, reqwest, openlark(feature "feishu"), wechatbot(feature "wechat")

komo-client    HTTP + SSE 客户端、发现文件读取、ratatui 聊天 TUI（视口 + 终端回滚区，
               含介入清单、Markdown 渲染）、操作子命令的输出渲染。
               只依赖 kernel——不认识 store / runtime。
               deps: kernel, reqwest, tokio, ratatui, crossterm, pulldown-cmark

komo (bin)     clap 分发：`komo` / `komo resume` → client 的 TUI；操作子命令 → client；`komo gateway` → gateway；
               `komo update`（§13.6）是 bin 里唯一自己发 HTTP 的地方。
               deps: client, gateway, clap, reqwest, flate2, tar
```

依赖只向下，三条支线在 bin 汇合：

```text
kernel ← store ← runtime ← gateway ─┐
kernel ← agent ←──────── gateway ───┤
kernel ← client ────────────────────┴─ komo (bin)
```

`gateway` 看不到 toasty 类型（store 只通过 runtime 的构造函数暴露）也看不到 ratatui；`runtime` 看不到 axum，也**不依赖 agent**（装配是 gateway 的事）；`client` 只认协议类型。改 TUI 不重编服务端，改服务端不重编 TUI；冷编时三条支线并行。什么时候再拆：runtime 里 memory 一旦超过 8k 行或引入自己的重依赖，拆成 `komo-memory`；触发条件是编译时间实测，不是模块数。

依赖清单与禁用项：

| 依赖 | 配置 | 理由 |
|---|---|---|
| `toasty` | `default-features = false, features = ["turso"]` | 去掉 `sqlite` 特性拉进来的 rusqlite / libsqlite3-sys |
| `turso` | 与 `toasty-driver-turso` 同 major，工作区唯一 | mimalloc 全局分配器只能有一个 |
| `reqwest` | `default-features = false, features = ["rustls-no-provider", "json", "stream", "charset", "form"]` | `rustls` 特性 = `__rustls-aws-lc-rs`；`no-provider` 后由 komo 在 `main` 里 `rustls::crypto::ring::default_provider().install_default()`，且必须在飞书 ws 线程启动之前。`form` 是 ChatGPT OAuth 的 token 端点要的 `application/x-www-form-urlencoded`（§13.3），只多拉 `serde_urlencoded` 一个纯 Rust 包 |
| `rustls` | `default-features = false, features = ["ring"]` | 同上 |
| `base64` | `0.22`，仅 komo-runtime | ChatGPT OAuth 的 access_token / id_token 是 JWT：只 base64url 解 payload，不验签（§13.3）。0.22.1 已经是依赖树里被拉得最多的一份（reqwest 等间接拉入），钉住它不再添一份重复版本；reqwest 自己另拉一份 0.23.1，是既有的重复，与本次改动无关 |
| `tempfile` | 已在工作区（此前只在各 crate 的 `[dev-dependencies]` 里）；komo-runtime 的 `[dependencies]` 新增一行 | ChatGPT 凭证文件的原子写（临时文件 + rename，§13.3）——不引新包，只是把已有的测试依赖也用到生产代码里 |
| `tokio` | 按需特性，不用 `full` | kernel 不依赖 tokio |
| `axum` | 默认 + `tower-http/cors` | 仅 gateway；gateway 自己直接声明 `tokio`（`rt-multi-thread`, `signal`, `fs`），不靠 axum 传递 |
| `tower-http` | `0.6`，`default-features = false, features = ["cors"]` | 与 reqwest 对齐（两个大版本都要 `^0.6`）；选 0.7 只会多一条自找的重复版本 |
| `sha2` | `0.10` | openlark-core 用 0.10；选 0.11 会把 `digest` / `block-buffer` / `crypto-common` / `cpufeatures` 一起劈成两份 |
| `croner` | `default-features = false`（W2 确认无 chrono 时的时刻表达） | kernel 已有 `time`，不要第二套日期时间库；croner 4 默认特性拉进 chrono + derive_builder / darling / strum |
| `grep` + `ignore` | `0.4`，`default-features = false`；仅 komo-runtime（`rg` 工具） | 内嵌 ripgrep 的搜索与遍历，不起子进程：外部 `rg` 要么没装、要么各家版本不同，而遍历规则（隐藏文件、`.gitignore`、覆盖 glob）本来就该用这套实现。默认特性集是空的，写出来是钉住两件事：**不开 `pcre2`**（"正则语法 = Rust regex"是对模型可见的契约）与不引 SIMD 的 `avx-accel`。带进来的 13 个包（`globset`、`walkdir`、`termcolor`、`crossbeam-deque`、`encoding_rs_io`、`memmap2`、`bstr` 等）全是纯 Rust、无 C 工具链，且没有新的重复版本（2026-09-21 对 `cargo tree -d` 核过） |
| `jiff` | `0.2`，默认特性（系统 tzdb + 系统时区）；komo-runtime 与 komo-client | runtime：cron 的 `ZoneResolver`（IANA 区名 → 偏移，§10）。client：CLI 的时刻一律印成本机墙钟 + 偏移（`2026-09-25 14:00:00 +08:00`），偏移按每个时刻各自取，夏令时两侧不同；不用 `time` 的 `local-offset`，它只能取"现在"的偏移，且在多线程进程里拒绝返回。kernel 仍只用 `time` |
| `arc-swap` | 默认；仅 komo-runtime（`config`） | §3 热重载的唯一 `Arc<ConfigSnapshot>` 原子替换 |
| `serde_ignored` | `0.1`，默认；仅 komo-runtime（`config`） | §3 的「不认识的键只警告」：包住 `toml::Deserializer`，把每个被忽略的键路径交给回调。只依赖 `serde`，不进别的 crate 的图 |
| 判断后端（TypeSafe「System One」） | **不新增依赖**：走已有的 `reqwest`（`rustls-no-provider`）+ 同一个 ring provider；凭证只从 `.env` 的 `TYPESAFE_API_KEY` 读 | §9.4 的可选重排用它。默认关；开着时才会发请求，且会把候选记忆的正文（截到 200 字符）发给第三方端点 |
| `tracing-subscriber` / `rustls`（**dev-only**） | 仅 komo-runtime 的 `[dev-dependencies]` | 前者用来抓 §47/§48 的行内指标做断言，后者让真机验收测试自己装一次 crypto provider（装 provider 是进程的事，见上面 `reqwest` 行）。**不进发布构建** |
| `opentelemetry` / `opentelemetry_sdk` | `0.33`；sdk `default-features = false, features = ["trace", "metrics"]`；仅 bin | 遥测（`docs/observability.md` §4）。SDK 只在 bin 初始化，runtime / gateway 只写 `tracing`。实测编译（2026-10-19，`cargo clean -p` 这几个包后 `--timings`，与别的构建并发、含等锁）：opentelemetry 12.7s、opentelemetry_sdk 13.1s |
| `opentelemetry-otlp` | `0.33`，`default-features = false, features = ["http-proto", "reqwest-blocking-client", "trace", "metrics"]`；仅 bin | OTLP/HTTP + protobuf，走工作区同一份 reqwest 0.13（多开其 `blocking` 特性）；不开 `grpc-tonic` / `logs`。**blocking 而非 async 客户端**：SDK 在无 tokio reactor 的线程上 `block_on` 导出。`opentelemetry-otlp` 12.7s、`opentelemetry-http` 25.9s、`opentelemetry-proto` 6.9s；`cargo tree -d` 重复项与加之前逐条相同（95 个根），无新重复 |
| `tracing-opentelemetry` | `0.34`，默认特性（含 `metrics`）；仅 bin | `OpenTelemetryLayer` + `MetricsLayer`（6.1s） |
| `tracing-appender` | `0.2`，`default-features = false`；仅 bin | Gateway 日志按天轮转、留 7 个（`docs/observability.md` §6）（6.3s） |
| `clap` | `derive` | 仅 bin |
| `flate2` / `tar` | `flate2` 显式 `default-features = false, features = ["rust_backend"]`（miniz_oxide），`tar 0.4` | 仅 bin：`komo update` 解发布包（§13.6）。两者都是纯 Rust（不带 C 工具链），且**不进 gateway / runtime 的图**——它们谁也不更新自己；`rust_backend` 写死是不跟上游默认后端变 |
| `ratatui` + `crossterm` + `pulldown-cmark` | 仅 komo-client | 在 client 支线上，与 gateway 并行编译 |
| `syntect` / `two-face`（代码高亮） | 可选，按编译预算实测再定 | 纯 UI 增强，不是需求 |
| `openlark` | 仅 gateway，feature `feishu`（默认开），`default-features = false, features = ["websocket"]` | 只要 ws 长连接；事件负载用 `register_raw` 拿原始 JSON、自己的宽容 serde 结构解析。带来 `prost 0.13`（turso 同步引擎用 0.14）和一份 `tokio-tungstenite` 重复——接受，不再自己额外引入 tungstenite |
| `wechatbot` | 仅 gateway，feature `wechat`（默认开）；**`[patch.crates-io]` 指向 `vendor/wechatbot`**（0.4.0 原样复制，只改 `Cargo.toml` 一行：reqwest `0.12` 默认特性 → `0.13`，`default-features = false, features = ["json", "rustls-no-provider", "http2", "charset"]`） | 上游所有发布版本都写 `reqwest = { version = "0.12", features = ["json"] }`，默认特性开着 = native-tls = openssl-sys，自己一个 feature 都没有；cargo 特性只加不减，工作区里无论怎么声明都关不掉。patch 后复用 komo `main` 装的同一个 ring provider，openssl / native-tls / hyper-tls 整条 C 构建链消失，**构建不再依赖任何系统库**（Fedora 不必 `openssl-devel`，Mac 不必 `brew install openssl@3`），且 reqwest 只剩 0.13 一份（wechatbot 用到的 reqwest API 面极小，0.13 全保留；2026-09-16 实测默认特性全量构建通过）。不选 vendored openssl：OpenSSL 3.6.3、1210 个 .c，`make depend` / `install_dev` 串行，估 2–3 分钟且在 wechat 关键路径起点，会取代 turso 链成为新关键路径。升级 wechatbot = 重新复制 + 重打这一行，见 `vendor/README.md` |
| `qrcode` | `default-features = false`；微信登录二维码渲染为终端字符 | 不需要 `image` |
| `libc` | `0.2`，仅 komo-runtime 的 unix 目标 | Gateway 起来时 `setrlimit` 抬能开的文件数（`limits.rs`，launchd 默认软上限 256，子进程跑 `cargo test` 不够）；codemode worker 的内存看门狗读 RSS（macOS `proc_pid_rusage`、Linux `/proc/<pid>/statm` 的页大小）。原本就被 tokio 等拉进依赖树，不新增包、不新增重复版本。（2026-10-15 之前还用于 codemode 的 Linux 沙箱：Landlock / seccomp 的系统调用号，随沙箱删除。） |
| `monty` / `monty-types` / `monty-fs` | `=1.1.0`（精确钉版本）；仅 komo-runtime | codemode 的脚本引擎（`docs/codemode.md` §2）：pydantic 的 Python 子集解释器，语言层没有文件 / 网络 / 子进程。精确钉：快照格式与 `monty_types::unstable`（JSON 转换用的节点访问器）没有兼容保证，升级必须重跑回放集。**不用 `monty-alloc`**：它要占 `#[global_allocator]`，与 turso 的 `mimalloc` 冲突（`the #[global_allocator] in this crate conflicts with global allocator in: turso`），内存限制改用 Monty 的 `max_memory` 软限 + 宿主的 RSS 看门狗。`monty-fs`（只读文件挂载，`=1.1.0`，同样精确钉；带进 `cap-std` 一族）。没用 `monty-pool`（进程池）。带进来约 76 个新包（ruff 解析栈、`unicode_names2` 等），新增重复版本见下 |
| `num-bigint` | `0.4`；仅 komo-runtime | Monty 的大整数与 JSON 之间的转换（`codemode/convert.rs`）；已在依赖树里（Monty 与 toasty 一侧都拉），不新增包 |
| `rmcp` | `3.x`，`default-features = false, features = ["client", "transport-child-process", "transport-streamable-http-client-reqwest", "reqwest-tls-no-provider", "auth"]`；仅 komo-runtime | MCP 客户端（`docs/mcp.md`），官方 Rust SDK。HTTP 走工作区同一个 reqwest 0.13（rustls-no-provider），不开 server 与宏。`auth`（2026-10-14，`oauth = true` 的服务器，`docs/mcp.md` §2.1）新拉 `oauth2 5.0`（不开默认特性），它的 chrono / rand / url 等依赖图里已有，Cargo.lock 只多这一个包。新拉 `process-wrap`、`sse-stream`、`futures`，新增一条重复版本 `nix 0.31`（process-wrap；既有 0.29）。实测（2026-10-08，增量重编）：rmcp 2.8s + 构建脚本 0.3s + process-wrap / sse-stream 各 0.2–0.3s |
| **不引入** | image | — |
| `tantivy` / `zstd-sys`（经 `turso` 默认特性 `fts`） | **已做**：工作区的 `turso` 写 `default-features = false, features = ["mimalloc"]`；`toasty-driver-turso` 0.11 起上游自己对 `turso` 关了默认特性，原先 `vendor/toasty-driver-turso` 那份补丁（只改 manifest 里 `turso` 那几行）已删 | 实测（2026-09-16）：冷编 71.5s → 64.2s，编译单元 577 → 515，单元耗时总和 −36s CPU，`tantivy` / `zstd` / `lz4_flex` 从依赖树消失，少一条 C 工具链。墙钟只省 7s 是因为它们与 `aws-lc-sys` 并行、不在关键路径。功能零损失：turso 的 FTS 是索引方法，MVCC 直接拒绝。升级 toasty 时核对驱动对 `turso` 的版本与特性，CI 用 `cargo tree -e features -i turso` 断言只剩 `mimalloc`；见 `vendor/README.md` |

三个渠道各自一个 feature，默认全开。feature 的用处不是裁功能，是让 `cargo tree -d` 能按渠道归因重复版本，以及某家 SDK 坏掉时能单独关掉它继续构建。`cargo tree -d` 进入 CI：出现新的重复版本要有理由。

编码规则：

- 接缝上 `dyn`，不泛型：`Vec<Box<dyn Tool>>`、`Arc<dyn LlmClient>`、`Arc<dyn Ledger>`。泛型执行器让每个组合单独实例化，编译时间和二进制都付账，而这里没有需要内联的热路径。
- 派生宏只用 `serde`、`toasty::Model`（store 内）、`clap::Parser`（bin 内）、`thiserror::Error`。不用 `strum`、`derive_more`、`bon`。
- 测试放在代码旁 `#[cfg(test)]`；跨 crate 需要的替身放在各 crate 的 `test-support` feature 后面，只作为 dev-dependency 启用。
- kernel 的 `lib.rs` 只有模块声明。
- 一个 Session 目录的三个写入点（JSONL、payloads、tool-output）全部在 store，runtime 只见 `Ledger` 和 `ToolOutputStore`。

```toml
[profile.dev.package."*"]
debug = "none"               # 第三方不带调试信息

[profile.dev]
debug = "line-tables-only"   # 自己的 crate 保留行号即可

[profile.release]
strip = "symbols"
lto = "thin"
codegen-units = 16
```

编译预算——第一阶段结束时在目标 Mac 与 Fedora 上各测一次并记入仓库，之后每个阶段重测；下面是初始预算，实测后校准：

| 场景 | 命令 | 初始预算 | 骨架实测（2026-09-16，Fedora） |
|---|---|---|---|
| 冷编 | `cargo build` | ≤ 60s（基线 65s：去掉 aws-lc、rusqlite、MCP、image；保留 ratatui 与三家渠道 SDK；若 openlark 拉回 aws-lc，+20s） | 70.2s（8 核 / 11 GB；`--no-default-features --features feishu,telegram`——本机缺 `openssl-devel`，`wechat` 无法构建。关键路径 `zstd-sys` 构建脚本 10.5s → `tantivy` 14.6s → `turso_core` 26.8s → `turso_sync_*` → `toasty-driver-turso` 5.1s → `toasty` 3.3s → 四个 komo crate 0.4s。openlark 确实把 aws-lc 拉了回来：`aws-lc-sys` 构建脚本 32.4s，但并行不在关键路径上。三个渠道全关时 68.5s） |
| 改 runtime 一行 | `touch crates/komo-runtime/src/agent.rs && cargo check` | ≤ 5s | 0.34s（骨架为空，只验证扇出形状：runtime → gateway → bin） |
| 改 client 一行 | `touch crates/komo-client/src/tui/app.rs && cargo check` | ≤ 5s；不得触发 gateway / runtime / store 重编 | 0.32s；只重检 komo-client 与 bin，gateway / runtime / store 未重编 |
| 改 store 一个模型 | `touch crates/komo-store/src/models/run.rs && cargo check` | ≤ 12s（toasty 展开不可避免，但只影响 store 及以上） | 0.37s（骨架里还没有 toasty 模型，此数不代表展开成本；文件现为 `src/models.rs`） |
| 改 kernel 一个类型 | `cargo check` | 全量，允许 ≤ 20s；这是有意为之的代价 | 0.41s（六个 crate 全部重检，骨架为空） |
| `cargo test --workspace` | 增量 | ≤ 30s | — |

预算不达标时，先用 `--timings` 找关键路径，再决定拆 crate 或换依赖——不凭感觉拆。

**2026-10-15 codemode 换 Monty 的构建账**（本机 macOS，`cargo build --timings`，空 target 目录；测量时机器满载、别的 agent 在并行编译，墙钟不可比，只记事实，**预算文字不改**——60s 的基线早已超了，见 §14 的 2026-09-21 一条）：

- 编译单元 577 → 661（+84），`Cargo.lock` 583 → 647 个包（+64）。新增单元 CPU 合计 146.6s：`monty` 38.3s（在关键路径上，可与 `turso_core` 并行）、`ruff_python_ast` 10.0s、`object` 9.9s、`monty-types` 6.9s、`bitvec` 6.6s、`unicode-general-category` 构建脚本 5.5s、`itertools 0.15` 4.4s、`ruff_python_parser` 4.2s。
- 两次整体墙钟：带 monty 3m28s、基线 4m48s（后一次撞上别的构建，顺序也不同，**不能当结论**）。POC 在较安静时测到冷构建 98s → 111s（+13s）、release 冷构建约 +8 MB。
- `cargo tree -d` 新增的重复版本（基线 90 行 → 99 行），全部来自 monty 的依赖：`compact_str 0.10`（`ruff_python_ast`，ratatui 那边原本就有 0.9.1）、`itertools 0.15`（`ruff_python_literal`；原有 0.12 / 0.14）、`bit-set` / `bit-vec 0.8`（`fancy-regex`，Monty 的正则引擎）、`phf_shared 0.11.3` ×2 与 `rand 0.8.8` 的又一份特性集（`phf_generator` / `unicode_names2` 构建脚本）、`smallvec 1.16.1` 的另一份特性集。komo 自己直接依赖的包没有被挤成两份。
- 2026-10-15 再加 `monty-fs`（脚本文件只读挂载）：`Cargo.lock` +10 个包（`monty-fs`、`cap-std`、`cap-primitives`、`ambient-authority`、`fs-set-times`、`io-extras`、`maybe-owned`、`rustix-linux-procfs`、`winx`，加两份 `io-lifetimes`）；`cargo tree -d` 99 → 101 行，新增的重复版本只有 `io-lifetimes 2.0.4` 与 `3.0.1`（`cap-primitives` / `io-extras` / `fs-set-times` 各取一个大版本，都来自 `monty-fs` 的 `cap-std`）。其余（`rustix` 等）与树里原有的同版本，没有再多出一份。


### 13.5 trait 边界

一个 trait 只在两种情况下存在：**有第二个实现**（生产之外还有测试替身，或有多个后端），或**要做编译防火墙**（上层依赖 trait 就够，不需要看到 toasty / reqwest / axum 类型）。其余组件是具体类型，直接构造、直接测；不为"以后可能换"预留 trait。所有 trait 定义在 kernel，实现散布在 store / runtime / gateway。接缝上一律 `Arc<dyn Trait>` / `Box<dyn Trait>`，异步 trait 用 `async_trait`（对象安全，一次堆分配相对模型往返可忽略）。

| trait | 生产实现 | 测试实现 | 为什么是 trait |
|---|---|---|---|
| `Ledger` | `Coordinator`（store）：正文 → JSONL → 数据库 | `MemLedger` | Agent Loop 的唯一写入口；让 loop 和 executor 的测试不碰文件与 Turso |
| `ToolOutputStore` | 文件流式写入 + 原子发布（store） | 内存 | 子进程 stdout/stderr 流式落盘，`bash` 测试需要替身 |
| `RunQueue` | `toasty::sql::statement` 条件更新 + 代次（store，无第二个 turso 句柄） | 内存 | 调度器、恢复扫描、Cron、手动 resume 共用的领取入口；并发领取测试 |
| `CronRepo` | toasty（store） | 内存 | 调度器 + CLI + Gateway API |
| `MemoryRepo` | toasty + `memory_terms`（store） | 内存 | `MemoryManager` 的全部状态读写；召回排序逻辑要在无数据库下测 |
| `LlmClient` / `TurnDriver` | 每种协议一个适配器（runtime） | 脚本化 driver | 主模型、记忆模型是同一 trait 的两个实例；loop 测试用脚本化回合 |
| `EmbeddingClient` | OpenAI-compatible / Ollama（runtime） | 固定向量 | 空间指纹与维度校验要在无网络下测 |
| `Tool` | 6 个（runtime） | — | §4 已定；executor 只认 trait |
| `Channel` / `Inbound` / `Notifier` | 飞书、Telegram、WeChat、HTTP API、SSE（gateway） | 内存渠道 | 首版就有三个真实现；介入通知的渲染是各渠道实现的事 |
| `Clock` | 系统时钟 | 可拨时钟 | Cron 到期、`valid_until`、重试退避全部依赖时间 |
| `ZoneResolver` | tzdb 实现（runtime，W3 选 crate） | `ScriptedZoneResolver`（可编脚本的假时区）/ `FixedOffsetZone`（降级） | §10 的夏令时两条规则要在无 tzdb 下测；kernel 不带时区数据库 |

不是 trait 的东西：`AgentLoop`、`ToolExecutor`、`Scheduler`、`Recovery`、`MemoryManager`、`SkillRegistry`、`Coordinator`、`Dispatcher`——各只有一个实现，依赖上表的 trait 就可测。`SessionRepo`、`ToolCallRepo`、`CheckpointRepo`、`OutboxRepo` 是 store 内部的具体类型，只被 `Coordinator` 用。**Intervention 不需要 `Approver` trait**：清单（§7.5）是 `runs` 与 `tool_calls` 上的查询，没有需要注入的第二实现。Telegram / WeChat / 飞书的 `/answer`、TUI 的待处理清单、CLI 子命令都打到 `POST /v1/interventions/{handle}/answer`；`satisfied` / `not_performed` 走 executor 的收尾与 `RecoveryStore::requeue`，`resolve` 重新入队再观察，`abandon` 走 `Ledger::complete`。渠道之间的差别只在渲染（§11.3），不在决策。（2026-10-11 之前这里还有 `Policy` trait 与 `ApprovalRepo`，随 Policy 删掉，§7。）

关键签名（接口示意，辅助类型省略；`Tool` 见 §4）：

```rust
/// 每个 Session 一个实例，串行。每个方法对应 §8.5 的一段箭头，
/// 方法内部完成"外置正文（如有）→ JSONL 追加并同步 → 数据库事务"的顺序。
#[async_trait]
pub trait Ledger: Send + Sync {
    /// 预留 Run（`accepted`）→ run.accepted → `queued`。同一 request_key 返回原 Run，哈希不同则拒绝。
    async fn accept_input(&self, input: AcceptInput) -> Result<Accepted, LedgerError>;
    /// 完整 assistant 回复 + 本轮全部调用计划，一个逻辑事件；返回 Runtime 分配的 ToolCallId。
    async fn record_round(&self, run: &RunId, round: AssistantRound) -> Result<Vec<ToolCallId>, LedgerError>;
    /// tool.planned：准备好的执行计划落盘（§8.4 第 6 行要求"planned 而未执行"是可分辨的状态）。
    async fn plan_call(&self, call: &ToolCallId, plan: &ExecutionPlan) -> Result<EventId, LedgerError>;
    /// tool.started + 执行尝试，同一事务；返回后才允许产生真实副作用。
    async fn start_call(&self, call: &ToolCallId, plan: &ExecutionPlan) -> Result<AttemptId, LedgerError>;
    /// 输出已发布；写入 tool.result 完整结构化正文、元信息与引用。
    async fn finish_call(&self, attempt: &AttemptId, published: PublishedOutput) -> Result<(), LedgerError>;
    /// 一次**没有执行过**的调用的结论（工具名不认识、参数准备不出来、`prepare` 拒绝、子代理不能
    /// 再委派）：它有结论、没有尝试，所以建的是那条 `tool.started` 缺席的尝试行。
    /// **必须写**：`tool.result` 缺席的调用在账本上永远悬着，从账本重建的转写就会带着一个
    /// 没有输出的 `function_call`，provider 直接 400。
    async fn fail_call(&self, call: &ToolCallId, attempt: &AttemptId, published: PublishedOutput) -> Result<(), LedgerError>;
    /// 停在某个外部条件上（§8.4）：状态变 `waiting`，理由进 `WaitReason`，释放执行名额。
    async fn suspend(&self, run: &RunId, wait: Wait) -> Result<(), LedgerError>;
    /// 调用前这一轮的最终回复必须已作为 message.assistant 落盘（record_round → complete）；
    /// run.completed 的 final_message 只用于补读，不进消息面。
    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError>;
    /// 按 seq 分页读事件（短事务，limit=0 由实现定页大小）；EventBatch.next 为 None 即读到末尾。
    async fn read(&self, session: &SessionId, from: Seq, limit: u32) -> Result<EventBatch, LedgerError>;
    /// `/new`：追加 conversation.boundary，不切 Session。
    async fn boundary(&self, session: &SessionId) -> Result<Seq, LedgerError>;
    /// control_outbox 审计补写，按 event_id 幂等；不产生状态。
    async fn append_audit(&self, session: &SessionId, event_id: &EventId, payload: EventPayload, occurred_at: OffsetDateTime) -> Result<Seq, LedgerError>;
}

#[async_trait]
pub trait ToolOutputStore: Send + Sync {
    /// 为一次尝试打开流式写入器（stdout/stderr 写 .partial）。上一个进程留下的 .partial 由它接管。
    async fn begin(&self, attempt: &AttemptRef) -> Result<Box<dyn OutputWriter>, StoreError>;
    /// 收齐后同步、原子写 output.json，返回带路径、大小、哈希的引用。
    async fn publish(&self, writer: Box<dyn OutputWriter>, result: ToolResultBody) -> Result<PublishedOutput, StoreError>;
    /// 按引用读取并校验哈希；不匹配返回 Corrupt，不返回内容。
    async fn open(&self, output: &OutputRef) -> Result<VerifiedOutput, StoreError>;
    /// 一次被打断的尝试留下的部分输出，各取末尾至多 tail_bytes（§8.6 的中断结果用）。只读。
    async fn interrupted(&self, attempt: &AttemptRef, tail_bytes: usize) -> Result<InterruptedOutput, StoreError>;
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// 一次 Run 的一个执行段。工具 Schema、系统提示、记忆注入在这里装配一次。
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError>;
}

#[async_trait]
pub trait TurnDriver: Send {
    /// 一次完整的 provider 往返。First 是首轮；ToolResults 按 call_id 回传上一轮结果，
    /// revised 按 provider_call_id 就地换掉更早结果的正文（配对不动）。
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError>;
    fn usage(&self) -> TokenUsage;
}

#[async_trait]
pub trait EmbeddingClient: Send + Sync {
    /// §9.5 的空间指纹；凭证不进入。
    fn space(&self) -> &EmbeddingSpace;
    async fn embed(&self, kind: InputKind, texts: &[String]) -> Result<Vec<Vector>, EmbedError>;
}

#[async_trait]
pub trait RunQueue: Send + Sync {
    /// 条件更新 + 递增代次；返回 None 表示没有可领取的 Run（调度器：下一个到期的）。
    async fn claim(&self, executor: &ExecutorId) -> Result<Option<Claimed>, StoreError>;
    /// 领取指定 Run（手动 resume、恢复扫描）；同样的条件更新，两个执行者只一个成功。
    async fn claim_run(&self, run: &RunId, executor: &ExecutorId) -> Result<Option<Claimed>, StoreError>;
    /// 交还名额（等人 / 等重试 / 执行者退出）；代次不对就什么都不做。
    async fn release(&self, claimed: &Claimed) -> Result<(), StoreError>;
}

/// kernel 不带 tzdb；两个方向都要，cron 的搜索两头都走。
pub trait ZoneResolver: Send + Sync {
    fn resolve(&self, zone: &str, local: PrimitiveDateTime) -> Result<ZoneResolution, ZoneError>; // Single / Ambiguous(早, 晚) / Gap
    fn offset_at(&self, zone: &str, instant: OffsetDateTime) -> Result<UtcOffset, ZoneError>;
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
    async fn serve(&self, inbound: Arc<dyn Inbound>, shutdown: Shutdown) -> Result<(), ChannelError>; // kernel 无 anyhow
}

/// Gateway 交给渠道的唯一入口。渠道不知道 Session 是什么。
#[async_trait]
pub trait Inbound: Send + Sync {
    async fn handle(&self, msg: InboundMessage) -> Result<InboundAck, GatewayError>;
}

#[async_trait]
pub trait Notifier: Send + Sync {
    /// 先持久化投递记录再发送。Sent = 已送达；Deferred = 渠道此刻无法推送（微信无回复令牌），
    /// 行留在 pending，由下一条入站消息触发冲刷。
    async fn deliver(&self, target: &DeliveryTarget, msg: Outbound) -> Result<Delivery, DeliverError>;
}
pub struct Delivery { pub id: DeliveryId, pub state: DeliveryState /* Sent | Deferred */ }

pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}
```

**W3 落地后的契约修订**（2026-09-16；实现见 `crates/komo-kernel/src/traits.rs`，与上面的示意不一致时以代码为准）：

- `Tool::execute(&self, plan: ExecutionPlan, ctx: &ToolContext, sink: &mut dyn OutputWriter)`（2026-10-11 起按值收 `ExecutionPlan`，此前是 `ApprovedPlan`）：工具**借用**本次尝试的流式写入器，发布仍在 executor 手里（§8.5 的下一步）。
- `Ledger::start_run(&self, run, executor, generation)` 写 `run.started`；领取（`RunQueue::claim` / `claim_run`）只改行，`rows affected` 是胜负的唯一信号。
- `CronRepo::advance(&self, id, next_run_at, status, last_error)`：推进槽位**不递增版本**：版本只在定义变了时才变（当初的理由是不作废绑定该 Job 的授权，授权 2026-10-11 删掉）。
- `RunEnd::Completed { final_message, rounds }`。（`Wait::Approval` 2026-10-11 删掉；JSONL 里 `WaitReason::Approval` 只为解码旧行保留，§7.4。）
- `StoreError::VersionConflict` + `From<StoreError> for RepoError`（`GrantMismatch` 随授权删掉）：store 的事务闭包只有一条错误通道。
- protocol：`ManualCronRunRequest`、`BoundaryRequest`、`MemoryScope` 的 `Display` / `FromStr`（`personal` | `project:<id>` | `environment:<id>`）、`GET /v1/models` → `ModelsResponse`、`GET /v1/config/check`、`POST /v1/config/reload`、`SseEvent::AssistantDelta`（**只在 SSE 上，永不进 JSONL**；`message.assistant` 仍是一次完整回复）。§13.1 的接口表相应多这三个端点。
- `Ledger::run_end(&self, run: &RunId) -> Result<Option<RunEnd>, LedgerError>`：一条 Run 的终态（只读，不带消息历史）。委派的核对走它（§4、§8.6；2026-10-16 同步委派删掉之后只收旧的 `delegate` 调用，`task` 的交回看 Run 终态用的是 Gateway 的看客，不经它）——父 Run 续跑时用它把那次 `delegate` 调用收尾，而不是去翻会话的整段事件；"子 Run 结束了没有、怎么结束的"是调度层要知道的一件事，不该逼它自己做一遍 fold。

### 13.6 安装与升级

发布产物由 `.github/workflows/release.yml` 在 `v*` 标签上构建，四个平台各一个包：`komo-darwin-arm64.tar.gz`、`komo-darwin-amd64.tar.gz`、`komo-linux-amd64.tar.gz`、`komo-linux-arm64.tar.gz`，外加一份共用的 `SHA256SUMS`（`sha256sum` 的默认两列格式）。**包内只有一个成员 `komo`**。linux 包在 Ubuntu 22.04 上原生构建（ring 与 mimalloc 编 C，不走交叉编译），glibc 下限因此是 2.35；darwin 两个架构都在 arm64 runner 上出。

装与升级是同一条约定的两个入口：`scripts/install.sh`（`curl -fsSL …/main/scripts/install.sh | bash`，认平台 → 问 `releases/latest` 或 `KOMO_VERSION` → 下包与 `SHA256SUMS` → `shasum`/`sha256sum` 核对 → `tar -xzf` → 落到 `komo.new` 再 `mv -f`；默认装到 `~/.local/bin`，不用 sudo，`--prefix` / `KOMO_INSTALL_DIR` 可改——2026-09-24 起，原来是 `/usr/local/bin`）和 `komo update`（§3，用 Rust 自己走一遍同一套名字与校验，`crates/komo/src/update.rs`）。仓库名、资产名、校验和文件名三处必须一致：那两个文件加这里。

`komo update` 的顺序是**下载 → 校验 sha256 → 解包 → 试跑 `--version` → 同目录 `rename`**。三条不变量：

1. **换上去是最后一步，而且是一次 `rename`**。中途任何一步失败（校验和不对、包坏了、试跑不过），现在装着的那个 komo 一个字节都没动；临时文件与目标同目录，跨文件系统的 rename 会退化成复制。自我更新唯一不可接受的结局不是"没更新成"，是"更新成一个跑不起来的东西"。
2. **试跑要求新二进制自报的版本就是标签版本**。glibc 太旧、架构不对、包被改过都在这一步挡下来。这条同时把发布流程绑住了：标签与 `[workspace.package].version` 不一致的包会被拒收，所以 `release.yml` 里那个 `guard` 作业是这条不变量的另一半，不是可选项。
3. **只换磁盘上那份，不碰在跑的进程**。发现文件里核对得上一个在跑的 Gateway 时就打印 `komo gateway restart`，**不替操作者重启**：重启会打断正在跑的 Run，那不该由一条更新命令决定。

只报告，不降级：装着的比最新发布新（源码构建的开发机、或者刚回滚过）时只说一句"发布的比它旧"，什么都不做。

**从本地源码装**是第三个入口：`scripts/install-local.sh [--prefix DIR] [--no-restart]`——`cargo build --release --locked -p komo` → 试跑 `--version` → 复制成 `komo.new` 再 `mv -f`（旧的留 `komo.bak`）→ `komo gateway restart`。与 `komo update` 的第 3 条不变量不同，它**替操作者重启**：会跑它的人就是在本机改 komo 的人，装完不重启等于没装（实测会话里每次"本地重新安装"都以手动重启收尾）；在 komo 的 bash 工具里跑时重启照常推迟到这条命令的结果记进会话之后，声明了 `resume_after_restart` 的监听重启后自动重挂（`docs/monitor.md` §5）。在 git worktree 里跑、且没设 `CARGO_TARGET_DIR` 时共用主检出的 `target/`：实测（2026-10-14）在新 worktree 里冷编 release 用了 659 秒，而依赖本来可以共用。

已核实（2026-09-20，本机 macOS arm64）：`install.sh --prefix` 走真实 `v0.0.2` 发布包，核对与安装都通过，包内成员名就是 `komo`；`komo update` 打真实 `api.github.com`，正确判定"发布的比它旧"并停在原地；`update.rs` 的下载 → 校验 → 解包三段用真实 `v0.0.2` 资产生跑过一遍——`SHA256SUMS` 认得、GNU tar 打的包解得出来（50318240 字节，与 `install.sh` 装出来的那份一致），试跑按预期拒收了标签对不上的那个包（`v0.0.2` 的二进制自报 `0.1.0+00b23c6`，正是上面第 2 条要挡的形状）。发布流程自身（四平台构建 + 校验和 + provenance + 发布）要等 v0.8 的第一个 `v*` 标签才会真跑一次。

## 14. 实现顺序与验收

**TUI 空闲 CPU 验收（2026-10-21）**：`next_stream` 原先每次等待都 clone 连接状态的 `watch::Receiver`，只推进临时副本的已读版本；SSE 连上后，同一次状态变化会反复唤醒驱动并重绘。现在帧与状态在 `SseHandle::next` 中等待，句柄持有状态游标，并以 `borrow_and_update` 消费变化。回归测试：`tui::tests::idle_sse_state_is_consumed_once` 在修复前失败、修复后通过（收到一次已连接后，两次被时钟取消的等待均保持挂起）；`sse::tests::updates_deliver_reconnections_frames_and_the_end` 覆盖连接、重连、恢复、事件帧和订阅结束。`cargo test --workspace`：2096 通过、8 忽略、0 失败。隔离假 Gateway 的空会话、120×40 PTY、各测 3 秒：已安装 release 为 2.77 秒 CPU / 3.015 秒墙钟（91.9%），修复后的 dev 构建为 0.03 秒 CPU / 3.000 秒墙钟（1.0%）。真实会话的旧进程尚未更新。增量 `cargo build --timings`：7.73 秒，只重编 `komo-client` 与 `komo`；报告在 `target/cargo-timings/cargo-timing-20261021T020540806Z-cad003b0e67e15b9.html`。

| 阶段 | 交付 | 验证 |
|---|---|---|
| 1. 进程与会话骨架 | 六个 crate 骨架（§13.4）；komo、Gateway 自动启动、HTTP/SSE；`komo-store`：Turso 连接、`ensure_schema` + DDL 对齐测试、`with_write_retry`、统一 Session 目录与状态索引；`komo-client`：HTTP/SSE 客户端与 TUI 骨架；**编译预算首次测量** | 多个 CLI 同时启动仅产生一个实例；断线后能查看原会话；两个写入器对不同 Session 并发提交不互相阻塞，对同一行冲突时一方重试成功且只应用一次；DDL 对齐测试挂掉能定位到列；改 client 一行不重编 gateway；保存一份合法的 config.toml 后 `komo doctor` 一秒内显示新 mtime 已生效，保存一份非法的则旧配置继续生效且 home chat / 日志有具体错误 |
| 2. AgentLoop 与执行 | 模型往返、执行计划、`prepare` → `start_call` → `execute`；`Ledger`；loop 用 `MemLedger` + 脚本化 `TurnDriver` 测（2026-10-11 之前这一阶段还有 Policy、Allow/Ask/Deny、审批持久化与 `ApprovedPlan` 类型状态，§7） | `start_call` 返回之前没有副作用；同一轮的两条只读调用同时在飞（`read` / `rg`），写是屏障、取消要收齐已在飞的那些（§6）；**不在本次运行能力面里的工具，模型拼出名字也执行不了**，而交给模型的 schema 与执行时的表来自同一份（§4）；**`write` / `edit`（含 `then_run` 的改动那一步）写数据目录下的 `sessions/`、`state.db*`、`runtime/`、`.env` 在 `prepare` 被拒**，结果 `denied`、理由点名路径，账上有结果不悬着（§7.2） |
| 3. 六个工具 | 文件、搜索、进程、codemode 与输出处理（`python` 工具与 Python 环境 2026-10-15 删除，§4）；重放策略声明（`Tool::replay`，默认 unsafe，§8.6） | 文件版本冲突可见；取消停止子进程；未知工具无法调用；`rg` 是只读计划（可并发、codemode 可调），且它不依赖机器上装没装 `rg` |
| 4. 聊天入口（飞书 + Telegram + WeChat） | `Channel` / `Inbound` / `Notifier`、Dispatcher、`allow_from` / `home_chat` / `guest_agent` 配置与 `/id`、`deliveries`、介入通知渲染、飞书 Run 回复卡片、`komo channel probe` | 同一 `event_id` / `update_id` / `msg_id` 重发只产生一个 Run，同一条介入被答两次第二次得到「已决定」；不在 `allow_from` 的发送者被拒且不留记录，`/id` 对其仍可用；把发送者加进 `allow_from` 并保存后，不重启 Gateway 其下一条消息即进入 Run；`/answer` 重发只答一次；来源会话与 home chat 都收到介入通知且第二个答复得到"已决定"；`/approve`、`y` 不是命令；Gateway 重启后 pending 投递补发一次，老库里 `approval_request` / `approval_settled` 的 pending 行被跳过而不卡住补发；飞书 ws 断线重连不丢事件也不重跑 Run；微信在用户未发消息前 `Deferred`，发消息后先收到积压的介入通知 |
| 5. 自动恢复与 resume | 持久队列、启动扫描、领取去重（`RunQueue` 条件更新 + 代次，`toasty::sql`）、检查点、调用核对与子进程回收；恢复决策表在 kernel 里是纯函数 | 重启自动接续原 Run；已完成动作不重放；未知效果不盲目重试；两个执行者并发 `claim` 同一 Run 只有一个成功；决策表对 §8.4 每一行有一个单元测试；重启后等待中的介入仍能在手机上 `/answer`；**旧数据照常起来**：带 `approval.*` 事件的旧会话能加载、回放与续聊（那些事件按未知事件解码），停在 `waiting + approval` 的旧 Run 在 Gateway 启动后按"批准"续跑、那次调用只执行一次，老库里多出来的 `approval_requests` / `policy_grants` 表不妨碍启动（§7.4） |
| 6. Skills | `SkillRegistry`、目录行门控、`read` 只读根（toolbox 已于 2026-10-15 删除，§5.3） | 新增 SKILL.md 无需重启即被 `komo skills list` 看到；`requires_tools` 不满足时不出现在提示里但仍可 inspect |
| 7. Cron | 持久调度、去重、重叠处理、人工接手 | 重启不重复创建同次触发；命令 Job 触发时直接执行、全程零模型请求，`cron add` 不签任何授权；Cron Run 的介入在聊天里 `/answer` 后继续，不获得交互操作者身份 |
| 8. 模型配置与 Memory | 独立记忆 / 向量模型、effort 校验、自动提取、混合召回（`memory_terms` + 向量）与遗忘 | 不串用模型配置；推断不自行确认；过期或遗忘内容不召回；重建可恢复 |
| 9. 场景与双平台验收 | 真实代码仓库、测试 HA 设备、Memos 主动记录；编译预算复测 | 两平台独立运行；记录从 Memos 找回；代码交付含验证证据；冷编与增量编译在预算内，或有记录在案的偏差原因 |
| 10. 生命周期与对账 | Session 的 `closing → deleted → purged`（§8.10）、Intervention 统一清单与四种结论（§7.5）、reconcile 的三条判定与领取守卫（§8.9） | **逻辑删除可靠**：`delete` 之后新输入被拒、未完成 Run 各写一条明确取消、内容一个字节没动；`purge` 在引用未处置时 409 并列出要先处理什么；`purged` 之后没有任何路径再创建那个目录。**清单堵死漏网**：`intervention list` 为空而会话被挡住这种组合构造不出来——"挡队"与"进清单"同源，且每一条 `waiting + intervention` 都在清单里（旧的 `waiting + approval` 不进清单，启动时按 §7.4 续跑）。**orphan 自动 reconcile**：目录被手工删掉而状态不是 `purged` → 对账报出来且 Run 不被领走（不出现"按空上下文执行一轮"）；handler 任务 panic 留下的 `running`（租约过期 + 存活判定）被回收；无法确认旧执行结束的那条停成清单里的一条 |

存储的基本约束（§8.5 的写入顺序、§7.2 的硬限制）从第二阶段开始贯穿全部工具，不能在所有功能完成后才补。

首个端到端验收：启动 komo 自动拉起 Gateway，通过模型调用 write 生成工作文件，在任务的后续步骤前重启 Gateway；不打开 CLI 也应自动接续原 Run，随后 resume 原会话查看结果，再读出原文件。随后验证一次 bash 调用在 started 之后、结果落盘之前被杀：重启后不自动重跑，中断结果连同已经打出来的部分输出交给模型，由它核对之后决定（§8.6）。

恢复故障注入验收：

| 强制中断位置                                         | 预期结果                                                         |
| ---------------------------------------------------- | ---------------------------------------------------------------- |
| Run 已提交，内存队列尚未收到通知                     | 启动或周期扫描找到原 Run，自动执行一次                           |
| 输入已持久保存且 queued 已提交，客户端确认响应丢失   | 客户端用同一请求键重发，仍返回原 Run                             |
| LLM 输出尚未收齐                                     | 重试模型请求；未收齐的调用从未执行                               |
| 完整计划已同步到 JSONL，第一个工具尚未 started       | 自动执行原计划，调用 ID 不变                                     |
| started 已提交，真实动作还没发出                     | 不能从 started 单独推断已执行或未执行：unsafe 的不重跑，中断结果交给模型；safe 的带原计划重跑一次 |
| Memos 等外部写入成功，完整输出与结果事件尚未持久保存 | 借助可靠幂等或关联信息核对；没有证据则记 uncertain 交给模型判断，执行器不产生第二条记录 |
| 输出文件和 JSONL 引用已持久保存，state.db 尚未更新     | 校验输出后补结果索引，复用原输出，工具不重做                     |
| output.json 已完成但 JSONL 结果事件尚未写入          | 校验身份、计划及完成状态后补记结果；不能只凭文件存在判断         |
| 输出文件引用已提交，但正文缺失或被修改               | 停止受影响任务，报告引用损坏，不通过重跑补造旧结果               |
| 控制决定（如 `session.closing`）已提交，审计 JSONL 尚未补写 | 从 state.db 读取原决定，outbox 幂等补写；旧库里残留的 `approval.*` 审计行不卡住补写器 |
| JSONL 最后半行未被任何提交引用                       | 隔离尾部后修复；按最后完整记录恢复，未知副作用仍按 §8.6 交给模型 |
| JSONL 已提交范围缺失或中间损坏                       | 停止受影响会话，报告损坏，不跳过记录或重做动作                   |
| 最终结果已同步到 JSONL，SSE 尚未送达                 | 客户端补读原结果，Run 保持 completed                             |
| 旧子进程仍存活，或恢复和 resume 同时触发             | 先阻止重复执行；核实进程结束，并且只有一个领取者                 |
| 用户取消后重启，或连续恢复失败耗尽预算               | 已取消的不复活；失败有明确终态，不无限循环                       |
| 逻辑删除进行到一半被杀（已进 `closing`，未完成 Run 还没处置完） | 重启后仍是 `closing`：Run 照 §8.4 走完或停在等待，不推进到 `deleted`，内容一个字节没动 |
| `purge` 进行到一半被杀（墓碑已提交、内容删了一半） | 对账把"标了 `purged` 而内容还在"的会话幂等收尾；不表现成损坏，也没有 Run 被领走 |
| 会话目录被手工删除，而状态仍是 `active` / `closing` | Run 不被领走（不按空上下文执行一轮），对账把这条不一致报成 `blocked` Intervention |

目录与引用验收：工具事件、大参数、完整输出及 stdout / stderr 都位于同一 Session 目录；改变 Gateway 当前工作目录不影响引用读取；不同 Session 或不同 attempt 不能互相覆盖。清理历史时保留未完成任务、Memory 证据和持久产物仍需引用的文件。**读路径不写**：把会话目录删掉之后，`GET /v1/sessions/{id}/events`、会话详情、对账与恢复扫描都不得把它建回来，而且对账必须把这条 Run 停成 `waiting + intervention` 并说明缺内容。

**task 进度查询验收（2026-10-19，§4，已做）**：① 任务正跑着一条 `bash`（已打出一行、还在 sleep）时，home 只给 `task_id` 调 `task`，快照里有 `正在执行：bash`、命令摘要和 `.partial` 里的输出尾部；② 查询不往任务会话提交任何输入（Run 数不变）；③ 查不存在的短号是一条落账的失败结果，开头 `查不到进度`，不建任务会话；④ 输出尾部读取：只取末尾最后几个非空行、起点落在多字节字符中间不出乱码、文件缺失或全空白是 `None`。测试：`komo-gateway` `tests/agents`（`a_status_query_shows_what_a_running_task_is_doing`、`a_status_query_for_an_unknown_task_fails_cleanly`）与 `service::tasks::tests`（尾部读取三条）；kernel 侧 `fold::progress`、`types::task` 的快照与渲染，runtime 侧 `tools::task::tests`、`executor::tests` 的状态查询。

**`task` 验收（2026-10-16，§4，已做）**：① 派出后这一轮照常往下，同一会话里用户的下一句不等任务就开跑；② 一轮派三个，三个任务会话同时在跑，各自交回一次；③ 宿主还在跑时交回的结果在一轮收尾后并入（`run.absorbed`），宿主空闲时开一条新 Run；④ `model` 填不认识的 alias / 不合法的 effort 在 `prepare` 被拒且账上有结果，填对的那条任务 Run 的快照是它，交回的那条输入是父的默认；⑤ 有 `task_id` 时省略 `model` 沿用那个任务上一条 Run 的；⑥ 带 `contract` 时交回的输入里写着复验结论，不合契约也照样交回；⑦ 任务会话里调不动 `task`；⑧ 同一请求键重放不派第二个；⑨ 旧库里停在 `waiting + dependency{子 Run}` 的父 Run 升级后照旧收尾，旧的 `delegate` / `dispatch` / `follow` 调用回放逐字节不变。Cron 触发的 Run 调 `task` 在 `prepare` 被拒（原先待定的"结果回到 Cron 会话投给谁"因此不出现）。测试：`komo-runtime` `tools::task::tests`、`executor::tests::task`（①②④⑤⑧、旧 `dispatch` / `follow` 按旧键收尾）、`executor::tests::delegation`（⑨，旧委派调用以"已落账计划"的形状交给执行器，工具已不在目录里）、`agent::tests::a_new_delegate_call_is_refused_and_the_run_carries_on`；`komo-store` coordinator 的对端规则；`komo-agent` `context::tests::a_task_contract_trails_the_task_not_the_system_prompt`；`komo-gateway` `tests/agents`（①③④⑤⑥⑦端到端，③是 `a_task_result_is_absorbed_into_a_running_home`）与 `service::tests` 的委派一组（⑨，装上测试用的旧 `delegate` 在真实 Gateway 上重走收尾）。**未做**：真机上升级一个停在 `waiting + dependency` 的旧库。

**委派续跑验收（§4，2026-10-16 随同步委派删掉，不再做）**：主对话第一轮派一条子 Run 查 A，第二轮（另一条 Run）用 `resume` 让它接着查 B——第二条子 Run 的首个模型请求里有第一条的任务正文与最后回答、**没有**它的 `tool_calls` / `provider_blocks`，也没有父对话的任何一句；结果回到第二轮那次调用上。对同一条子 Run 续跑两次，第二次被拒且理由里写着末端是哪一条；续一条 `cancelled` 的、主对话的、边界之前的、别的 Session 的，都在 `prepare` 被拒并在账上有结果（不悬着）。续跑出来的子 Run 调不动 `delegate`。重启在续跑子 Run 跑到一半时，照 §8.4 接着跑，窗口与重启前逐字节相同。

**插话验收（2026-10-15，§6）**：TUI 里一条 Run 跑着工具时打一句话——它在那一轮的结果之后、下一次请求之前并进来（`run.absorbed`），下一次请求里那句话就在那个位置、只出现一次，宿主接着答；原输入终态 `absorbed`、不再开跑，后面排着的 Run 照常放行。最终回答那一轮、宿主被取消、模型不同、渠道消息、监听事件都不并，照常排队。崩在 `run.absorbed` 已同步、库未提交之间，重启后它是 `absorbed` 而不是再跑一遍。单元测试已覆盖（`komo-store` coordinator、`komo-runtime` `agent::tests::interjection`、`komo-agent` 回放、`komo-client` TUI）；**未做真机端到端**：在真实 Gateway 上跑监听时发消息、量出那句话等了多久。

**资源命名空间验收（2026-09-22）**：`read("skill://<skill>/SKILL.md")` 与 `rg(path="skill://")`（多根 → 多条目标）端到端成立，且**审计里显示的是逻辑 URI**（`PlanTarget::describe()` 形如 `skill://review/SKILL.md（<真实路径>）`），当时读一份 skill **不产生审批**（skill 目录是只读根；审批 2026-10-11 删掉）；`tool://<工具>/schema|doc` 只在**这一次的能力面**里可读（面外写 `tool://write/schema` 在 `prepare` 就被拒、不进计划），`tool://` 是虚拟入口、没有磁盘目标；`..` / 空段 / 指向挂载点之外的链接 / 没有会话内容目录这几类同样在 `prepare` 被拒；超限正文给出的 `artifact://<run>/<call>/<attempt>/…` 与产物入口 `artifact://files/<run>/<名字>` **在 Gateway 重启之后仍读回同一份正文**；本地路径的用法与改造前逐字相同（计划里仍是 `{"path": …}`，旧计划的哈希不变）。

测试同时断言实际副作用次数、Run / ToolCall 身份、预算和事件配对；仅检查“恢复后状态变成 running”不算通过。每个断点做进程终止测试，并单独安排操作系统重启 / 存储持久性验证。

Memory 与模型验收覆盖：

- 同一偏好换一种中文表述仍可语义召回，同时保留原始来源；中文两字查询直接命中 bigram。
- 用户原话、工具观察、模型推断、用户确认在展示和上下文中均可区分；候选不因反复提取而升级。
- 切换聊天模型或 effort 不影响独立配置的记忆模型；记忆提取、冲突整理均使用自己的模型与 effort。
- 未设置 effort 不发送该字段；不支持的 effort 在请求前拒绝；模型 400 不触发静默删参重试。
- 同维度但不同模型 / 版本生成的向量不混用；修改记忆时旧 revision 不召回；重建中崩溃可以接续。
- 向量服务不可用时 hybrid 明示降级，vector-only 明确报不可用；不能把故障解释为“没有相关记忆”。
- confirm / forget 使用预期 revision，重复请求幂等；resume 和旧检查点不能重新注入已遗忘内容。
- 主动记录写入 Memos 后返回 ID / 链接，查询回到原文；删除或修改原文后更新关联摘要，写入未知时先核对。
- 在目标 NAS 与 Mac 上测量至少 1 千与 1 万条代表性记忆的索引时间、检索 P95 和内存，再决定是否需要近似向量索引；不预先宣称性能达标。

**待验证事项**——设计依赖但尚未在目标版本上核实的点。每一项在对应阶段开始前验证，结论写回本文：

| 项 | 影响 | 若不成立 |
|---|---|---|
| Turso MVCC 模式提交是否 fsync；能否读回同步设置 | §8.2 权威边界 | **已核实（2026-09-16，spikes/store.md）：提交 fsync，MVCC 写 `state.db-log`、WAL 写 `state.db-wal`，默认 `synchronous=FULL` 且可读回可改写；`synchronous=OFF` 后 strace 里 fsync 全部消失，说明不是空实现。§8.2 的 outbox-先-确认保护不启用。** 附带：`state.db-log` 与主库同等重要；`data_sync_retry` 默认 false 时 fsync 出错是 panic，连接时设成 1。未核实：真正的断电 / 内核崩溃持久性（只做了 25/25 的进程 abort） |
| toasty 0.10 是否支持带条件的 UPDATE 并返回受影响行数 | §8.7 领取代次 | **已核实（2026-09-16，spikes/store.md）：条件能表达，行数拿不到——查询目标的 `.exec()` 恒 `Ok(())`。改用 toasty 自带的 `toasty::sql::statement -> Result<u64>`，`Transaction` 上也能用，所以不需要第二个 `turso::Database` 句柄（§8.2、§13.5 已改）。并发正确性已验：220 轮 × 2–4 竞争者恰好一个赢家。附带纠正：autocommit 单语句写在 4 写入器下 200 次只成 6 次，每个写都必须进 `db.transaction()`（§8.2 已改）** |
| `reqwest` `rustls-no-provider` 下没有其他依赖重新启用 `aws-lc-rs`；`openlark` 是否把它拉回来 | §13.4 冷编预算 | **已核实（2026-09-16，骨架实测）：拉回来了**，肇事者是 `openlark-core 0.20` 显式声明 reqwest 的 `rustls` 特性（= `__rustls-aws-lc-rs`），特性可加不可减，`default-features = false` 压不住，要挡只能 `[patch]` openlark-**core** 把该特性换成 `rustls-no-provider`。代价实测：`aws-lc-sys` 构建脚本 32s 但 8 核并行、不在关键路径，墙钟只 +1.7s（68.5s → 70.2s）。**决定：接受，不维护 patch。** |
| `instr` 全表关键词臂在 1 万条时的 P95 | §9.4 | 加 `memory_terms` 的按 token 倒排表（仍是普通表，仍可重建） |
| Turso 长读事务（SSE 游标补读）与并发写提交的快照语义 | §11.1 回复订阅 | 读路径改为短事务分页 |
| `toasty-driver-turso` 能否通过 `[patch]` 关闭 `turso` 的 `fts` 默认特性且链接正常 | §13.4 | **已核实并已做（2026-09-16）**：能链接，toasty 读写照常；冷编 −7.3s、−62 编译单元。见 §13.4 依赖表 |
| iLink 消息是否带稳定 `msg_id` 可作 `request_key` | §11.1 微信去重 | **已核实（2026-09-16，wechatbot 0.4.0 源码）：没有 `msg_id`。** 逐消息的唯一候选是 `client_id`——发送方客户端生成的 UUID，`String` 非 Option。**键改为 `wechat:{from_user_id}:{client_id}`**，`(from, 内容哈希, 60s 窗口)` 降级为 `client_id` 为空时的回退。已确认两条重投来源：拉取游标不持久化（重启从空游标开始）、「回了消息却回空游标」时 SDK 原地重取。**剩余未核实（需真机登录，步骤见 `spikes/wechat.md` §5）**：① 入站 `client_id` 的形态与是否恒非空；② 空游标对服务端意味着「重发未确认」还是「从现在开始」——后者是丢消息，比重复严重，会要求把游标持久化进 state.db；③ 重投时 `client_id` 是否不变；④ 旧 `context_token` 能否跨进程使用 |
| `wechatbot` 的 native-tls 在 Fedora 上链系统 openssl 是否顺利；与 `rustls-no-provider` 的 reqwest 0.13 共存 | §13.4 | **已核实（2026-09-16，源码 + 实测）：共存没有问题，但 native-tls 这条路整个不走了。** 共存侧：native-tls 走 openssl 自己的 `Once`，与 rustls 无共享状态，`CryptoProvider::install_default()` 仍只由 `main` 调一次；原文「一份 reqwest / hyper 重复」里 hyper 是错的，锁文件里 hyper / hyper-util / rustls / hyper-rustls 各只有一份。TLS 侧：上游让 reqwest 0.12 默认特性开着且自己没有 feature，工作区无法关掉。**决定并已做：`vendor/wechatbot` + `[patch.crates-io]`，reqwest 升 0.13 走 rustls**，openssl 整条链消失，reqwest 重复也消失，默认特性全量构建通过，Fedora 不必装 `openssl-devel`。vendored openssl 作为退路而非首选（估 2–3 分钟且在关键路径起点） |
| `lark-websocket-protobuf` 是否需要 `protoc` | 构建工具链要求 | **已核实（2026-09-16）：不需要。** 0.1.2 无 `build.rs`，依赖树里没有 prost-build / tonic-build / protobuf-codegen，本机无 protoc 编译通过。安装说明不加 `protobuf` |
| 飞书卡片回调的 `event_id`、Telegram `callback_query_id` 在重推时是否保持不变 | §11.3 按钮去重（2026-10-11 审批删掉，按钮不再产生，这一行留作记录） | **已核实（2026-09-16，官方文档）**：飞书对 2.0 事件与回调的官方去重建议就是「通过 `event_id` 字段判断事件唯一性」，配合「至少发送一次」与 15s/5min/1h/6h 最多 4 次重推，等价于保证重推携带同一 `event_id`；Telegram long polling 重投的单位是整个 `Update`（`offset` 未推进即原样再取），`update_id` 与其中的 `callback_query.id` 一并不变。**两个键都稳定，组合键退路作废**；去重键只挡平台重投，用户连点由当时的 `approval_id` 幂等承担（今天是介入答复的幂等，§11.3）。剩余未核实：① 卡片回调自身的重推间隔 / 次数（官方只在「事件」侧给表）；② ws 模式下卡片回调载荷与 HTTP 是否逐字段一致；③ openlark 的 ws 客户端能否回传卡片响应帧——设计已改用 PATCH 绕开，不阻塞 |
| ws 上 `card.action.trigger` 能不能到 komo（按钮回调） | §11.3 卡片按钮（2026-10-11 审批删掉，按钮不再产生，这一行留作记录） | **源码核实（2026-09-18，`openlark-client 0.20.0/src/ws_client/frame_handler.rs:106`）：不能。** 数据帧按 header 的 `type` 分发，`"event" \| ""` 进事件分发，**`"card"` 是 `debug!("Card frame received, skipping")` 后返回 `None`——丢掉，连 ack 都不回**；`"card"` 这个分支的存在本身就说明飞书把卡片回调归成 `card` 帧。所以按钮要么等上游支持，要么 vendored openlark 自己接上（`[patch.crates-io]` 已有 wechatbot / toasty 的先例）。**未定：** 是否值得为此维护一个 patch——在那之前，答复的路是文本命令（当时是 `/approve <短ID>`、`/approve all`，今天是 `/answer`），卡片正文已写出命令；卡片回调仍需在开放平台的事件订阅里配置，否则连 `card` 帧都不会来 |
| Telegram 把消息编辑成与现状相同内容时返回的错误（官方未文档化，且声明 `error_code` 内容会变） | §11.3 决定后去掉按钮的幂等重试（2026-10-11 审批删掉，按钮不再产生，这一行留作记录） | 不匹配错误文案；编辑失败一律当非致命，决定以 Ledger 为准 |
| 补发积压投递的真实代价（启动路径） | §3 第 4 步、§11.4 | **已核实（2026-09-18，本机实测）：它就是"重启好慢"的全部。** `komo gateway restart` 9.0s：bootout + 等待 launchctl 卸完 + bootstrap 只占 0.22s（分别 6ms / 220ms / 11ms），**约 5.1s 花在渠道起来时的按平台补发、2.9s 花在启动时的整体补发**——两者都在 "Gateway 就绪" 之前同步跑，每条 pending 一个平台往返（飞书 ~290ms），而当时积压的投递因为卡片被平台拒（下一行）永远送不出去。改成就绪之后后台跑之后：**2.7s**，其中进程启动到就绪 1.8s。剩余 1.25s 是启动时的 embedding 维度探测（本地 ollama 往返），与补发无关，**已做（2026-09-18，本机 Fedora 实测）**：探测移出就绪路径（§3「就绪也不等模型探测」）——把向量端点指向一个只收连接不回话的 socket 时，修复前发现文件与 `Gateway 就绪` **都在 120.12s**（等满模型超时），修复后 **0.16s**；探测改在后台，`docs/komo_bot.md` 的那 1.25s 同样不再落在重启上。回归测试 `komo-gateway/tests/memory/startup.rs`（预修版本 5s 超时失败） |
| 飞书卡片 2.0 支持哪些组件 | §11.3 卡片渲染 | **已核实（2026-09-18，官方不兼容变更 + 线上报文）：2.0 不再支持 `note` 组件与 `action` 模块**，且 2.0 对不认识的组件是**整张卡打回**而不是忽略。线上表现（当时）：每一个审批请求都被拒（`http 400 / code 230099`，`ErrCode 200861 unsupported tag note`），**审批一条都到不了聊天里**，而失败只落在网关日志的一行 WARN 上——审批的主入口（§11.3）整个是死的，操作者只看得到"等待审批"的 Run。今天这条约束管的是 Run 回复卡片。替代写法已按官方给的来：备注 = 普通文本组件 + `notation` 字号 + 灰色；按钮行 = `column_set` 每列一个 button，那一块带固定 `element_id` 供决定后整块摘掉 |
| 飞书 Run 回复卡片的真机行为（§11.3）：`Get` 表情与 reply 接口需要的权限 scope、markdown 组件对模型最终回复的排版 | §11.3 飞书的 Run 回复卡片 | **未真机验证（2026-09-25，只有假开放平台上的测试）。** 表情或回复被平台拒时按设计退化：表情失败只记 WARN，卡片没发出去则终态照旧发文本；排版不理想时改渲染，不改投递路径 |
| 飞书话题回复的真机行为（§11.3 飞书群里的话题） | 主流消息是否开出话题、话题内消息的事件是否带 `thread_id` + `root_id`、后台通知是否落进话题 | **私聊已验证（2026-10-22，会话 01a16243）**：主线第一条开出话题 `omt_19bc2202…`；话题里第二条的事件带回 `root_id`，落到同一个 `chat:feishu:{chat}#{om_root}`；两张处理中卡片都更新成结果；后台命令跑完的结果以文本回复进同一话题；用 `GET /im/v1/messages?container_id_type=thread` 读回，话题里 5 条齐全，主线没有机器人消息。**群未验证**：群里同一套路径，只多了 @提及过滤 |
| 聊天里发图片的真机行为（§11.2 图片、§8.3 工具结果带图） | 三个渠道收到的图能不能下载下来、模型 `read` 之后看不看得到 | **未真机验证（2026-10-22，只有假服务端上的测试）**：微信、飞书、Telegram 各发一张截图（飞书再发一条「@机器人 + 截图 + 一句话」的富文本），看 `workspaces/inbox/<平台>/` 里有没有文件、Run 的第一轮是不是 `read` 那个路径、回答说没说对图里的内容。飞书要先确认应用开了 `im:resource`，没开时正文里是「没能下载：…」。富文本里 @ 的写法按 `{"tag":"at","user_id":"@_user_1"}` 处理，没拿真实事件核对——不对的话群里「@机器人 + 截图」那条会被当成没 @ 而忽略，私聊不受影响。模型侧已实测（2026-10-22，magpie `/v1/responses`，8×8 红色 PNG）：图片放在 `function_call_output` 的 `[input_text, input_image]` 里，`claude/claude-opus-5-5` 与 `codex/gpt-6.1-sol` 都答「红色」 |
| 群里访客的真机行为（§11.3 群里的访客） | 非操作者 @ 是否进 `guest:` 会话、回复是否在同一话题、模型请求里有没有工具与记忆 | **未真机验证（2026-10-22，只有假开放平台上的测试）**：需要另一个人在群里 @ 一次，看 `sessions` 里 `kind = guest` 那一行与网关日志 |
| reconcile 一拍的真实成本（目标：1 万 Run / 1 千 Session）与它该排在哪个周期 | §8.9 的周期兜底 | 从 `AUDIT_TICK` 那一拍拆出来，按更粗的间隔跑（例如 5 分钟），或只对"启动后还没对过账的那些"跑；启动时那一次无论如何都要跑 |
| `sessions.state` 之外是否还需要一个"回收进行中"的中间态 | §8.10 的 `purge` | 当前设计靠"墓碑先落、内容后删"取得幂等，不需要第四个状态；若实测发现"内容删到一半"无法与"内容被外部删掉"区分，再补一个状态列值（仍是加列/加值，不改 schema 形状） |
| **既有的 state.db 能不能加上新列**（§8.2 那句"schema 变化只增不改、`ensure_schema` 连上时补列"） | 升级路径：任何一个加了列的新版本在旧库上都起不来 | **已实测（2026-09-20，委派那两列 `runs.parent_run_id` / `runs.delegate`，本机 macOS + 真实 Turso MVCC 库）：补列会静默丢掉。** 现象：旧二进制建的库 → 新二进制启动 → 日志有 `补一列 table="runs" column="parent_run_id"` 两条 → 但同进程随后的每一条用到该列的语句都报 `Parse error: no such column: parent_run_id`（领取 SQL、待处理清单、对账、建会话全中），**重启也没用**；与此同时把 `state.db` 单独复制出来、由另一个进程打开时，同一段 `ensure_schema` 又能"补上"并打印出带新列的 DDL（所以库里没有落盘、那个进程看到的只是自己那份视图）。**新装的库完全正常**（真机端到端委派已验证），坏的只有"旧库 + 新列"这一条路。待办：查 Turso MVCC 下 DDL 的持久化语义（是否必须走非 MVCC 连接 / 是否要升级 turso），再决定补列是改成"用原始连接迁移"还是"表重建"。 **已定位并已改（2026-09-20，同日）：MVCC 连接上的 DDL 不落盘**（`Ok` + 日志照打，重开即无），所以补列改走**建池之前**——`Db::connect` 在 `toasty::Db::builder` 之前用**普通（非 MVCC）连接**把已存在的文件补到当前 schema（`migrate_file`：建缺失表 → `ALTER TABLE ADD COLUMN` → 建缺失索引），全部幂等；`ensure_schema` 退成**守卫**：文件库再缺列就**报错**（"补列必须走建池之前的迁移"），只有内存库（没有文件）还由它补。实现上还有个坑：turso 的读游标拖着一条读事务，**同一条连接上"边读边改"会 panic**在 `vdbe/execute.rs` 的 `SetCookie`（`invalid transaction state for SetCookie: TransactionState::Read, should be write`），所以那段是"一次读清 → 丢连接 → 只用一条只写连接改"。**已验证（真库副本，本机 macOS）**：副本 `sessions` 少 `state`/`state_changed_at`、`runs` 少 8 列 → `Db::connect` 之后 12/31 列齐全、另开一条连接读得回（确实落盘），按模型读 sessions 25 行（含 `origin`）、runs 正常。回归测试两条 `a_missing_column_is_added_on_reopen` / `the_delegate_columns_are_added_to_an_existing_runs_table`：**在普通连接上造旧形状、在另一条新连接上确认落盘**，原先那两条在池连接上造形状，两边都在空转——这正是真机上漏过去的原因。未核实：断电 / 内核崩溃下的持久性，以及升级 turso 后行为是否改变。 |
| 模型一轮里提了两个调用、其中一个没有结果时，会话后面的新 Run 会不会被毒住 | §8.3 回放窗口 | **已实测（2026-09-20，真实 provider）：会。** 旧二进制下模型一轮提了两个调用，一个被拒/没执行，那条 `message.assistant` 留在会话里；其后**同一会话的每一条新 Run** 首个模型请求都被 provider 400 拒（`No tool output found for tool call call_00_…`），且会连着重试。当时用"回放窗口按 Run 过滤"（`service/segment.rs` 的 `window(surface, Some(run))`）挡住了它——**那是拿对话连续性换的**（见本节末与下一行）。**已改（2026-09-21）**：窗口改回"这一段对话"，毒化由两件事挡住——① 正跑的那条 Run 之外的消息**不带协议**（历史 Run 只发布用户正文与它最后答的正文），"有调用、没结果"的半轮因此进不了新请求；② `fail_call` 与按引用恢复让每一次调用都真的有个结果（下面那句）。回归测试：`service::segment::tests::a_later_run_still_reads_what_the_earlier_one_said`（历史那轮的 `tool_calls` / `provider_blocks` 一条都不出现）、`the_running_run_keeps_its_whole_protocol`（正跑的那条一个都不能少）。**已修（2026-09-20，本机真实会话复盘）**：线上那次 400 的调用是两次 `delegate`——两个漏法都补上了。①`resumed()` 重建调用时不按引用读回外置的 `arguments` / `plan`（§8.3 原话要求"读取历史或恢复调用时按引用加载需要的内容"）：委派的任务正文 6 KB 被外置，重 `prepare` 拿到的是一份 `null` 参数，当场失败。②`execute_one` 的"未知工具 / prepare 失败 / 放行被拒 / 子代理不能再委派"四条分支把结论**只交给模型、不写账本**（`Ledger::fail_call` 就是补这一笔）：那次调用于是永远悬着，父的窗口里两个 `function_call` 一前一后，provider 报的是前面那个。回归测试：`service::segment::tests::an_externalized_argument_and_plan_come_back_by_reference`、`service::tests::a_delegated_task_over_the_inline_limit_still_settles_the_parent_call`（去掉任一处的回填就失败）、`service::tests::a_call_that_cannot_run_still_gets_a_result_in_the_ledger`（去掉 `fail_call` 就失败）。 |
| 超限的**正文**（`run.accepted.text` / `message.assistant.text` 过 4 KiB）在回放窗口里是不是也需要按引用读回 | §8.3 的"读取历史或恢复调用时按引用加载" | **已核实（2026-09-20，真实会话）**：会外置（本次会话里两条子 Run 的 `run.accepted` 与一条 `message.assistant` 都是 `text: null` + `text_ref`），而当时全仓**没有任何一处**读 `text_ref`——`replay()` 交给模型的用户消息因此是空的（没炸只是因为子代理的任务正文同时也在它的系统提示里，正常 Run 的用户输入很少过 4 KiB）。**已做（2026-09-21）**：`replay()` 现在按引用读回正文（`segment.rs` 的 `message_text`：内联优先、`text_ref` 次之，哈希由 `PayloadStore::open` 校验，读不出来或不是 UTF-8 都算会话缺内容 → 停下来报告而不是发一条空消息）。回归测试：`service::segment::tests::an_externalized_message_comes_back_by_reference`（用户输入与模型回复两条路）、`a_payload_that_cannot_be_read_stops_the_segment`。 |
| 内嵌 `grep` + `ignore` 之后冷编还在不在 60s 预算内 | §13.4 的编译预算 | **已实测（2026-09-21，本机 macOS M5，`cargo build --timings`、空 target 目录）：加之前 1m10s（`/tmp` 里 HEAD 的干净 worktree），加之后 1m10s——差值落在噪声里。** 新带进来的 13 个包（`globset` / `walkdir` / `termcolor` / `crossbeam-deque` / `memmap2` / `bstr` / `encoding_rs_io` 等）与 `turso_core` / `aws-lc-sys` 并行，不在关键路径上。**顺带发现：60s 这个预算当时就已经超了**（§13.4 记的 2026-09-16 是 64.2s），与这次改动无关——要么把预算调到实测值，要么回去找关键路径，待办。 |
| 工具结果的正文该给模型多少、由谁渲染 | §8.3 的投影与预算 | **已做（2026-09-21）**：`[execution] model_result_bytes`（默认 50 KiB，2000 行上限，按 RunSnapshot 冻结）+ `komo-kernel/src/projection.rs` 一处纯函数；事实只有落盘的（事件 + `output.json` 里的 `body.preview`），所以"刚跑完"与"回放"逐字节相同（`komo-gateway/tests/observation` 里有断言）。**未核实**：50 KiB / 2000 行默认值对真实任务是否最优——要等真实会话的 recall 次数（§49 的指标）出来再调。**已补（2026-09-21）**：那四个数现在有得数了——每份观察落盘多少字节（`tool_output_bytes`）、产物多少（`artifact_bytes`）、投影给模型多少（`projected_bytes`），以及"模型回头 `read` 我们落盘的观察"的次数（`observation_recall_count`，判据是路径落在本 Session 的 `tool-output/` 或 `artifacts/` 底下）。四者都只进 trace（`komo::observation`），不进事件流（§47）；抓取与断言见 `komo-runtime/src/executor/tests.rs` 里那两条（`the_projection_reports_how_many_bytes_it_kept_and_stored`、`reading_back_an_observation_counts_as_a_recall`）。**已接（2026-09-24）**：§6 同句里的"活动执行时限"进了配置——`[execution] call_timeout_secs`（默认 1800，热生效，走 `CallEnv` / `ToolContext`）。起因是一次真实会话：模型给 `cargo test --workspace && cargo build --release` 传了 `timeout_secs: 1800`，执行器写死的 300s 先到点，shell 判成 `uncertain`，Run 停下等人核对。现在 shell 的超时取 `min(请求, 上限)`，由工具自己杀进程组报 `Timeout`；执行器的兜底计时器在上限之外多等 15s（进程组 TERM/KILL 回收各 5s），不再与工具抢同一个时刻。回归测试：`tools::shell::tests::a_requested_timeout_beyond_the_call_limit_is_clamped_to_it`、`config::tests::the_call_timeout_is_read_from_the_execution_section`。2026-10-11 那条 `python` 沿用 `call_timeout` 的改动随 `python` 工具于 2026-10-15 删除，对应的两条回归测试一并删掉；`bash` 的时限仍取 `call_timeout`（上一句）。 |
| `read` 一次读回来的正文够不够模型用 | §4 与 §8.3 | **已做（2026-09-21）**：`read` 交给模型的那一段从 400 字符改到 8 KiB（`read::PREVIEW_BYTES`），投影再按预算收。**未核实**：真实仓库里"读一个文件要几次 `read`"——如果还是很多次，说明该按结构切（一次给整段函数）而不是按字节。 |
| 记忆召回的重排值不值（一次判断请求换来的排序） | §9.4 的可选重排 | **已做（2026-09-21）**：`[typesafe]` + `memory.retrieval.rerank` / `rerank_shortlist`——短名单（比 `top_k` 宽，校验拦「一样宽」的组合）交给一条 `Choice`，按回来的概率表排序，再照旧按 `top_k` 截；**只重排不增删**，挑中 `none` 或后端不可用都保持融合顺序。**已实测（2026-09-21，本机真机，4 条候选 / 中文输入）**：`jev-1.13.0` 一次 0.6s、约 650 输入 token，顺序 `[m-4, m-1, m-2, m-3]`——热水器与空调那两条排到了前面。离线验收在 `komo-runtime/src/memory/tests.rs`（抬高本来排不进的条目、`none` 不采纳、后端坏了照常召回、开关关着不发请求），真机那条是 `memory/rerank.rs` 里的 `mod live`（`--ignored`，要 `TYPESAFE_API_KEY`）。**未核实**：真实规模（成百上千条）下短名单取多宽、这一次请求值不值——按真实会话的命中率调，别凭感觉改 |
| 系统提示里的 skill 目录行够不够用（166 个 skill 的机器） | §5.6 的目录行 | **已实测（2026-09-21，本机真实语料 + `komo skills list`）：不够，而且坏在两处。** ① `description: >` / `|` 这些 YAML 块标量没有解析——取值是字面量 `>`，325 份 `SKILL.md` 里 111 份的描述是这么写的，目录行长成 `- log-diagnosis：>`；② 2000 字符的预算按目录序 `continue` 丢弃，166 个不同名字里只剩 16 条进提示。真实会话里模型的第一批调用是读 `cart-loong-diff` / `ask-user`（正是那 16 条里的两条），找 `log-diagnosis` 靠的是 `ls` 整个目录，多花了好几轮。**已做（2026-09-21）**：frontmatter 支持块标量（`>` 折行 / `|` 字面，含 chomping，描述里的冒号不再被当成键）；目录改成**整批**定形状——描述装得下就是"名字 + 一句描述"，装不下就只留名字（166 条名字 2752 字符），名字都装不下时末尾写"另有 N 条没列出来"；默认上限 2000 → 4000。回归测试：`frontmatter::tests::a_folded_description_is_joined_into_one_sentence`、`skills::tests::every_skill_keeps_its_name_when_the_descriptions_do_not_fit`、`a_folded_description_becomes_one_catalog_line`。**未做**：按当前输入排序的 top-K——排序块不能进 system 前缀（§9.4 的前缀缓存），先看补全之后还错不错。 |
| 在线压缩在真实会话上多久触发一次、省下多少（§6 的 `[compaction]`） | 默认值（`keep_recent_tokens` 20000、`cache_write_read_ratio` 12.5、冷却 2 次）与"默认关"能不能翻成默认开 | **未验证（2026-10-09 落地，默认关）。** 要看的数都在 `context.compacted` 里：每次的 `decision`（`archive_tokens`、`post_compaction_tokens`、`breakeven_requests`、`reason`）、`skipped` 的理由分布、摘要请求的 `usage`；对照同一会话之后几次请求的 `cache_read` / `cache_write`（`message.assistant` 逐轮记着）算实际回本。触发太频繁就调大冷却或 `keep_recent_tokens`；2026-10-16 起只按窗口触发（`update_plan` 退役），几乎不触发就看 `max_context_tokens` 是不是定得太高。**缓存读数在主力模型上拿得到（2026-10-11 实测）**：DeepSeek 的 Responses（`input_tokens_details.cached_tokens`）与 Chat Completions（`prompt_tokens_details.cached_tokens`，另有 `prompt_cache_hit_tokens` 同值）都报，同一前缀第二次请求命中 1280/1479，现有解析两条路都读得到；10-09 之前的会话没有这一格只是因为字段那天才加 |
| 在用的 provider 超长时报什么、溢出恢复认不认得出 | §6 的溢出恢复（`is_context_overflow`） | **未验证（2026-10-21 落地）。** 模式表照搬 pi，只用单测里的样例文本核对过；magpie、DeepSeek、opencode relay、Codex ChatGPT 后端真实超长时的状态码与措辞没有实测。认不出时 Run 照旧失败、不退避重试（只有 5xx 包装的超长会被白白退避）；看日志里失败 Run 的 `run.failed` 理由，有漏网的措辞就补进表里。另外要看救回来之后压一次够不够：`context.compacted` 里 `context_overflow: true` 之后紧跟一次失败，说明 `keep_recent_tokens` 留的近期段本身就超窗口 |
| 各 provider 的 `/models` 报不报上下文窗口、字段叫什么 | §13.3 的窗口来源、§6 的窗口保护 | **已实测（2026-10-15，本机 curl + `komo config check`）**：magpie（`127.0.0.1:3425/v1`）每条都带 `max_input_tokens` / `context_window` / `context_length` / `max_output_tokens`（`claude/claude-opus-5-5` 1000000、`codex/gpt-6.1-sol` 500000、`codex/gpt-6-sol` 272000）；DeepSeek 带 `context_window`（`deepseek-flash` 1048576）；opencode relay 只回 `id`，得靠配置。Codex ChatGPT 后端回 `models[].slug` + `context_window`（`gpt-6-sol` 272000、`gpt-6.1-sol` 500000），但**按 `minimal_client_version` 过滤**——`client_version=0.8.0` 只回 `{"models":[]}`，所以探测固定报 `99.0.0`；装机后 `komo config check` 实测七个 alias：deepseek / gpt6-sol / magpie-gpt-6.1-sol 来自 provider，relay 三个未知。**未核实**：Anthropic 原生 `/v1/models` 是否带 `max_input_tokens`、默认分页（20 条）会不会漏掉目标模型 |
| 字节 / 4 的 token 估计对中日韩正文偏多少 | §6 的估价与窗口保护（`estimate_tokens`） | **未验证。** 一个汉字 3 字节、估 0.75 token，实际常是 1–2 个——中文为主的会话估低，窗口保护会晚到。窗口判断已经与实报的 `input` 取大，残余风险是"这一轮新加的中文结果"；偏得多就按字符类别分别估，或改用 provider 的计数接口 |
| Anthropic 修订工具结果时补的第 4 个缓存断点实际命中率 | §8.3 的衰减与提示缓存 | **Responses 后端已有实测（2026-10-10，会话 01a12475-…，magpie、claude-opus-5-5）**：首版按次数逐条改写，10 轮 run 里 `cache_read` 四次退回 13317（只剩系统提示 + 工具表），全价输入 85k（约 53k 是失效重写），折算成本与 Claude Code 持平——自动前缀缓存下改写中间一条就是整段失效，由此改成缓存优先（§8.3）。**Anthropic 的未验证。** 补在被改结果之前那条 user 消息末块——它正是更早某次请求的末尾断点；命中要看 `cache_read` 是否覆盖到那里。命中不了就退回"系统提示 + 工具表"两级，衰减省下的读取要重新算账 |
| 工作目录里的项目规范（`AGENTS.md` / `CLAUDE.md`）进不进提示、怎么找 | §4.3 冻结、`docs/agent.md` §10 的提示顺序 | **已做（2026-10-13），真实会话未验证。** 起因：pi 会话 01a132e5 在 komo-bot 目录里启动，第一轮就带着 `AGENTS.md`；komo 会话 01a11fda 的工作目录是 `workspaces/`，前 3 轮花在找源码、之后手动 `cat AGENTS.md`。规则照 pi 的 `loadProjectContextFiles`：从工作目录一直往上走到 `/`（不停在 git 根或家目录），每层按 `AGENTS.override.md` → `AGENTS.md` → `AGENTS.MD` → `CLAUDE.md` → `CLAUDE.MD` 取第一个读得出来的普通文件，外层在前、提示里说明冲突以靠后的为准；不加全局文件（Profile 的 `instructions` 就是那一份），不做 git worktree 去重。每条 Run 受理时找一次，正文冻结进 `payloads/`（`RunSnapshot.project_files_ref`），续跑按引用读回、读不出来按会话缺内容停下；Cron 与旧行没有快照，兜底身份现场找。只进主 Agent 的提示（komo 自查之后、Skills 之前）；子代理继承父快照但不渲染。**没有大小上限**（pi 也没有）：大文件整份进每一段的提示前缀，改了文件下一条 Run 的前缀缓存从这一块起失效——真实会话的 token 代价待看。测试：`service::project_files::tests`、`context::prompt::tests` 的两条 project_files、e2e `project_files_are_frozen_at_acceptance_like_the_profile`。**改为会话级沿用（2026-10-14）**：起因是 komo 会话 01a13a9e 里 agent 改了 vault 的 `AGENTS.md`，之后两条 Run 的系统提示前缀从这一块起失效，缓存读从 11.7K 掉到 7.7K。现在同一会话（同一 Agent、同一工作目录）的新 Run 沿用上一条主 Run 的 `project_files_ref`，系统提示逐字不变；受理时现场找到的不一样就另存为 `RunSnapshot.project_files_update_ref`，回放时补在这条 Run 的任务消息后面（`prompt::project_update_block`，全删了也明说）。做在 Gateway 的冻结与装配两层，三个 provider 适配器不动。e2e 同名测试改为断言三次请求的系统提示逐字相同、新版只出现在新 Run 的任务后面 |
| 在 Gateway 自己的 `bash` 里 `komo gateway restart` / `stop` 推迟执行（§3） | TUI 的 `!` 与模型发起的重启不再卡住、不再把那条 Run 停在等人上 | **单元测试已过，真机未验证（2026-10-13）。** launchd 一支要在 Mac 上装新二进制后跑一次 `!komo gateway restart`：命令立刻返回、Run 正常结束、约 1.5 秒后 Gateway 换了实例、TUI 说"事件订阅已恢复"，`logs/gateway-restart.log` 里是"Gateway 已重启"。systemd 一支（`systemd-run --user --on-active=2`）要在 Fedora 上同样跑一次；`systemd-run` 起不来时命令直接报错，不会退回同步重启 |
| 监听（`monitor`）在 Linux 与真实会话上的行为（§6.1「监听」） | `docs/monitor.md` §5 的停机 / 遗留进程组回收；事件经 home 转告到聊天的打扰程度 | **macOS 上测试已过，Linux 与真实会话未验证（2026-10-14）。** 四条端到端测试（`komo-gateway/tests/monitor`）只在 macOS 上跑过，那里没有 bwrap。Linux 上要核实两件：bwrap 的 `--die-with-parent` 让 Gateway 崩溃时脚本随之退出（遗留进程组那一支走不到）；systemd `stop` 时进程组在 `Running::stop` 里先被终止、登记留给下一代。真实会话要看：盯一条 MR 流水线时每条事件的转告是否太吵——太吵就加"这条不用说"的出口，而不只靠脚本过滤 |
