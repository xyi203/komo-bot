//! Agent 层：身份、能力与上下文装配（`docs/bot.md` §二、§四、`docs/komo_bot.md` §13.4，
//! `docs/agent.md` 是这一层最新那次收口的设计）。
//!
//! 只依赖 `komo-kernel`。它是完整的 **Agent Context Boundary**：回答"这次运行是谁、能用
//! 什么、这一刻模型应该看到什么"，不回答"这件事怎样受控地发生"——后者（授权、进程、
//! 落盘、恢复核对、记忆检索）在 `komo-runtime`，而 `runtime` **不依赖这个 crate**。两边都
//! 在 `komo-gateway` 组装起来：Gateway 收集事实（`ContextInput`），这里纯装配
//! （`AgentContext`）。
//!
//! ```text
//! surface   Profile 在工具目录里挑出来的能力面（§4 末）+ 编排操作名
//! skills    SkillRegistry：人写的 SKILL.md 的发现与目录行（§5.6、§14）
//! context   ContextInput → AgentContext：系统提示、回放消息与记忆段的唯一装配入口（§5、§8、§10、§13）
//! ```

pub mod context;
pub mod skills;
pub mod surface;

pub use surface::{DELEGATE_TOOL, surface_of};
