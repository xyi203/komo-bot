//! 一次提交里可以出现的写，以及每种写对应的 SQL 与事务内校验。
//!
//! 字段是 `i64` ID + JSON 文本：`Storage` 实现（[`super::storage`]）把 D1 的记录类型 serde
//! 成 `record` 字符串再交给线，这里不必知道它们的形状。写入**已经准备好**——线只接收数据，
//! 不接收 future，所以"提交里 await 外部东西"在形状上就做不到（§3.1）。
//!
//! # 校验失败与存储故障的界线
//!
//! 能在事务里**通过读**判出来的失败（ID 已被另一种实体占用、实体不可变却重写、目标不存在、
//! request_id 已被占用、document 已退役或地址上已有活的……）是 [`ApplyError::Rejected`]：
//! 线 `ROLLBACK`、保持健康、不占 seq。其余——引擎或 I/O 返回的错误——是
//! [`ApplyError::Engine`]，让整条线失败（spec §10.1）。不靠解析引擎的错误文本来分类：
//! 约束冲突都在写之前读出来判。
//!
//! # 一批里的应用顺序
//!
//! document 的几条规则与批内次序无关（同批"先内容后退役"、"先退役旧的再建新的"），
//! 而 SQL 是顺序执行的，所以 [`ordered`] 把一批排成：
//! 内容替换 → 退役既有 document → 其余写（含建 document）。
//! 同批建了又退役的 document 直接以"已退役"的样子落库（它从来没有当前值，也不参与地址冲突）。

use std::collections::HashSet;

use turso::{Connection, Value};

/// 提交的类别，决定落盘档位（§3.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitKind {
    /// entry、任务状态、submission、session……`synchronous=FULL`，返回即已 fsync。
    Structural,
    /// 只替换 document 的值（节流的进度）。`synchronous=OFF`：不 fsync，下一次结构性提交
    /// 的 fsync 连带落盘；掉电最多丢到上一次结构性提交之后的进度。只允许 [`Write::ReplaceDocument`]。
    Progress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// 新建，之后不可变。
    InsertConversation {
        id: i64,
        owner_conversation: Option<i64>,
        owner_task: Option<i64>,
        parent_conversation: Option<i64>,
        parent_at: Option<i64>,
        session_id: Option<String>,
        record: String,
    },
    /// 新建，之后不可变。
    InsertEntry {
        id: i64,
        conversation_id: i64,
        head: Option<i64>,
        record: String,
    },
    /// 新建或整行替换。
    UpsertTask {
        id: i64,
        conversation_id: i64,
        kind: String,
        status: String,
        abort_requested: bool,
        background: bool,
        record: String,
    },
    /// 新建或整行替换；`request_id` 在 conversation 内唯一。
    UpsertSubmission {
        id: i64,
        conversation_id: i64,
        request_id: Option<String>,
        status: String,
        record: String,
    },
    /// 地址上不能已有活的 document。`key_value` 由调用方编码（`None` 与 `Some("")` 要分得开）。
    CreateDocument {
        id: i64,
        kind: String,
        scope_kind: String,
        owner_id: i64,
        key_value: String,
        version: i64,
        value: String,
    },
    /// 整值替换（连同定义版本）；document 必须存在且未退役。
    ReplaceDocument {
        id: i64,
        version: i64,
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

/// 事务内应用一条写的失败。见模块文档。
#[derive(Debug)]
pub(crate) enum ApplyError {
    Rejected(String),
    Engine(String),
}

fn engine(what: &str, error: turso::Error) -> ApplyError {
    ApplyError::Engine(format!("{what}：{error}"))
}

fn rejected(message: impl Into<String>) -> ApplyError {
    ApplyError::Rejected(message.into())
}

impl Write {
    fn name(&self) -> &'static str {
        match self {
            Write::InsertConversation { .. } => "InsertConversation",
            Write::InsertEntry { .. } => "InsertEntry",
            Write::UpsertTask { .. } => "UpsertTask",
            Write::UpsertSubmission { .. } => "UpsertSubmission",
            Write::CreateDocument { .. } => "CreateDocument",
            Write::ReplaceDocument { .. } => "ReplaceDocument",
            Write::RetireDocument { .. } => "RetireDocument",
            Write::UpsertSession { .. } => "UpsertSession",
        }
    }

    /// 这条写落下的数字实体 ID（取号器要越过它）；session 与对既有 document 的改动没有。
    pub(crate) fn minted_id(&self) -> Option<i64> {
        match self {
            Write::InsertConversation { id, .. }
            | Write::InsertEntry { id, .. }
            | Write::UpsertTask { id, .. }
            | Write::UpsertSubmission { id, .. }
            | Write::CreateDocument { id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// 提交前的检查（不读库）；不通过就是调用方的错，线不受影响。
pub(crate) fn validate(kind: CommitKind, writes: &[Write]) -> Result<(), String> {
    if kind == CommitKind::Progress {
        if writes.is_empty() {
            return Err("空的 Progress 提交".into());
        }
        if let Some(other) = writes
            .iter()
            .find(|w| !matches!(w, Write::ReplaceDocument { .. }))
        {
            return Err(format!(
                "Progress 提交只允许替换 document，收到 {}",
                other.name()
            ));
        }
    }
    // 同一批对一个 document：至多一条内容命令（建 / 换）、至多一次退役。
    let mut content: HashSet<i64> = HashSet::new();
    let mut retired: HashSet<i64> = HashSet::new();
    for write in writes {
        if let Some(id) = write.minted_id()
            && id < 1
        {
            return Err(format!("ID {id} 越界"));
        }
        match write {
            Write::CreateDocument { id, .. } | Write::ReplaceDocument { id, .. }
                if !content.insert(*id) =>
            {
                return Err(format!("document {id} 同一批里有不止一条内容命令"));
            }
            Write::RetireDocument { id } if !retired.insert(*id) => {
                return Err(format!("document {id} 同一批里退役了不止一次"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// 一批的应用顺序与批级信息。
pub(crate) struct Batch<'a> {
    pub ordered: Vec<&'a Write>,
    /// 同批建了又退役的 document：落库即已退役。
    ephemeral: HashSet<i64>,
}

pub(crate) fn ordered(writes: &[Write]) -> Batch<'_> {
    let created: HashSet<i64> = writes
        .iter()
        .filter_map(|w| match w {
            Write::CreateDocument { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    let ephemeral: HashSet<i64> = writes
        .iter()
        .filter_map(|w| match w {
            Write::RetireDocument { id } if created.contains(id) => Some(*id),
            _ => None,
        })
        .collect();
    let phase = |w: &Write| match w {
        Write::ReplaceDocument { .. } => 0,
        Write::RetireDocument { id } if !created.contains(id) => 1,
        // 同批建的 document 的退役已经并进了建（`ephemeral`），这里不再有动作。
        Write::RetireDocument { .. } => 3,
        _ => 2,
    };
    let mut ordered: Vec<&Write> = writes.iter().collect();
    ordered.sort_by_key(|w| phase(w)); // 稳定排序：同一阶段内保持调用方的次序
    Batch { ordered, ephemeral }
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

/// 取出全部行（游标在函数内收干净，不拖着读事务去做后面的写）。
async fn rows(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Vec<Vec<Value>>, ApplyError> {
    let mut cursor = conn.query(sql, params).await.map_err(|e| engine(sql, e))?;
    let mut out = Vec::new();
    while let Some(row) = cursor.next().await.map_err(|e| engine(sql, e))? {
        let mut cols = Vec::with_capacity(row.column_count());
        for i in 0..row.column_count() {
            cols.push(row.get_value(i).map_err(|e| engine(sql, e))?);
        }
        out.push(cols);
    }
    Ok(out)
}

/// 数字 ID 现在属于哪张实体表（全局一个命名空间）。
async fn owner_of(conn: &Connection, id: i64) -> Result<Option<&'static str>, ApplyError> {
    let found = rows(
        conn,
        "SELECT 'conversation' FROM conversations WHERE id = ?1 \
         UNION ALL SELECT 'entry' FROM entries WHERE id = ?1 \
         UNION ALL SELECT 'task' FROM tasks WHERE id = ?1 \
         UNION ALL SELECT 'submission' FROM submissions WHERE id = ?1 \
         UNION ALL SELECT 'document' FROM documents WHERE id = ?1",
        vec![int(id)],
    )
    .await?;
    Ok(match found.first().and_then(|r| r.first()) {
        Some(Value::Text(t)) => Some(match t.as_str() {
            "conversation" => "conversation",
            "entry" => "entry",
            "task" => "task",
            "submission" => "submission",
            _ => "document",
        }),
        _ => None,
    })
}

/// 新建一个实体前：ID 必须没人用，或者（可替换的实体）已经属于同一种。
async fn claim(
    conn: &Connection,
    id: i64,
    table: &'static str,
    replaceable: bool,
) -> Result<(), ApplyError> {
    match owner_of(conn, id).await? {
        Some(existing) if !replaceable || existing != table => Err(rejected(format!(
            "ID {id} 已属于 {existing}，不能再写成 {table}"
        ))),
        _ => Ok(()),
    }
}

/// `None` 不存在；`Some(None)` 活着；`Some(Some(seq))` 已退役。
async fn document_state(conn: &Connection, id: i64) -> Result<Option<Option<i64>>, ApplyError> {
    let found = rows(
        conn,
        "SELECT retired_seq FROM documents WHERE id = ?1",
        vec![int(id)],
    )
    .await?;
    Ok(found.first().map(|row| match row.first() {
        Some(Value::Integer(seq)) => Some(*seq),
        _ => None,
    }))
}

async fn exec(
    conn: &Connection,
    what: &str,
    sql: &str,
    params: Vec<Value>,
) -> Result<(), ApplyError> {
    conn.execute(sql, params)
        .await
        .map(|_| ())
        .map_err(|e| engine(what, e))
}

impl Write {
    /// 在已开的事务里校验并执行这一条写。`seq` 是这次提交的序号。
    pub(crate) async fn apply(
        &self,
        conn: &Connection,
        seq: i64,
        batch: &Batch<'_>,
    ) -> Result<(), ApplyError> {
        let what = self.name();
        match self {
            Write::InsertConversation {
                id,
                owner_conversation,
                owner_task,
                parent_conversation,
                parent_at,
                session_id,
                record,
            } => {
                claim(conn, *id, "conversation", false).await?;
                exec(
                    conn,
                    what,
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
                )
                .await
            }
            Write::InsertEntry {
                id,
                conversation_id,
                head,
                record,
            } => {
                claim(conn, *id, "entry", false).await?;
                exec(
                    conn,
                    what,
                    "INSERT INTO entries (id, conversation_id, head, commit_seq, record) VALUES (?1, ?2, ?3, ?4, ?5)",
                    vec![int(*id), int(*conversation_id), opt(*head), int(seq), text(record)],
                )
                .await
            }
            Write::UpsertTask {
                id,
                conversation_id,
                kind,
                status,
                abort_requested,
                background,
                record,
            } => {
                claim(conn, *id, "task", true).await?;
                exec(
                    conn,
                    what,
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
                )
                .await
            }
            Write::UpsertSubmission {
                id,
                conversation_id,
                request_id,
                status,
                record,
            } => {
                claim(conn, *id, "submission", true).await?;
                if let Some(request) = request_id {
                    let holder = rows(
                        conn,
                        "SELECT id FROM submissions WHERE conversation_id = ?1 AND request_id = ?2 AND id != ?3",
                        vec![int(*conversation_id), text(request), int(*id)],
                    )
                    .await?;
                    if !holder.is_empty() {
                        return Err(rejected(format!(
                            "request ID {request:?} 在 conversation {conversation_id} 里已被占用"
                        )));
                    }
                }
                exec(
                    conn,
                    what,
                    "INSERT INTO submissions (id, conversation_id, request_id, status, record) VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id, request_id = excluded.request_id, \
                     status = excluded.status, record = excluded.record",
                    vec![
                        int(*id),
                        int(*conversation_id),
                        opt_text(request_id),
                        text(status),
                        text(record),
                    ],
                )
                .await
            }
            Write::CreateDocument {
                id,
                kind,
                scope_kind,
                owner_id,
                key_value,
                version,
                value,
            } => {
                claim(conn, *id, "document", false).await?;
                let retired = if batch.ephemeral.contains(id) {
                    int(seq)
                } else {
                    let live = rows(
                        conn,
                        "SELECT id FROM documents WHERE kind = ?1 AND scope_kind = ?2 AND owner_id = ?3 AND key_value = ?4 AND retired_seq IS NULL",
                        vec![text(kind), text(scope_kind), int(*owner_id), text(key_value)],
                    )
                    .await?;
                    if !live.is_empty() {
                        return Err(rejected("这个 document 地址上已经有活的 incarnation"));
                    }
                    Value::Null
                };
                exec(
                    conn,
                    what,
                    "INSERT INTO documents (id, kind, scope_kind, owner_id, key_value, version, created_seq, changed_seq, retired_seq, value) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)",
                    vec![
                        int(*id),
                        text(kind),
                        text(scope_kind),
                        int(*owner_id),
                        text(key_value),
                        int(*version),
                        int(seq),
                        retired,
                        text(value),
                    ],
                )
                .await
            }
            Write::ReplaceDocument { id, version, value } => {
                match document_state(conn, *id).await? {
                    None => return Err(rejected(format!("未知的 document：{id}"))),
                    Some(Some(_)) => return Err(rejected(format!("document {id} 已退役"))),
                    Some(None) => {}
                }
                exec(
                    conn,
                    what,
                    "UPDATE documents SET value = ?2, version = ?3, changed_seq = ?4 WHERE id = ?1",
                    vec![int(*id), text(value), int(*version), int(seq)],
                )
                .await
            }
            Write::RetireDocument { id } => {
                if batch.ephemeral.contains(id) {
                    return Ok(());
                }
                match document_state(conn, *id).await? {
                    None => return Err(rejected(format!("未知的 document：{id}"))),
                    Some(Some(_)) => return Err(rejected(format!("document {id} 已退役"))),
                    Some(None) => {}
                }
                exec(
                    conn,
                    what,
                    "UPDATE documents SET retired_seq = ?2 WHERE id = ?1",
                    vec![int(*id), int(seq)],
                )
                .await
            }
            Write::UpsertSession {
                id,
                lifecycle,
                current_branch,
                root_conversation,
                record,
            } => {
                exec(
                    conn,
                    what,
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
                )
                .await
            }
        }
    }
}
