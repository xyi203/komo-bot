//! Agent 层：身份、能力与上下文装配（`docs/bot.md` §二、§四、`docs/komo_bot.md` §13.4）。
//!
//! 只依赖 `komo-kernel`。它回答"这次运行是谁、能用什么、提示里放什么"，不回答
//! "这件事怎样受控地发生"——后者（授权、进程、落盘、恢复核对）在 `komo-runtime`，
//! 而 `runtime` **不依赖这个 crate**。两边都在 `komo-gateway` 组装起来。
//!
//! ```text
//! surface   Profile 在工具目录里挑出来的能力面（§4 末）+ 编排操作名
//! skills    SkillRegistry：人写的 SKILL.md 的发现与目录行（§5.6）
//! ```

pub mod skills;
pub mod surface;

pub use surface::{DELEGATE_TOOL, surface_of};
