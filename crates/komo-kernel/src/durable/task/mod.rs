//! 通用 Task 引擎的纯逻辑（pi-durable spec §5；设计 §12.4）。
//!
//! 这里只有判定：状态转换合法吗、一个阶段返回后该停还是继续、开库时 `running` 怎么回
//! `pending`、`waiting` 何时可恢复、`completing` 何时转 terminal、等待环、所有权遍历。
//! 输入是已提交的记录，输出是决定；写盘、定时、线程都在 `komo-runtime` 的调度器里。

pub mod graph;
pub mod reconcile;
pub mod resolve;
pub mod state;
pub mod step;

pub use graph::{Node, Overlay, TaskGraph, TaskNode, Walked};
pub use reconcile::{
    AbortMark, abort_marks, fail_fast_targets, finalizable, reconcile_on_open, waiting_on,
};
pub use resolve::{BlockedReason, DefinitionInfo, Fit, can_reserve, fit};
pub use state::{
    NextTaskState, RunningTask, StateError, cancellation_intent, check_checkpoint,
    check_next_state, check_replacement, checkpoint_phase, failed_outcome, is_live,
    legal_transition, memo_of, put_memo, with_abort_mark, with_state,
};
pub use step::{InvocationMode, PhaseReturn, Replacement, StepDecision, StepInput, decide_step};
