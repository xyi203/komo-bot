//! `/fork` 与 `/clone`：把一个 Session 的一条路径**复制**成一个新 Session
//! （`docs/storage-durable-d5.md` §7；决定 10）。
//!
//! 两者都复制，区别只在落点：
//!
//! - `/clone`：复制到 `target`（默认 leaf）为止的整条路径；
//! - `/fork`：选中的是用户输入时复制到它的**上一条**（输入的原文留给编辑框），其余同 `/clone`。
//!
//! 复制的是**可见 entry**：路径上的每条 entry 逐条 `append_entry` 进新 Session 的根 conversation，
//! 新铸 ID，`head`（"自此重新开始" / 压缩的第一条保留）与 `edits` 的目标按旧 ID → 新 ID 的映射重写，
//! `label.data.targetId` 与 `branch.summary.data.fromId` 这两个 gateway 自己写的引用同样重写。
//! **不复制** task、submission、`komo.live`、`komo.inbox`；复制 `komo.agent`（去掉 `source`，免得
//! 按来源找会话时找到副本）。新会话不带 `parent_session`（那是任务会话指向 home 用的）。
//!
//! # blob
//!
//! `tool-output/`、`artifacts/`、`payloads/` 三个目录整份复制到新 Session 的目录。顺序是
//! 复制到暂存目录 → 逐文件 `sync_all` → 目录 `fsync` → 改名成 `sessions/{新 ID}/` → **之后**才提交
//! （blob 先落、引用后到，§8.5）；提交失败就删掉这个目录。entry 里的引用是相对 Session 目录的，
//! 目录结构原样保留，所以引用在副本里照样能解析，源 Session 被 purge 之后副本仍然完整。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use komo_kernel::durable::{ContextEdit, EntryId, EntryRecord, SessionKind, SessionRecord};
use komo_kernel::types::ids::SessionId;
use komo_runtime::durable::{DraftHead, EntryDraft};
use serde_json::{Value, json};

use super::DurableCore;
use super::agent_doc::{self, AgentDoc};
use super::sessions::{NewSession, SessionError, create_in};
use super::tree::{BRANCH_SUMMARY_ENTRY, LABEL_ENTRY, entry_id_text, input_text, parse_entry_id};

/// 整份复制的 blob 子目录。
const BLOB_DIRS: [&str; 3] = ["tool-output", "artifacts", "payloads"];

#[derive(Debug, Clone, Default)]
pub struct ForkRequest {
    /// 复制到哪一条为止；没有就是当前 leaf。
    pub target: Option<EntryId>,
    /// `true` = `/clone`（连输入本身一起复制）；`false` = `/fork`。
    pub clone: bool,
}

fn io_error(context: &str, error: impl std::fmt::Display) -> SessionError {
    SessionError::Internal(format!("{context}：{error}"))
}

/// 把 `source` 目录整份复制到 `target`（逐文件 fsync、逐目录 fsync）。源不存在什么都不做；
/// 里面有符号链接就拒绝（副本要独立于源）。
async fn copy_dir(source: &Path, target: &Path) -> Result<(), SessionError> {
    if !tokio::fs::try_exists(source)
        .await
        .map_err(|e| io_error("检查 blob 目录", e))?
    {
        return Ok(());
    }
    let mut queue = vec![(source.to_path_buf(), target.to_path_buf())];
    while let Some((source, target)) = queue.pop() {
        komo_store::session_dir::create_dir_all_synced(&target)
            .await
            .map_err(|e| io_error("建 blob 目录", e))?;
        let mut items = tokio::fs::read_dir(&source)
            .await
            .map_err(|e| io_error("读 blob 目录", e))?;
        while let Some(item) = items
            .next_entry()
            .await
            .map_err(|e| io_error("读 blob 目录", e))?
        {
            let kind = item
                .file_type()
                .await
                .map_err(|e| io_error("读 blob 类型", e))?;
            let destination = target.join(item.file_name());
            if kind.is_symlink() {
                return Err(SessionError::Invalid(
                    "会话输出包含符号链接，无法独立复制".into(),
                ));
            }
            if kind.is_dir() {
                queue.push((item.path(), destination));
            } else {
                tokio::fs::copy(item.path(), &destination)
                    .await
                    .map_err(|e| io_error("复制 blob", e))?;
                tokio::fs::File::open(&destination)
                    .await
                    .map_err(|e| io_error("复制 blob", e))?
                    .sync_all()
                    .await
                    .map_err(|e| io_error("复制 blob", e))?;
            }
        }
        komo_store::session_dir::sync_dir(&target)
            .await
            .map_err(|e| io_error("同步 blob 目录", e))?;
    }
    Ok(())
}

/// 把源 Session 的 blob 复制成 `sessions/{target}/`：先放暂存目录，同步完再改名。
async fn copy_blobs(sessions: &Path, source: &str, target: &str) -> Result<(), SessionError> {
    let source_dir = sessions.join(source);
    if !BLOB_DIRS.iter().any(|dir| source_dir.join(dir).exists()) {
        return Ok(());
    }
    let staging: PathBuf = sessions.join(format!(".{target}.fork"));
    let prepared: Result<(), SessionError> = async {
        komo_store::session_dir::create_dir_all_synced(&staging)
            .await
            .map_err(|e| io_error("建暂存目录", e))?;
        for dir in BLOB_DIRS {
            copy_dir(&source_dir.join(dir), &staging.join(dir)).await?;
        }
        komo_store::session_dir::sync_dir(&staging)
            .await
            .map_err(|e| io_error("同步暂存目录", e))?;
        tokio::fs::rename(&staging, sessions.join(target))
            .await
            .map_err(|e| io_error("改名暂存目录", e))?;
        komo_store::session_dir::sync_dir(sessions)
            .await
            .map_err(|e| io_error("同步 sessions 目录", e))?;
        Ok(())
    }
    .await;
    if prepared.is_err() {
        let _ = tokio::fs::remove_dir_all(&staging).await;
    }
    prepared
}

/// 一条 entry 复制成草稿：`head` 与 `edits` 的目标按 `map` 重写；gateway 自己写的引用同样重写。
/// `head == 自己`（`/new` 的 reset）写成 `SelfHead`，不查映射。
fn redraft(entry: &EntryRecord, map: &HashMap<EntryId, EntryId>) -> EntryDraft {
    let head = entry.head.and_then(|head| {
        if head == entry.id {
            Some(DraftHead::SelfHead)
        } else {
            map.get(&head).copied().map(DraftHead::At)
        }
    });
    let edits: Vec<ContextEdit> = entry
        .edits
        .iter()
        .filter_map(|edit| {
            map.get(&edit.target).map(|target| ContextEdit {
                target: *target,
                action: edit.action.clone(),
            })
        })
        .collect();
    let mut data = entry.data.clone();
    let reference = match entry.kind.as_str() {
        LABEL_ENTRY => Some("targetId"),
        BRANCH_SUMMARY_ENTRY => Some("fromId"),
        _ => None,
    };
    if let (Some(field), Some(Value::Object(map_data))) = (reference, data.as_mut())
        && let Some(old) = map_data
            .get(field)
            .and_then(Value::as_str)
            .and_then(|text| parse_entry_id(text).ok())
        && let Some(new) = map.get(&old)
    {
        map_data.insert(field.to_owned(), json!(entry_id_text(*new)));
    }
    EntryDraft {
        kind: entry.kind.clone(),
        model: entry.model.clone(),
        data,
        head,
        edits,
    }
}

impl DurableCore {
    /// `/fork` 与 `/clone`：复制成一个新会话，返回新会话的行。源会话忙时 409。
    pub async fn fork_session(
        &self,
        source: &str,
        request: ForkRequest,
    ) -> Result<SessionRecord, SessionError> {
        let tree = self.editable_tree(source).await?;
        let target = match request.target {
            Some(target) => {
                self.entry_in(&tree, target)?;
                Some(target)
            }
            None => tree.leaf,
        };
        let Some(target) = target else {
            return Err(SessionError::Invalid("空会话没有可复制的分支".into()));
        };
        // `/fork` 选中输入：复制到它的上一条，原文留给编辑框（旧实现同口径）。
        let selected = self.entry_in(&tree, target)?;
        let upto = if !request.clone && input_text(selected).is_some() {
            tree.prev_of(target)
        } else {
            Some(target)
        };
        let entries: Vec<EntryRecord> = upto
            .map(|upto| tree.path(upto).into_iter().cloned().collect())
            .unwrap_or_default();

        let doc = agent_doc::read(&*self.storage, tree.session.current_branch)
            .await?
            .unwrap_or_else(|| AgentDoc::new(tree.session.agent.clone().unwrap_or_default()));
        let mut spec = NewSession::new(doc.agent.clone());
        spec.kind = SessionKind::Normal;
        spec.title = tree.session.title.clone();
        spec.title_provisional = tree.session.title_provisional;
        spec.doc = AgentDoc {
            source: None,
            workdir: doc.workdir.clone().or_else(|| tree.session.cwd.clone()),
            ..doc
        };

        let new_id = SessionId::new_at(self.clock.now()).to_string();
        let sessions = self.home.join("sessions");
        copy_blobs(&sessions, source, &new_id).await?;

        let committed = self
            .engine
            .commit({
                let new_id = new_id.clone();
                move |tx| {
                    Box::pin(async move {
                        let record = create_in(tx, &spec, &new_id).await?;
                        let mut map: HashMap<EntryId, EntryId> = HashMap::new();
                        for entry in &entries {
                            let draft = redraft(entry, &map);
                            let copied = tx.append_entry(record.root_conversation, draft).await?;
                            map.insert(entry.id, copied.id);
                        }
                        Ok(record)
                    })
                }
            })
            .await;
        match committed {
            Ok(record) => Ok(record),
            Err(error) => {
                // 引用没落下，这份 blob 不属于任何会话：删掉（目录是新 UUID，只有我们碰过它）。
                let _ = tokio::fs::remove_dir_all(sessions.join(&new_id)).await;
                Err(error.into())
            }
        }
    }
}
