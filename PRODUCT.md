# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

作者本人是首要用户（personal agent，单操作者/host operator 形态），设计决策以其偏好与真实使用场景为准。同时公开发布是认真的：面向愿意自己配 API key、跑 gateway 的技术型自托管用户，界面与流程不能对陌生新用户不友好。

## Product Purpose

komo 是一个 Rust 个人 agent 框架：一个二进制提供交互式 LLM 聊天、本地工具、长期记忆、目标与领域、定时 Job（prompt 或直跑命令），以及一个 always-on gateway 承载聊天渠道（飞书 / Telegram / 微信，独立插件进程）和主动后台工作。gateway 是唯一持有状态的进程，TUI、CLI 与 Web 客户端都是它的 HTTP / SSE 客户端（macOS 与手机客户端在计划中，见 `docs/clients.md`）。所有状态本地存于 `~/.komo`。

成功的定义（用户确认）：**长期记忆越用越懂我** —— 记忆系统随时间积累出真实价值，agent 越来越了解它的主人，这是与其他 agent 框架的根本差异。日常依赖与工程品质服务于这个目标。

## Positioning

以「记忆随时间积累」为核心机制的个人 agent：按会话自动提取记忆（候选 → 生效 → 有争议 / 遗忘 / 归档的生命周期），约定每段都带、线索按关键词与向量混合召回过门槛才带，离线整理合并重复，采纳计数记下哪些记忆真的被用上，记忆可归属到目标或领域、结案即归档（`docs/memory.md`、`docs/goals.md`）；配合本地优先（一切数据在 `~/.komo`）、单二进制、always-on gateway。邻近产品（通用 chatbot、无状态 agent 框架）无法如实复制这一「越用越懂你」的主张。

## Operating Context

- 主形态是聊天助理：终端 TUI（`komo`）、Web 客户端（`apps/client`，见 `docs/web.md`）、以及飞书 / Telegram / 微信渠道内的对话。
- gateway 作为常驻进程运行（macOS launchd / Linux systemd 托管），承担定时 Job、monitor、后台任务会话与记忆提取；主动消息经 home chat 送达。
- 操作者通过 CLI 或 Web 管理记忆、目标与领域、技能、cron、会话与配置，全部走 gateway 的 `/v1/*` HTTP 接口。没有审批与权限策略（2026-10-11 删除）；唯一的例外是触发器起的 Run 调用声明为对外的工具时，停在 confirm 检查点等操作者 approve / reject（`docs/confirm.md`），在 Web 的待处理页或渠道里答。
- 单机单用户：数据库、配置、凭证都在本地 `~/.komo` 下。

## Capabilities and Constraints

- 前端：`apps/client` 是 TanStack Start（SPA 模式）+ Tailwind v4 + Radix，由 Gateway 同源托管，只走 gateway 的 HTTP / SSE；颜色只引用 `src/styles.css` `@theme` 里的语义 token。C1 起 W0 原型 `apps/web` 已删除。之后包成 Electron 与 PWA（`docs/clients.md`）。
- 多 LLM provider（OpenAI / Anthropic 协议及兼容端点，如 DeepSeek、Codex），在设置里按 provider 管理对话与向量模型，模型菜单按会话切换；无 key 也能启动（回复配置指引）。
- 领域术语以 `docs/komo_bot.md`、`docs/memory.md`、`docs/goals.md` 为准（Session、Run、约定、线索、目标、领域、任务会话、插话、待处理等），界面文案应与之一致。
- 平台判定为 web：电脑上用 Web 客户端（以后 macOS 用 Electron 包同一套网页），手机先用 PWA、再按需上 Capacitor（`docs/clients.md`）；在那之前手机走飞书等渠道。

## Brand Commitments

**绑定约束（用户确认，照此执行）**，原出处 `README.md` Brand 段（随 v0.8 脚手架 `ddaaabeb` 删除，见 git 历史 `5e506989`）：

- 名称 **Komo**，源自日语 *komorebi*（木漏れ日）：树叶间洒落的阳光——温暖清晰，小片刻积累成恒久之物，呼应记忆随时间积累的产品核心。
- 视觉语言：软绿、米白、阳光黄；dappled-light（叶隙光斑）形状意象。
- 人格：树荫下安静的朋友——温暖而不打扰，专注倾听，记得被托付的细节。
- 候选 slogan：「记住每一缕光」/「陪你把日子攒成光」/ *Light through your days*。
- 资产：`komo_logo.png`（吉祥物 + 字标 + slogan）当前不在仓库里，只在 git 历史（`5e506989`）。Web 原型侧栏用的是内联 SVG 标记。

## Evidence on Hand

- 真实可运行的产品：CLI、TUI、gateway、三个聊天渠道与 Web 原型均已实现；`README.md` 的功能描述有代码背书。桌面与手机客户端尚未开始。
- 无测评、无用户案例、无第三方评价——未来对外表述不得虚构这些。

## Product Principles

1. **记忆是产品的心脏**：功能与界面取舍优先服务「越用越懂我」——让记忆的积累、回忆与巩固可见、可信、可管理。
2. **作者优先，新人不受阻**：以首要用户的真实工作流为准绳做深，同时首次上手路径（init → 配 key → chat）必须对陌生自托管用户顺畅。
3. **本地与安静**：数据留在本地，主动性有节制——温暖而不打扰是人格也是行为准则（只有触发器起的对外动作才停下来问、渠道的安静行为都体现这一点）。
4. **一个世界，多个入口**：TUI、Web（以后的桌面与手机）、聊天渠道是同一个 agent 的不同入口；术语、行为与人格跨入口一致。

## Accessibility & Inclusion

无产品级特殊要求（未确立专门标准）；界面文案中英双语并存是现状（CLI/文档英文为主，品牌与用户沟通含中文）。Web 原型只有中文；新版客户端计划从一开始支持中文与英文，语言偏好存在各端（`docs/clients.md` D9）。
