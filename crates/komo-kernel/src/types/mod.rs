//! 值类型：ID、状态机、执行计划、渠道身份、模型角色、记忆数据模型。
//!
//! 这里没有 I/O，没有时钟，没有 tokio（§13.4）。需要"现在几点"的地方一律由调用方
//! 从 [`crate::traits::Clock`] 取一个 `OffsetDateTime` 传进来。

pub mod chat;
pub mod digest;
pub mod ids;
pub mod memory;
pub mod model;
pub mod plan;
pub mod refs;
pub mod status;
pub mod tool;
pub mod turn;

pub use chat::{
    ApprovalPresentation, ApprovalScope, ChannelPeer, ChannelPlatform, Delivery, DeliveryState,
    DeliveryTarget, Outbound, PeerId, Principal,
};
pub use digest::{ContentHash, sha256};
pub use ids::{
    ApprovalId, AttemptId, CronJobId, DeliveryId, EventId, ExecutorId, GrantId, IdParseError,
    MemoryId, OperationId, RequestKey, RunId, Seq, SessionId, ShortId, ToolCallId, uuid_v7_at,
};
pub use memory::{
    Confirmation, Evidence, EvidenceRef, ExtractionMetadata, MemoryItem, MemoryKind, MemoryScope,
    MemoryState, MemoryUsage, MemoryWork, Provenance, RecallQuery, RecallResult, RetrievalMode,
    SupersededRef,
};
pub use model::{
    CatalogModel, DistanceRule, Effort, EffortSetting, EmbeddingConfig, EmbeddingSpace, InputKind,
    ModelCatalog, ModelConfig, ModelRole, ModelType, TokenUsage, Vector,
};
pub use plan::{
    ApprovedPlan, ConsumeIntent, ConsumedApproval, EnvVersion, ExecutionPlan, Operation, PlanHash,
    PlanSource, PlanTarget, PlanVersions, Proof, RecoveryMode, ResourceRef, SourceKind,
    TargetAccess, Verification,
};
pub use refs::{
    AttemptRef, ContentRef, INLINE_ARGUMENT_LIMIT_BYTES, OutputRef, PREVIEW_LIMIT_BYTES,
    PayloadRef, PublishedOutput, ToolResultBody, ToolResultStatus, VerifiedOutput,
};
pub use status::{
    AttemptState, Claimed, FinalEventRef, RetryCause, RunEnd, RunState, SessionState,
    ToolCallState, WaitReason,
};
pub use tool::{
    CancelToken, PyError, PythonJob, PythonResult, ResumedCall, ToolContext, ToolDefinition,
    ToolError, ToolOutput, WorkspaceRoot,
};
pub use turn::{
    AcceptInput, Accepted, AssistantRound, EventBatch, GrantUse, LlmError, MemoryUse, PlannedCall,
    ProviderToolCall, ReplayMessage, Role, Round, RoundInput, SeqRange, ToolCallRequest,
    ToolResultForModel, TurnRequest,
};
