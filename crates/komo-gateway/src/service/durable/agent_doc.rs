//! `komo.agent` document（conversation 级，gateway 写，`docs/storage-durable-d5.md` §2）。
//!
//! 一个对话 conversation 用哪个 Agent、哪个模型、什么工作目录——钩子 `setup` 每次现读它。
//! 会话内分支 fork 时**同一次提交**把它复制给子 conversation（决定 10）；任务子 conversation
//! 没有它，钩子沿 `komo.task.parent` 回退到父的。

use komo_kernel::durable::{ConversationId, DocumentAddress, DocumentScope, Storage};
use komo_runtime::durable::{DurableError, Tx};
use serde::{Deserialize, Serialize};

/// document 的种类名。
pub const AGENT_DOC_KIND: &str = "komo.agent";
pub const AGENT_DOC_VERSION: u32 = 1;

/// `{ agent, model?, effort?, workdir?, maxRounds?, source? }`。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDoc {
    /// Agent 的 ID（`[agents.<id>]`）。
    pub agent: String,
    /// 模型 alias；按**当前**配置解析。没有 = Agent 的默认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    /// 来源（origin，如 `chat:telegram:123`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl AgentDoc {
    pub fn new(agent: impl Into<String>) -> Self {
        AgentDoc {
            agent: agent.into(),
            ..Default::default()
        }
    }
}

pub fn address(conversation: ConversationId) -> DocumentAddress {
    DocumentAddress {
        kind: AGENT_DOC_KIND.to_owned(),
        scope: DocumentScope::Conversation { conversation },
        key: None,
    }
}

fn decode(value: serde_json::Value) -> Result<AgentDoc, DurableError> {
    serde_json::from_value(value)
        .map_err(|e| DurableError::rejected(format!("komo.agent is malformed: {e}")))
}

/// 已提交的 `komo.agent`（读不经过线）。
pub async fn read(
    storage: &dyn Storage,
    conversation: ConversationId,
) -> Result<Option<AgentDoc>, DurableError> {
    match storage.find_document(&address(conversation)).await? {
        None => Ok(None),
        Some(record) => decode(record.value).map(Some),
    }
}

/// 提交里读（看得到本事务自己的写）。
pub async fn read_in(
    tx: &mut Tx,
    conversation: ConversationId,
) -> Result<Option<AgentDoc>, DurableError> {
    match tx.document(&address(conversation)).await? {
        None => Ok(None),
        Some((_, value)) => decode(value).map(Some),
    }
}

/// 提交里整值写（没有就建，有就替换）。
pub async fn write_in(
    tx: &mut Tx,
    conversation: ConversationId,
    doc: &AgentDoc,
) -> Result<(), DurableError> {
    let value = serde_json::to_value(doc)
        .map_err(|e| DurableError::rejected(format!("komo.agent cannot be encoded: {e}")))?;
    tx.put_document(address(conversation), AGENT_DOC_VERSION, value)
        .await
}

/// 会话内分支 fork：把 `from` 的 `komo.agent` 复制给 `to`（同一次提交里调）。`from` 没有就什么都不做。
pub async fn copy_in(
    tx: &mut Tx,
    from: ConversationId,
    to: ConversationId,
) -> Result<(), DurableError> {
    if let Some(doc) = read_in(tx, from).await? {
        write_in(tx, to, &doc).await?;
    }
    Ok(())
}
