//! 记忆召回块（`docs/memory.md` §9.4「按 Run 召回」）：[`AgentHooks::context_entries`] 的 Gateway 实现。
//!
//! 每轮请求前做两件事，交回的 entry 由 harness 排在系统段补丁之后、同一次提交：
//!
//! 1. **核对已展示的条目。** 当前上下文视图里每个召回块还展示着哪些 `id@revision`，逐条
//!    `revalidate`（被遗忘、过期、被取代、版本变了）并按现在放行的作用域筛。有失效的就追加一条只带
//!    `edits` 的 `komo.memory.revoke`：受影响的块 `replace` 成去掉那几行的视图，一行不剩就 `omit`。
//!    "立刻停用"（§9.2）优先于前缀稳定，缓存只从被改的那块往后失效。
//! 2. **这个 Run 还没召回过就召回。** 以 Run 首个输入的 submission 认"这个 Run"，当前视图里已有它的
//!    召回记录就不再召回（重启、退避重发、后续轮次、插话都复用）。查询文本是最近一条不是宿主附上的
//!    用户消息；选定的条目去掉本段已展示的，剩下的渲染成一条带 `komoOrigin = memory` 的 user 消息。
//!    一条不剩（或召回失败）也落一条**没有 `model`** 的记录：不进上下文，只让之后知道召回过。
//!
//! 召回块最前面的事项组（`docs/goals.md` §5）走同一套：已展示的 `id@revision` 记在 `RecallData.matters`，
//! 每轮请求前连同记忆一起核对、改写；`memory = private` 的会话不带事项。
//!
//! fork 任务什么都不做：它的上下文接着父的（连父的召回块），首请求要与父的上一次逐字节同前缀。
//!
//! [`AgentHooks::context_entries`]: komo_runtime::durable::harness::AgentHooks::context_entries

use std::collections::HashMap;
use std::sync::Arc;

use komo_agent::context::memory::{shown_matters, shown_uses, without_all};
use komo_kernel::durable::recall::{
    MEMORY_ORIGIN, MEMORY_RECALL_ENTRY_KIND, MEMORY_REVOKE_ENTRY_KIND, RecallData,
};
use komo_kernel::durable::{
    Content, ContentBlock, ContextEdit, EntryId, EntryRecord, HOST_ORIGIN_KEY, Message,
    UserMessage, legacy_ids,
};
use komo_kernel::types::agent::MemoryMode;
use komo_kernel::types::matter::MatterUse;
use komo_kernel::types::memory::scope_allowed;
use komo_kernel::types::turn::MemoryUse;
use komo_runtime::durable::EntryDraft;
use komo_runtime::durable::harness::ContextRequest;
use serde_json::{Value, json};

use super::sections::recall_scopes;
use super::setup::Resolved;
use crate::service::state::GatewayState;

/// 视图里一个还展示着条目的召回块。
struct Shown {
    entry: EntryId,
    message: UserMessage,
    text: String,
    uses: Vec<MemoryUse>,
    matters: Vec<MatterUse>,
}

pub(super) async fn entries(
    state: &Arc<GatewayState>,
    resolved: &Resolved,
    request: ContextRequest<'_>,
) -> Vec<EntryDraft> {
    if resolved.is_fork() {
        return Vec::new();
    }
    let scopes = recall_scopes(resolved);
    let records: HashMap<EntryId, &EntryRecord> = request
        .entries
        .iter()
        .map(|entry| (entry.id, entry))
        .collect();
    let mut out = Vec::new();

    let blocks = shown_blocks(&records, &request);
    let mut shown: Vec<MemoryUse> = blocks.iter().flat_map(|b| b.uses.clone()).collect();
    let mut shown_matter_uses: Vec<MatterUse> =
        blocks.iter().flat_map(|b| b.matters.clone()).collect();
    if !shown.is_empty() || !shown_matter_uses.is_empty() {
        let now = state.clock.now();
        let alive: Vec<MemoryUse> = state
            .memories
            .revalidate(&shown, now)
            .await
            .into_iter()
            .filter(|item| {
                scopes
                    .as_deref()
                    .is_none_or(|scopes| scope_allowed(scopes, &item.scope))
            })
            .map(|item| MemoryUse {
                memory: item.id,
                revision: item.revision,
            })
            .collect();
        // 事项（`docs/goals.md` §5）：版本变了、搁置、结案、删除都算失效，走同一条改写路径。
        let alive_matters = state.memories.revalidate_matters(&shown_matter_uses).await;
        if let Some(revoke) = revoke(&blocks, &alive, &alive_matters) {
            out.push(revoke);
        }
        shown = alive;
        shown_matter_uses = alive_matters;
    }

    let (Some(run), Some(scopes)) = (request.run, scopes) else {
        return out;
    };
    if !state.memories.enabled() || recalled(&records, &request, run) {
        return out;
    }
    let text = latest_user_text(&request.view.messages).unwrap_or_default();
    let injection = state
        .memories
        .recall_for_run(
            &legacy_ids::run_id(run),
            &text,
            &shown,
            &scopes,
            // 事项不分作用域：私有记忆的 Agent 不带（`memory = private`，`docs/goals.md` §5）。
            (resolved.profile.memory != MemoryMode::Private).then_some(&shown_matter_uses[..]),
        )
        .await;
    let data = RecallData {
        submission: run,
        uses: injection.uses,
        matters: injection.matter_uses,
        recall: injection.diagnostics,
        at: request.now,
    };
    let mut draft = EntryDraft::new(MEMORY_RECALL_ENTRY_KIND)
        .with_data(serde_json::to_value(&data).expect("RecallData 可序列化"));
    if let Some(text) = injection.text.filter(|text| !text.is_empty()) {
        let mut extra = serde_json::Map::new();
        extra.insert(HOST_ORIGIN_KEY.into(), Value::from(MEMORY_ORIGIN));
        draft.model = Some(vec![Message::User(UserMessage {
            content: Content::Text(text),
            timestamp: request.now,
            extra,
        })]);
    }
    out.push(draft);
    out
}

/// 视图里的召回块，连同它此刻还展示着的条目（改写过的块少几行，`omit` 过的不在）。
fn shown_blocks(
    records: &HashMap<EntryId, &EntryRecord>,
    request: &ContextRequest<'_>,
) -> Vec<Shown> {
    let view = request.view;
    view.entries
        .iter()
        .zip(&view.contributions)
        .filter_map(|(id, contribution)| {
            let data = RecallData::of(records.get(id)?)?;
            let Some(Message::User(message)) = contribution.first() else {
                return None;
            };
            let text = content_text(&message.content);
            let uses = shown_uses(&text, &data.uses);
            let matters = shown_matters(&text, &data.matters);
            (!uses.is_empty() || !matters.is_empty()).then(|| Shown {
                entry: *id,
                message: message.clone(),
                text,
                uses,
                matters,
            })
        })
        .collect()
}

/// 有失效的条目就交回一条改写旧块的 write entry（只带 `edits`）。
fn revoke(
    blocks: &[Shown],
    alive: &[MemoryUse],
    alive_matters: &[MatterUse],
) -> Option<EntryDraft> {
    let mut edits = Vec::new();
    let mut revoked = Vec::new();
    for block in blocks {
        let dead: Vec<MemoryUse> = block
            .uses
            .iter()
            .filter(|use_| !alive.contains(use_))
            .cloned()
            .collect();
        let dead_matters: Vec<MatterUse> = block
            .matters
            .iter()
            .filter(|use_| !alive_matters.contains(use_))
            .cloned()
            .collect();
        if dead.is_empty() && dead_matters.is_empty() {
            continue;
        }
        edits.push(match without_all(&block.text, &dead, &dead_matters) {
            Some(text) => ContextEdit::replace(
                block.entry,
                vec![Message::User(UserMessage {
                    content: Content::Text(text),
                    ..block.message.clone()
                })],
            ),
            None => ContextEdit::omit(block.entry),
        });
        revoked.push(json!({ "entry": block.entry, "uses": dead, "matters": dead_matters }));
    }
    if edits.is_empty() {
        return None;
    }
    let mut draft =
        EntryDraft::new(MEMORY_REVOKE_ENTRY_KIND).with_data(json!({ "revoked": revoked }));
    draft.edits = edits;
    Some(draft)
}

/// 这个 Run 在当前视图里已经有召回记录（有没有正文都算）。
fn recalled(
    records: &HashMap<EntryId, &EntryRecord>,
    request: &ContextRequest<'_>,
    run: komo_kernel::durable::SubmissionId,
) -> bool {
    request.view.entries.iter().any(|id| {
        records
            .get(id)
            .and_then(|entry| RecallData::of(entry))
            .is_some_and(|data| data.submission == run)
    })
}

/// 最近一条用户自己说的话（宿主附上的召回块不算）。
fn latest_user_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        Message::User(user) if user.host_origin().is_none() => Some(content_text(&user.content)),
        _ => None,
    })
}

fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}
