//! Durable 模式的 `export` 路由（D6，`docs/storage-durable.md` §9）：
//! `GET /v1/sessions/{id}/export` 把一个会话导出成 pi v3 树形 JSONL。
//!
//! # 文件形状
//!
//! 第一行是 pi 的 v3 header（`SessionHeader`：`type: "session"`、`version: 3`、`id`、`timestamp`、
//! `cwd`、`parentSession`），多一个 komo 自己的 `komoBlobRoot`（pi 读 header 时忽略不认识的字段）；
//! 其后每行一条 `pi_tree` 给出的 entry（与 `/tree` 同一个投影，逐条原样）。没有导入：只出不进（§8）。
//!
//! # blob 引用
//!
//! pi 的格式里没有"引用外部字节"的约定（图片是内联 base64），所以 **blob 原样保持 komo 里的引用
//! 形状，不内联字节**：
//!
//! - 图片块是 `{type: "image", mime, path, hash}`，`path` 相对 `komoBlobRoot`（Session 目录，
//!   如 `images/a.png`）；
//! - 工具输出是 `tool-output/{run}/{call}/{attempt}/…`（`details` 里的 `output_ref.path`）与消息里印的
//!   `artifact://{run}/{call}/{attempt}/{result|stdout|…}`、产物 `artifact://files/{path}`，都解析到
//!   `komoBlobRoot` 之下（对应关系见 `types::resource`）。
//!
//! pi 读入时这些块只是不带 `data` 的图片与普通文字，不影响树与上下文推导；要看字节就拿
//! `komoBlobRoot` 拼路径。
//!
//! # 当前位置
//!
//! pi 读文件时**最后一条 entry 就是当前位置**（导航本身不落盘）。所以导出要让 leaf 落在文件末尾：
//!
//! - leaf 已经是末条：原样；
//! - leaf 没有后代、也不是 `label` / `session_info`（这两种按文件顺序"后写者胜"，挪位置会改结果）：
//!   把它挪到最后（其余 entry 的相对顺序不变，父仍在子前）；
//! - 其余（leaf 在树中间、或在"第一条之前"）：末尾追加一条 `custom` entry `komo.leaf`，
//!   `parentId` 指向 leaf（"第一条之前"时是新根，上下文为空），`data.leafId` 记下真正的 leaf。
//!   `custom` 不进上下文，所以 pi 读回的上下文与 `/tree` 在 leaf 处的一致，只是多一条叶子。

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use komo_kernel::durable::pi_tree::{ConversationEntries, PiTree, pi_tree};
use komo_kernel::durable::records::SessionRecord;
use komo_kernel::session::{EntryKind, SessionEntry, SessionHeader};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::http::Api;
use crate::http::error::{ApiFailure, ApiResult};

/// 末尾那条"当前位置"标记的 id 与 `customType`（id 不是数字，不会与 entry 的 pi ID 相撞）。
pub const LEAF_MARKER_ID: &str = "komo-leaf";
pub const LEAF_MARKER_TYPE: &str = "komo.leaf";

pub fn routes() -> Router<Api> {
    Router::new().route("/v1/sessions/{id}/export", get(export))
}

/// `GET /v1/sessions/{id}/export`：`application/x-ndjson`，一行一条记录。
async fn export(State(api): State<Api>, Path(id): Path<String>) -> ApiResult<Response> {
    let core = api.state.core()?;
    let data = core.read_tree(&id).await?;
    let conversations: Vec<ConversationEntries<'_>> = data
        .branches
        .iter()
        .map(|branch| ConversationEntries {
            record: &branch.conversation,
            entries: &branch.entries,
        })
        .collect();
    let tree = pi_tree(
        &conversations,
        data.session.current_branch,
        data.session.leaf,
    );
    let blob_root = api
        .state
        .snapshot()
        .paths
        .sessions_dir
        .join(&data.session.id);
    let body = jsonl(&data.session, &blob_root.to_string_lossy(), tree)
        .map_err(|error| ApiFailure::invalid(format!("导出失败：{error}")))?;
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response())
}

/// header + entry，按行（每行以 `\n` 结尾）。
pub fn jsonl(session: &SessionRecord, blob_root: &str, tree: PiTree) -> Result<String, String> {
    let entries = arranged(tree);
    let timestamp = if session.created_at > 0 {
        rfc3339(session.created_at)
    } else {
        entries
            .first()
            .map_or_else(|| rfc3339(0), |entry| entry.timestamp.clone())
    };
    let mut header = SessionHeader::new(
        session.id.clone(),
        timestamp,
        session.cwd.clone().unwrap_or_default(),
    );
    header.parent_session = session.parent_session.clone();
    let mut header = serde_json::to_value(&header).map_err(|error| error.to_string())?;
    header["komoBlobRoot"] = Value::String(blob_root.to_owned());

    let mut out = String::new();
    for line in std::iter::once(header).chain(
        entries
            .iter()
            .map(|entry| serde_json::to_value(entry).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?,
    ) {
        out.push_str(&line.to_string());
        out.push('\n');
    }
    Ok(out)
}

/// 让 pi（"最后一条 = 当前位置"）读回同一个 leaf，见模块文档。
fn arranged(tree: PiTree) -> Vec<SessionEntry> {
    let PiTree {
        mut entries,
        leaf_id,
    } = tree;
    if entries.is_empty() {
        return entries;
    }
    let Some(leaf) = leaf_id else {
        // 第一条之前：一个新根的标记，pi 读回的上下文是空的。
        entries.push(marker(None, &entries[0].timestamp, None));
        return entries;
    };
    let Some(at) = entries.iter().position(|entry| entry.id == leaf) else {
        return entries;
    };
    if at + 1 == entries.len() {
        return entries;
    }
    let has_children = entries
        .iter()
        .any(|entry| entry.parent_id.as_deref() == Some(leaf.as_str()));
    let order_sensitive = matches!(
        entries[at].kind,
        EntryKind::Label { .. } | EntryKind::SessionInfo { .. }
    );
    if !has_children && !order_sensitive {
        let entry = entries.remove(at);
        entries.push(entry);
    } else {
        let timestamp = entries[at].timestamp.clone();
        entries.push(marker(Some(&leaf), &timestamp, Some(&leaf)));
    }
    entries
}

fn marker(parent: Option<&str>, timestamp: &str, leaf: Option<&str>) -> SessionEntry {
    SessionEntry {
        id: LEAF_MARKER_ID.to_owned(),
        parent_id: parent.map(str::to_owned),
        timestamp: timestamp.to_owned(),
        kind: EntryKind::Custom {
            custom_type: LEAF_MARKER_TYPE.to_owned(),
            data: Some(serde_json::json!({ "leafId": leaf })),
        },
    }
}

fn rfc3339(millis: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests;
