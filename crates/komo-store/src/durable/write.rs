//! 一次提交里可以出现的写，以及每种写对应的那一条 SQL。
//!
//! 字段先用 `i64` ID + JSON 文本：D1 的记录类型落地后在 `Storage` 实现里 serde 成
//! `record` 字符串，这里不必知道它们的形状。写入**已经准备好**——线只接收数据，不接收
//! future，所以"提交里 await 外部东西"在形状上就做不到（§3.1）。

use turso::{Connection, Value};

/// 提交的类别，决定落盘档位（§3.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitKind {
    /// entry、任务状态、submission、session……`synchronous=FULL`，返回即已 fsync。
    Structural,
    /// 只替换 document（节流的进度）。`synchronous=OFF`：不 fsync，下一次结构性提交
    /// 的 fsync 连带落盘；掉电最多丢到上一次结构性提交之后的进度。只允许 [`Write::ReplaceDocument`]。
    Progress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    InsertConversation {
        id: i64,
        owner_conversation: Option<i64>,
        owner_task: Option<i64>,
        parent_conversation: Option<i64>,
        parent_at: Option<i64>,
        session_id: Option<String>,
        record: String,
    },
    InsertEntry {
        id: i64,
        conversation_id: i64,
        head: Option<i64>,
        record: String,
    },
    /// 新建或整行替换（任务状态机每一步都是整条记录的新版本）。
    UpsertTask {
        id: i64,
        conversation_id: i64,
        kind: String,
        status: String,
        abort_requested: bool,
        background: bool,
        record: String,
    },
    InsertSubmission {
        id: i64,
        conversation_id: i64,
        request_id: Option<String>,
        status: String,
        record: String,
    },
    /// 必须命中一行，否则整次提交失败。
    UpdateSubmission {
        id: i64,
        status: String,
        record: String,
    },
    CreateDocument {
        id: i64,
        kind: String,
        scope_kind: String,
        owner_id: i64,
        key_value: String,
        value: String,
    },
    /// 整值替换，`version` 加一；文档必须存在且未退役，否则整次提交失败。
    ReplaceDocument {
        id: i64,
        value: String,
    },
    RetireDocument {
        id: i64,
    },
    UpsertSession {
        id: String,
        lifecycle: String,
        current_branch: Option<i64>,
        root_conversation: i64,
        record: String,
    },
}

/// 提交前的检查；不通过就是调用方的错，线不受影响。
pub(crate) fn validate(kind: CommitKind, writes: &[Write]) -> Result<(), String> {
    if writes.is_empty() {
        return Err("空提交".into());
    }
    if kind == CommitKind::Progress
        && let Some(other) = writes
            .iter()
            .find(|w| !matches!(w, Write::ReplaceDocument { .. }))
    {
        return Err(format!(
            "Progress 提交只允许替换 document，收到 {}",
            other.name()
        ));
    }
    Ok(())
}

fn int(v: i64) -> Value {
    Value::Integer(v)
}

fn opt(v: Option<i64>) -> Value {
    v.map_or(Value::Null, Value::Integer)
}

fn text(v: &str) -> Value {
    Value::Text(v.to_string())
}

fn opt_text(v: &Option<String>) -> Value {
    v.as_deref().map_or(Value::Null, text)
}

impl Write {
    fn name(&self) -> &'static str {
        match self {
            Write::InsertConversation { .. } => "InsertConversation",
            Write::InsertEntry { .. } => "InsertEntry",
            Write::UpsertTask { .. } => "UpsertTask",
            Write::InsertSubmission { .. } => "InsertSubmission",
            Write::UpdateSubmission { .. } => "UpdateSubmission",
            Write::CreateDocument { .. } => "CreateDocument",
            Write::ReplaceDocument { .. } => "ReplaceDocument",
            Write::RetireDocument { .. } => "RetireDocument",
            Write::UpsertSession { .. } => "UpsertSession",
        }
    }

    /// 在已开的事务里执行这一条写。`seq` 是这次提交的序号。
    pub(crate) async fn apply(&self, conn: &Connection, seq: i64) -> Result<(), String> {
        let (sql, params, must_hit): (&str, Vec<Value>, bool) = match self {
            Write::InsertConversation {
                id,
                owner_conversation,
                owner_task,
                parent_conversation,
                parent_at,
                session_id,
                record,
            } => (
                "INSERT INTO conversations (id, owner_conversation, owner_task, parent_conversation, parent_at, session_id, commit_seq, record) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                vec![
                    int(*id),
                    opt(*owner_conversation),
                    opt(*owner_task),
                    opt(*parent_conversation),
                    opt(*parent_at),
                    opt_text(session_id),
                    int(seq),
                    text(record),
                ],
                false,
            ),
            Write::InsertEntry {
                id,
                conversation_id,
                head,
                record,
            } => (
                "INSERT INTO entries (id, conversation_id, head, commit_seq, record) VALUES (?1, ?2, ?3, ?4, ?5)",
                vec![
                    int(*id),
                    int(*conversation_id),
                    opt(*head),
                    int(seq),
                    text(record),
                ],
                false,
            ),
            Write::UpsertTask {
                id,
                conversation_id,
                kind,
                status,
                abort_requested,
                background,
                record,
            } => (
                "INSERT INTO tasks (id, conversation_id, kind, status, abort_requested, background, record) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id, kind = excluded.kind, status = excluded.status, \
                 abort_requested = excluded.abort_requested, background = excluded.background, record = excluded.record",
                vec![
                    int(*id),
                    int(*conversation_id),
                    text(kind),
                    text(status),
                    int(*abort_requested as i64),
                    int(*background as i64),
                    text(record),
                ],
                false,
            ),
            Write::InsertSubmission {
                id,
                conversation_id,
                request_id,
                status,
                record,
            } => (
                "INSERT INTO submissions (id, conversation_id, request_id, status, record) VALUES (?1, ?2, ?3, ?4, ?5)",
                vec![
                    int(*id),
                    int(*conversation_id),
                    opt_text(request_id),
                    text(status),
                    text(record),
                ],
                false,
            ),
            Write::UpdateSubmission { id, status, record } => (
                "UPDATE submissions SET status = ?2, record = ?3 WHERE id = ?1",
                vec![int(*id), text(status), text(record)],
                true,
            ),
            Write::CreateDocument {
                id,
                kind,
                scope_kind,
                owner_id,
                key_value,
                value,
            } => (
                "INSERT INTO documents (id, kind, scope_kind, owner_id, key_value, version, created_seq, retired_seq, value) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, NULL, ?7)",
                vec![
                    int(*id),
                    text(kind),
                    text(scope_kind),
                    int(*owner_id),
                    text(key_value),
                    int(seq),
                    text(value),
                ],
                false,
            ),
            Write::ReplaceDocument { id, value } => (
                "UPDATE documents SET value = ?2, version = version + 1 WHERE id = ?1 AND retired_seq IS NULL",
                vec![int(*id), text(value)],
                true,
            ),
            Write::RetireDocument { id } => (
                "UPDATE documents SET retired_seq = ?2 WHERE id = ?1 AND retired_seq IS NULL",
                vec![int(*id), int(seq)],
                true,
            ),
            Write::UpsertSession {
                id,
                lifecycle,
                current_branch,
                root_conversation,
                record,
            } => (
                "INSERT INTO sessions (id, lifecycle, current_branch, root_conversation, record) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET lifecycle = excluded.lifecycle, current_branch = excluded.current_branch, \
                 root_conversation = excluded.root_conversation, record = excluded.record",
                vec![
                    text(id),
                    text(lifecycle),
                    opt(*current_branch),
                    int(*root_conversation),
                    text(record),
                ],
                false,
            ),
        };
        let changed = conn
            .execute(sql, params)
            .await
            .map_err(|e| format!("{}：{e}", self.name()))?;
        if must_hit && changed == 0 {
            return Err(format!("{} 没有命中任何行", self.name()));
        }
        Ok(())
    }
}
