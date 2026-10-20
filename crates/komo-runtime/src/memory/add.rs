//! 手动记一条（§9.3「手动记一条」）：`komo memory add` 落到这里。
//!
//! 两条路，**按调用方分确认等级**：
//!
//! - 带 `session` + `run`：Agent 在 bash 里替用户记（子进程带着 `KOMO_SESSION` /
//!   `KOMO_RUN`）。证据是这个 Run 里用户说的话，记成 active + user_statement +
//!   unconfirmed——与提取到一句明确的用户原话同级（§9.2），等操作者自己 confirm。
//! - 都不带：操作者在终端亲口说的，记成 active + user_statement + user_confirmed。
//!
//! 这不是安全边界：bash 里去掉两个变量就走得到第二条路，和 bash 本来就能跑
//! `komo memory confirm` 一样（§7 没有 Policy）。它分的是**常态下谁在说话**。

use std::collections::BTreeSet;

use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::http::MemoryAddRequest;
use komo_kernel::types::ids::{MemoryId, RunId, SessionId};
use komo_kernel::types::memory::{
    Confirmation, Evidence, EvidenceRef, ExtractionMetadata, MemoryItem, MemoryState, MemoryUsage,
    Provenance,
};
use komo_kernel::types::plan::PlanSource;
use time::OffsetDateTime;

use super::extract::{HANDBACK_KEY_PREFIXES, MAX_CONTENT_CHARS, Transcript, looks_like_secret};
use super::{MemoryError, MemoryManager};

/// 手动记下的条目在 `extraction.prompt_version` 里的标记。提取时靠它认出"这段对话里
/// 已经手动记过的"，不再提一遍（[`is_manual`]）。
pub const MANUAL_VERSION: &str = "memory-add/v1";
/// 手动记下的条目在 `extraction.model` 里写什么：不是哪个模型整理的。
const MANUAL_MODEL: &str = "komo memory add";

/// 这一条是 `komo memory add` 记下的。
pub fn is_manual(item: &MemoryItem) -> bool {
    item.extraction.prompt_version == MANUAL_VERSION
}

impl MemoryManager {
    /// 手动记一条。返回记下的那条，以及它是不是新建的（`false` = 库里已有逐字相同的
    /// 一条，这次只补了证据或确认）。
    pub async fn add(&self, request: &MemoryAddRequest) -> Result<(MemoryItem, bool), MemoryError> {
        if !self.enabled() {
            return Err(MemoryError::Disabled);
        }
        let content = request.content.trim().to_string();
        if content.is_empty() {
            return Err(MemoryError::Invalid("正文是空的".into()));
        }
        let chars = content.chars().count();
        if chars > MAX_CONTENT_CHARS {
            return Err(MemoryError::Invalid(format!(
                "正文 {chars} 字，超过 {MAX_CONTENT_CHARS}：一条记忆是一句简短、只说一件事的陈述（§9.2）"
            )));
        }
        // 与提取同一道闸：不把凭证存成长期事实（§9.3）。
        if looks_like_secret(&content) {
            return Err(MemoryError::Invalid(
                "正文里像是有密钥、令牌或密码，不记".into(),
            ));
        }

        let now = self.clock.now();
        let (evidence, confirmation) = match (&request.session, &request.run) {
            (Some(session), Some(run)) => (
                self.user_evidence(session, run, now).await?,
                Confirmation::Unconfirmed,
            ),
            (None, None) => (Vec::new(), Confirmation::UserConfirmed),
            _ => {
                return Err(MemoryError::Invalid(
                    "session 与 run 要么都给（Agent 在 Run 里替用户记），要么都不给（操作者自己记）"
                        .into(),
                ));
            }
        };

        if let Some(existing) = self
            .catalog
            .find_by_content(&request.scope, &content)
            .await?
        {
            match existing.state {
                // 遗忘是操作者做的决定：同一句话再来一次不悄悄复活它（§9.6）。
                MemoryState::Forgotten => {
                    return Err(MemoryError::Invalid(format!(
                        "这一条之前遗忘过（{}@{}）：要用它，`komo memory restore {} --revision {}`",
                        existing.id, existing.revision, existing.id, existing.revision
                    )));
                }
                // 已经被取代的旧答案：用户又说了一遍，算一条新的主张。
                MemoryState::Superseded => {}
                MemoryState::Active | MemoryState::Candidate | MemoryState::Contested => {
                    let item = self
                        .reinforce(existing, evidence, confirmation, now)
                        .await?;
                    return Ok((item, false));
                }
            }
        }

        let item = MemoryItem {
            id: MemoryId::new_at(now),
            revision: 1,
            content,
            kind: request.kind,
            scope: request.scope.clone(),
            provenance: Provenance::UserStatement,
            confirmation,
            state: MemoryState::Active,
            evidence,
            observed_at: now,
            valid_until: None,
            created_at: now,
            updated_at: now,
            extraction: ExtractionMetadata::new(MANUAL_MODEL, None, MANUAL_VERSION),
            usage: MemoryUsage::default(),
            supersedes: None,
            superseded_by: None,
        };
        Ok((self.repo.put(item, None).await?, true))
    }

    /// 库里已有逐字相同的一条：补证据；用户明说过的候选抬成 active + user_statement
    /// （§9.2 的"明确的用户原话"）；操作者自己记的，顺带确认。contested 留着等人裁。
    async fn reinforce(
        &self,
        existing: MemoryItem,
        evidence: Vec<Evidence>,
        confirmation: Confirmation,
        now: OffsetDateTime,
    ) -> Result<MemoryItem, MemoryError> {
        let mut merged = existing.clone();
        for evidence in evidence {
            if !merged
                .evidence
                .iter()
                .any(|have| have.reference == evidence.reference)
            {
                merged.evidence.push(evidence);
            }
        }
        if merged.state == MemoryState::Candidate {
            merged.state = MemoryState::Active;
            merged.provenance = Provenance::UserStatement;
        }
        let mut item = if merged == existing {
            existing
        } else {
            merged.updated_at = now;
            let revision = existing.revision;
            self.repo.put(merged, Some(revision)).await?
        };
        if confirmation == Confirmation::UserConfirmed
            && item.confirmation != Confirmation::UserConfirmed
        {
            item = self.repo.confirm(&item.id, item.revision, now).await?;
        }
        Ok(item)
    }

    /// 这一批窗口里已经手动记过的条目（还没退场的那些）。读不出来只记 warn：少了它
    /// 顶多多提一条，提取照常跑。
    pub(super) async fn recorded_by_hand(&self, transcript: &Transcript) -> Vec<MemoryItem> {
        match self.catalog.citing_session(&transcript.session).await {
            Ok(items) => items
                .into_iter()
                .filter(|item| {
                    matches!(
                        item.state,
                        MemoryState::Active | MemoryState::Candidate | MemoryState::Contested
                    ) && transcript.recorded_by_hand(item)
                })
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "找不出这段对话里手动记过的条目");
                Vec::new()
            }
        }
    }

    /// 这个 Run 里用户说的话，一条一份证据。一句都没有就不记——Agent 替用户记，凭的
    /// 只能是用户的原话。
    async fn user_evidence(
        &self,
        session: &SessionId,
        run: &RunId,
        now: OffsetDateTime,
    ) -> Result<Vec<Evidence>, MemoryError> {
        let events = self.events.events_of(session).await?;
        let evidence: Vec<Evidence> = user_inputs(&events, run)
            .into_iter()
            .map(|event| Evidence {
                reference: EvidenceRef::Event {
                    session: session.clone(),
                    event: event.event_id.clone(),
                    seq: event.seq,
                },
                provenance: Provenance::UserStatement,
                observed_at: now,
                extracted_from_run: Some(run.clone()),
            })
            .collect();
        if evidence.is_empty() {
            return Err(MemoryError::Invalid(format!(
                "会话 {session} 的 Run {run} 里找不到用户说的话：替用户记一条要有用户原话作证据"
            )));
        }
        Ok(evidence)
    }
}

/// 这个 Run 里用户说的话：它自己受理的那句、并进它的插话（插话带着自己的 Run id，
/// 由 `run.absorbed` 指向宿主），以及 `message.user`。任务 / 监听 / 后台命令交回的内容、
/// Cron 的提示词、子 Run 的委派正文都不是用户说的。
fn user_inputs<'a>(events: &'a [Event], run: &RunId) -> Vec<&'a Event> {
    let absorbed: BTreeSet<&RunId> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::RunAbsorbed(body) if &body.into == run => event.run.as_ref(),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter(|event| {
            let Some(owner) = event.run.as_ref() else {
                return false;
            };
            if owner != run && !absorbed.contains(owner) {
                return false;
            }
            match &event.payload {
                EventPayload::RunAccepted(body) => {
                    !HANDBACK_KEY_PREFIXES
                        .iter()
                        .any(|prefix| body.request_key.as_str().starts_with(prefix))
                        && !matches!(body.source, PlanSource::Cron { .. })
                        && body.delegate.is_none()
                }
                EventPayload::MessageUser(_) => true,
                _ => false,
            }
        })
        .collect()
}
