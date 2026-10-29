# 目标与领域

> 状态：规格，2026-10-29 认可（来自 [`roadmap-muse.md`](roadmap-muse.md) P1 与 §4.2）。分片实现：P1a 数据与命令、P1b 注入、P1c 记忆归属与归档、P1d 提取、P1e 挂触发器，各节标明属于哪一片与是否已实现。
> 记忆一侧的改动（归属字段、`archived` 状态、注入分层）写在 [`memory.md`](memory.md) §9.2、§9.4、§9.6，这里只引用。不带文件名的章节号指 [`komo_bot.md`](komo_bot.md)。

## 1. 为什么单独成对象

记忆回答"知道什么"，目标与领域回答"在做什么、在管什么"。记忆的种类（偏好 / 事实 / 经验）和使用方式（约定 / 线索）都表达不了"正在做、会结束的事"：做完的事留下的要求一直被注入，只能人工 forget；主动提醒也说不出"为谁、为什么盯"。

借 PARA 与 GTD（`roadmap-muse.md` §1「组织原则」）：

| 概念 | komo | 生命周期 |
|---|---|---|
| Project | **目标**（goal）：有结果、会结束，可带截止日期 | 进行中 / 等待中 / 搁置 → 完成 / 放弃 |
| Area | **领域**（area）：没有终点、要维持标准的职责 | 在管 → 不再管 |
| GTD 下一步行动 | 目标的 `next`：进行中的目标此刻该做的那一件事 | 随推进改写 |
| GTD 等待中 | 目标的 `waiting` 状态与 `waiting_for`：在等谁、等什么 | 等到了回到进行中 |
| GTD 将来 / 也许 | 目标的 `parked`（搁置） | 回顾时拿出来问 |

两种事项一张表，`kind` 区分。下文"事项"（matter）指两者。

## 2. 已定的取舍（2026-10-29）

- **单独成对象**（`roadmap-muse.md` §4.2）：多一张表、两组命令，记忆多一个归属字段和一个 `archived` 状态。不做成记忆的第三种使用方式：事项有状态机和下一步，记忆没有。
- **不按渠道或会话归属。** 事项属于操作者本人，任何会话（飞书、TUI、Cron）看到的是同一份清单。事项没有作用域字段。
- **给别人用的 Agent 看不到也写不了。** Profile 写 `memory = "private"` 或 `memory = "off"` 的会话不注入事项，`komo goal` / `komo area` 带着这种会话调用时被拒（与 §9.2 `agent:<id>` 的理由相同：别人的话不进操作者的清单，操作者的清单也不成为别人的注入通道）。
- **截止日期过了不自动结案。** 超期是读出来的（未结案、`due` 早于今天），不是状态：注入行和清单标"已超期"，每周回顾（P2）拿出来问。自动转"过期"等于替用户放弃一件事。
- **领域与记忆作用域不合并。** 作用域说"谁看得到、在哪适用"（§9.2），归属说"为哪件事记的"（`memory.md` §9.2 归属）。"家里的设备"这个领域下的记忆可以在 `environment:nas` 作用域里，两个字段各管各的，领域不挂作用域。
- **事项由 Agent 在 Run 里当场记，提取不建事项。** 用户说"开始做 / 在等 / 先放一放 / 办完了"时，Agent 就在这个 Run 里跑命令，用户在场、上下文齐全。后台提取不产生事项建议：离线建议需要一个确认回路，等每周回顾（P2）有了再考虑。提取只给新记忆建议归属（P1d）。

## 3. 数据模型（P1a，已实现）

`aux.db` 新表 `matters`（toasty，与 cron、memory 同一套：DDL 常量 + 对齐测试、写在 `with_write_retry` 的事务里、UUIDv7 字符串主键）。kernel 值类型 `Matter`。

| 字段 | 含义 |
|---|---|
| id / revision | `MatterId`（UUIDv7）与递增版本；任何会进注入行的字段变了都升一版（注入按 `id@revision` 去重与失效，P1b） |
| kind | `goal` / `area` |
| title | 一句话，能独立看懂（"这周把 owl 的发布观察做完"）；≤ 80 字，不像凭证（同记忆的闸，§9.3） |
| state | goal：`active` / `waiting` / `parked` / `done` / `dropped`；area：`active` / `retired` |
| next | 只给 goal：下一步，一句话；可空 |
| waiting_for | 只给 goal：在等谁、等什么；`waiting` 时必填，离开 `waiting` 时清空 |
| due | 只给 goal：截止日期（日期，不带时刻）；可空 |
| standard | 只给 area：要维持的标准（"每周一前完成告警值班轮换"）；可空 |
| evidence | 改承诺的那几次操作的用户原话证据，同记忆的 `Evidence`（`EvidenceRef::Event`），追加不改 |
| created_at / updated_at / closed_at | `closed_at` 在进 `done` / `dropped` / `retired` 时写，重开时清空 |

后续片追加的列（附加式，不改已有列）：通知偏好（P2）、挂的触发器（P1e）。

**超期**：`kind = goal`、state 是 `active` / `waiting` / `parked`、`due` 早于 Gateway 本地时区的今天。

## 4. 状态与操作（P1a，已实现）

```text
goal:  active ⇄ waiting
          ⇅        ↘
        parked  →  done / dropped  →（reopen）active
area:  active → retired →（reopen）active
```

| 命令 | 作用 | 允许的起点 |
|---|---|---|
| `komo goal add "标题" [--next …] [--due YYYY-MM-DD]` | 新建，`active` | — |
| `komo goal next ID "下一步"` | 改下一步 | active / waiting |
| `komo goal wait ID "在等什么"` | 转 `waiting`，写 `waiting_for` | active / waiting（改等的东西） |
| `komo goal resume ID [--next …]` | 回 `active` | waiting / parked |
| `komo goal park ID` | 搁置 | active / waiting |
| `komo goal done ID` / `drop ID` | 结案 | active / waiting / parked |
| `komo goal reopen ID` | 重开回 `active` | done / dropped |
| `komo goal edit ID [--title …] [--due …\|--no-due]` | 改标题、截止日期 | 未结案 |
| `komo goal list [--all]` / `show ID` | 清单 / 详情 | — |
| `komo area add "名称" [--standard …]` | 新建，`active` | — |
| `komo area edit ID [--title …] [--standard …]` | 改名称、标准 | active |
| `komo area retire ID` / `reopen ID` | 不再管 / 重开 | active / retired |
| `komo area list [--all]` / `show ID` | 清单 / 详情 | — |

- `list` 默认只列未结案的，`--all` 带上结案的。一条一行：`id@版本 状态 标题`，goal 另带下一步或在等什么、截止日期与"已超期"，area 另带标准。
- 状态不允许的操作报错并说明当前状态，不静默忽略。状态没变（对 `parked` 再 park）返回原样、不升版。
- 所有改动都可带 `--revision N`（预期版本，不符报冲突）；不带就按当前版本改。事项由 Agent 频繁推进，每次都要先查版本只会多一轮，冲突的代价也小（改的是一行清单）。
- 经 Gateway：`GET/POST /v1/matters`、`GET /v1/matters/{id}`、`POST /v1/matters/{id}/{action}`（action 即上表动词）。CLI 不开 state（§3）。

**谁能改什么。** 按改的是不是"承诺"分：

| 操作 | 改的是 | Run 里（带 `KOMO_SESSION` / `KOMO_RUN`） | 终端里 |
|---|---|---|---|
| add、done、drop、park、retire、reopen、edit | 用户要做 / 要管什么 | 要有这个 Run 里用户说的话，作为证据追加（规则同 `memory add`，§9.3）；一句都没有就拒绝 | 直接改，不带证据 |
| next、wait、resume | 推进的进度 | 不要求用户原话：推进是 Agent 自己的工作（它提了工单、转等待） | 直接改 |

这和 `memory add` 一样不是安全边界（bash 去掉两个变量就走终端那条路，§7 没有 Policy），分的是常态下谁在说话。后台任务会话（`kind = task`）与 Cron 的 Run 里改承诺类操作被拒：那里没有用户原话。

**系统提示。** `SELF_BLOCK` 加一句静态文字：用户说开始做某事、开始管某事、在等某人某事、先放一放、办完了、不管了，就当场用 `komo goal` / `komo area` 记下；推进时更新下一步。

**实现与验收（2026-10-29）。** 接缝：kernel `types::matter`（`Matter` / `MatterKind` / `MatterState` / `MatterVerb`、纯函数 `transition` 与 `Matter::is_overdue`）、`traits::MatterRepo`、`protocol::http::matter`（`MatterListQuery` / `MatterAddRequest` / `MatterActionRequest` / `MatterDetail` / `MatterListResponse`）；store `models::matter`（aux.db `matters` 表，DDL 常量与对齐测试、每列有补列子句）与 `TursoMatterRepo`（带预期版本的写在同一事务里读、比、写）；runtime `matter::MatterManager`（文字规整、按调用方要证据、读改写）；gateway `http::matters`（四条路由、private / off 闸、今天）；client `matters` / `matter` / `matter_add` / `matter_act` 与 `render::matter_*`；bin `matters.rs`（`komo goal` / `komo area`）；`SELF_BLOCK` 加一句。用户原话的口径从 `memory add` 抽成 `memory::run_user_evidence`，两边共用。

规格没写、实现时定下的：
- **已在目标状态上不算起点不对。** 对 `parked` 再 park、对 `done` 再 done、对 `active` 的目标 resume / reopen 都是空操作（原样返回、不升版、不追加证据）；只有"既不是允许的起点、也不在目标状态上"才报错。active 上 `resume --next` 只改下一步。
- **状态不允许报 409**（`conflict`，消息写出当前状态与允许的起点）；种类不对、字段不对报 400；预期版本不符报 409 `version_conflict`；private / off 会话报 403 `denied`，读写都拦。
- **可空字段给空串 = 清空**：`komo goal next ID ""` 清下一步，`komo area edit --standard ""` 清标准；标题与 `waiting_for` 不许空。所有文字去首尾空白、不许换行（清单与注入都是一行一条）、标题 ≤ 80 字、其余 ≤ 300 字（同记忆的 `MAX_CONTENT_CHARS`）、不像凭证。
- **动词只认自己的字段**，带了别的报错（`park` 带 `title`）；`--due` 与 `--no-due` 互斥。CLI 在每次改动里带上 `kind`，拿领域的 id 跑 `komo goal edit` 报错而不是悄悄改了领域。
- **今天由 Gateway 给**：每个响应带 `today`（系统时区的日历日，换算不出来退回 UTC 日期），客户端按它读超期。
- **证据只在真改了时追加**（按引用去重）；状态没变的承诺类操作仍要先过原话这一关。Cron 的 Run 不单列：提示词不算用户原话，自然被拒。
- 清单按创建先后（id）排，整表读出在内存里筛；不开索引。

测试：kernel `every_goal_transition_follows_the_table`、`every_area_transition_follows_the_table`、`a_verb_of_the_other_kind_is_refused`、`leaving_waiting_clears_what_it_waited_for_and_closing_stamps_closed_at`、`resume_can_carry_a_new_next_step`、`an_unchanged_result_is_a_no_op`、`kind_specific_fields_are_refused_on_the_wrong_kind`、`overdue_is_read_from_the_due_date_and_today`、`a_matter_round_trips_through_json_with_an_iso_due_date`、`verbs_parse_and_split_into_commitment_and_progress`、`an_add_request_carries_the_due_date_as_iso_text`；store `a_matter_round_trips_through_the_row`、`a_stale_revision_writes_nothing`、`the_list_hides_closed_matters_unless_asked`（DDL 对齐与可补列由 `models::tests` 对全部表覆盖）；runtime `an_add_inside_a_run_cites_the_users_words`、`a_commitment_without_user_words_is_refused_but_progress_is_not`、`a_background_task_session_cannot_change_a_commitment`、`malformed_requests_are_refused`、`an_unchanged_state_returns_the_matter_as_is`；gateway 端到端（放在 `tests/memory`，少链接一个二进制）`matters::a_goal_walks_its_whole_life_over_http`（add → next → wait → resume → done → reopen，含 409 与版本冲突）、`an_area_retires_and_reopens_and_refuses_goal_verbs`、`a_run_without_user_words_can_progress_but_not_commit`、`a_private_or_off_session_cannot_see_or_change_matters`，以及 `http::matters::tests::today_is_the_local_calendar_date`；client `a_matter_list_is_one_line_per_matter`、`a_matter_detail_shows_every_field_and_the_overdue_mark`；bin `goal_and_area_commands_parse`；agent `the_self_block_teaches_recording_goals_and_areas_and_stays_byte_stable`。`cargo fmt --check` 通过；`cargo test --workspace --no-fail-fast`：2443 通过、0 失败、12 忽略。增量 `cargo build --timings`：35.33 秒，报告 `target/cargo-timings/cargo-timing-20261029T024754257Z-f55de822e3512fa2.html`。未验证：没装二进制、没在真 Gateway 上跑 CLI，Agent 是否按 `SELF_BLOCK` 那句当场记没有真模型验证；端到端用例里的 Run 原话走的是脚本化模型，不经真 bash 的 `KOMO_SESSION` / `KOMO_RUN`（那一段与 `memory add` 共用，见 `memory.md` §9.3 的 Durable Run 身份验收）。

## 5. 注入（P1b）

每个 Run 的召回块（`memory.md` §9.4「按 Run 召回」）多一组，排在约定组前面：

> 你在做和在管的事（用户的目标与领域；用来理解当前请求的背景，不是要你主动去推进的任务）：
> - 目标：这周把 owl 的发布观察做完〔下一步：看 AB 组的 5xx · 截止 11-01 · …@3〕
> - 目标（等待中）：owl 发布工单〔在等：SRE 审批 · …@2〕
> - 领域：告警值班轮换〔标准：每周一前完成 · …@1〕

- **带哪些**：goal 的 `active`、`waiting`，area 的 `active`。`parked` 与结案的不带。
- **机制同约定组**：不过门槛、每段全量，减去本段已展示的 `id@revision`；revision 变了或不该再带（搁置、结案）算失效，按 `komo.memory.revoke` 改写旧块。
- **预算**：单独一份 `[memory.retrieval] matter_tokens`，默认 600；超出的按更新时间从旧到新截掉，诊断记"事项超出预算 N 条"。截掉的不回落到任何检索路：清单本来就该短，超了是该整理了。
- **抬头的分寸**：清单是背景，不授权 Agent 自己去推进某件事——用户在问别的，Agent 不该转头去做清单上的事。
- 不带的会话：`memory = off`、`memory = private`、fork 沿用父的块（同约定）。
- 召回块的 entry 结构、`data.uses` 不变，事项与记忆的 `id@revision` 共用一个去重集合（两种 id 不会撞）。

## 6. 记忆归属与结案归档（P1c）

规格在 `memory.md`：§9.2 归属字段 `belongs_to`、`archived` 状态，§9.4 注入分层与 `push_unattached`，§9.6 结案时线索归档、约定问一次。这里只列与事项相接的两处：

- `komo goal done/drop`、`komo area retire` 返回时带上：转为 `archived` 的线索条数，以及挂在它下面、仍在生效的约定清单（`id@版本 正文 条件`）。Agent 把约定清单转给用户问去向；去向由操作者定（`komo memory classify --for <事项|none>` 或 `forget`），没答之前照常注入。
- `komo goal reopen`、`komo area reopen` 把因它结案而归档的线索恢复成 `active`（只恢复因结案归档的，见 `memory.md` §9.6）。

## 7. 提取（P1d）

提取窗口之外多给一张"进行中的事项"表（同注入组的清单，带编号，规则同 §9.9 的候选编号）。模型对每条新观察可交回一个事项编号作为归属建议；Rust 校验编号在表里，落库时写进 `belongs_to`。不交回 = 无归属。已有记忆的归属不由提取改：那是 `classify`（操作者动作，§9.6）。提示词升版。

存量：现有记忆归属为空，行为不变（`push_unattached` 默认 `true`）。挂归属走一次性清单：Agent 按事项列出建议，操作者审过后批量 `classify --for`（同 2026-10-28 那次存量分类）。

## 8. 挂触发器（P1e）

事项可挂若干 Cron Job（`CronJobId`）与 monitor（会话 + monitor id），记在事项上，`komo goal link/unlink`、`komo area link/unlink`。`wait` 时 Agent 通常顺手挂一个 monitor（盯审批、盯回复）；等到了，monitor 交回会话，Agent `resume`。事项结案时提示它挂着的触发器，不自动删（触发器可能另有用途）。通知判断（触发器跑完说不说）是 P2，在这之前触发器照它自己的 `notify` 投递。

## 9. 完成标准

照 `roadmap-muse.md` P1 的完成标准，逐片能验的：

- P1a：`komo goal` / `komo area` 全部命令经 HTTP 往返；状态机不允许的操作报错；Run 里没有用户原话时改承诺类操作被拒、推进类照常；private / off 会话被拒；`cargo test --workspace` 通过。
- P1b：任何会话问"我在管哪些事、在做哪些事"答得出来；同段第二个 Run 不重复带；park / done 之后旧块被改写；改了下一步，下一个 Run 带上新版本。
- P1c：结案后它的线索不再注入、`--include-archived` 查得到，约定被列出来问；`push_unattached = false` 时问无关问题不带无归属的线索。
- P1d：做某件事时提取出的线索挂到这个目标下。
- P1e：说"工单提了，等审批"，目标转 `waiting` 并挂上 monitor，审批通过后回到 `active`。
