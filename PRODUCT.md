# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

作者本人是首要用户（personal agent，单操作者/host operator 形态），设计决策以其偏好与真实使用场景为准。同时公开发布是认真的：面向愿意自己配 API key、跑 gateway 的技术型自托管用户，界面与流程不能对陌生新用户不友好。

## Product Purpose

komo 是一个 Rust 个人 agent 框架：一个二进制提供交互式 LLM 聊天、本地工具、长期记忆、定时 routine（命令 / agent / 消息三种动作），以及一个 always-on gateway 承载聊天渠道（飞书/Telegram/微信）和主动后台工作。gateway 是唯一持有状态的进程，TUI、CLI、桌面与 Web 客户端都是它的 HTTP 客户端。所有状态本地存于 `~/.komo`。

成功的定义（用户确认）：**长期记忆越用越懂我** —— 记忆系统随时间积累出真实价值，agent 越来越了解它的主人，这是与其他 agent 框架的根本差异。日常依赖与工程品质服务于这个目标。

## Positioning

以「记忆随时间积累」为核心机制的个人 agent：三层记忆面（MEMORY.md / memory tool / hybrid recall）+ 夜间 dream 巩固 + 使用信号驱动的候选晋升，配合本地优先（一切数据在 `~/.komo`）、单二进制、always-on gateway。邻近产品（通用 chatbot、无状态 agent 框架）无法如实复制这一「越用越懂你」的主张。

## Operating Context

- 主形态是聊天助理：终端 TUI（`komo chat`）、Electron 桌面壳与 Web SPA（共享 React 渲染层）、以及飞书/Telegram/微信/HA 等渠道内的对话。
- gateway 作为常驻进程运行（macOS launchd 托管），承担 review / routine / dream 三个 sweep 与主动输出；主动消息经 home chat 送达。
- 操作者通过 CLI 管理记忆、技能、cron、run ledger、权限策略等（统一走 gateway 的 `POST /api/operator`）；副作用工具走审批流（chat 内 `/approve`）。
- 单机单用户：数据库、配置、凭证都在本地 `~/.komo` 下。

## Capabilities and Constraints

- 前端：bun workspace（`apps/`），`apps/app` 为共享 React 渲染层，由 Electron（`apps/desktop`）与 Web SPA（`apps/web`）挂载；shadcn 组件 + 语义主题 token，`bun run lint` 禁止裸色值；react-query 管服务端状态、zustand 管客户端状态；对 gateway 仅走 HTTP（`HttpKomoClient`）。
- 多 LLM provider（DeepSeek/Anthropic/OpenAI/Codex/OpenRouter），模型菜单按会话切换；无 key 也能启动（回复配置指引）。
- 领域术语（`CONTEXT.md` 是术语表）：Turn / Tool Note / Turn Trace / Run Ledger / History Window / Cancel 等，界面文案应与之一致。
- 平台判定为 web：Electron 桌面壳复用同一 web 渲染层，不引入原生设计语言。

## Brand Commitments

**绑定约束（用户确认，照此执行）**，出处 `README.md` Brand 段：

- 名称 **Komo**，源自日语 *komorebi*（木漏れ日）：树叶间洒落的阳光——温暖清晰，小片刻积累成恒久之物，呼应记忆随时间积累的产品核心。
- 视觉语言：软绿、米白、阳光黄；dappled-light（叶隙光斑）形状意象。
- 人格：树荫下安静的朋友——温暖而不打扰，专注倾听，记得被托付的细节。
- 候选 slogan：「记住每一缕光」/「陪你把日子攒成光」/ *Light through your days*。
- 现有资产：`docs/images/komo_logo.png`（吉祥物 + 字标 + slogan）。

## Evidence on Hand

- 真实可运行的产品：CLI、TUI、gateway、桌面/Web 客户端均已实现；`README.md` 的功能描述有代码背书。
- 无测评、无用户案例、无第三方评价——未来对外表述不得虚构这些。

## Product Principles

1. **记忆是产品的心脏**：功能与界面取舍优先服务「越用越懂我」——让记忆的积累、回忆与巩固可见、可信、可管理。
2. **作者优先，新人不受阻**：以首要用户的真实工作流为准绳做深，同时首次上手路径（init → 配 key → chat）必须对陌生自托管用户顺畅。
3. **本地与安静**：数据留在本地，主动性有节制——温暖而不打扰是人格也是行为准则（审批流、quiet 渠道行为都体现这一点）。
4. **一个世界，多个入口**：TUI、桌面、Web、聊天渠道是同一个 agent 的不同入口；术语、行为与人格跨入口一致。

## Accessibility & Inclusion

无产品级特殊要求（未确立专门标准）；界面文案中英双语并存是现状（CLI/文档英文为主，品牌与用户沟通含中文）。
