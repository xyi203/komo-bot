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
use komo_kernel::traits::RepoError;
use komo_kernel::types::ids::{MatterId, MemoryId, RunId, SessionId};
use komo_kernel::types::memory::{
    Confirmation, Evidence, EvidenceRef, ExtractionMetadata, MemoryApplication, MemoryItem,
    MemoryKind, MemoryState, MemoryUsage, Provenance,
};
use komo_kernel::types::plan::PlanSource;
use time::OffsetDateTime;

use super::extract::{HANDBACK_KEY_PREFIXES, MAX_CONTENT_CHARS, Transcript, looks_like_secret};
use super::{MemoryError, MemoryManager, SessionEvents};

/// 手动记下的条目在 `extraction.prompt_version` 里的标记。提取时靠它认出"这段对话里
/// 已经手动记过的"，不再提一遍（[`is_manual`]）。v2 起带上使用方式与条件（§9.3）；v1 的
/// 存量条目仍算手动记的。
pub const MANUAL_VERSION: &str = "memory-add/v2";
/// 手动记下的条目在 `extraction.model` 里写什么：不是哪个模型整理的。
const MANUAL_MODEL: &str = "komo memory add";

/// 这一条是 `komo memory add` 记下的。
pub fn is_manual(item: &MemoryItem) -> bool {
    item.extraction.prompt_version.starts_with("memory-add/")
}

/// convention 的规则（§9.2，add 与 classify 共用）：只给 preference，必须带条件。
/// 返回规整后的条件（去首尾空白、空 = 没有）。
pub(super) fn checked_usage(
    kind: MemoryKind,
    application: Option<MemoryApplication>,
    applies_when: Option<&str>,
) -> Result<Option<String>, MemoryError> {
    let when = applies_when.map(str::trim).filter(|when| !when.is_empty());
    if let Some(when) = when {
        let chars = when.chars().count();
        if chars > MAX_CONTENT_CHARS {
            return Err(MemoryError::Invalid(format!(
                "条件 {chars} 字，超过 {MAX_CONTENT_CHARS}：写成一句能独立理解的话"
            )));
        }
        if looks_like_secret(when) {
            return Err(MemoryError::Invalid(
                "条件里像是有密钥、令牌或密码，不记".into(),
            ));
        }
    }
    if application == Some(MemoryApplication::Convention) {
        if kind != MemoryKind::Preference {
            return Err(MemoryError::Invalid(
                "convention 只能给 preference：事实与经验是线索，不是用户定下的约定".into(),
            ));
        }
        if when.is_none() {
            return Err(MemoryError::Invalid(
                "convention 必须带 applies_when：写明在什么情况下适用".into(),
            ));
        }
    }
    Ok(when.map(str::to_string))
}

/// `classify` 对归属的处理：`--for <id>` / `--for none` / 没给。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BelongsChange {
    Keep,
    Clear,
    Set(MatterId),
}

impl MemoryManager {
    /// 归属只能指向存在且未结案的事项：结案的那件事名下的线索会被归档，再往上挂就是造出一条
    /// 还没来得及归档的（`docs/goals.md` §6）；要挂先 `reopen`。
    async fn open_matter(&self, id: &MatterId) -> Result<(), MemoryError> {
        let Some(matters) = &self.matters else {
            return Err(MemoryError::Invalid(
                "这台 Gateway 没接目标与领域，没有事项可挂".into(),
            ));
        };
        match matters.get(id).await? {
            None => Err(MemoryError::Invalid(format!("没有事项 {id}"))),
            Some(matter) if matter.state.is_closed() => Err(MemoryError::Invalid(format!(
                "事项 {id} 已经{}：先 reopen 再挂，或不挂归属",
                matter.state
            ))),
            Some(_) => Ok(()),
        }
    }

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

        // 省略 application 记成 reference；convention 要 preference + 条件，否则参数错误。
        let applies_when = checked_usage(
            request.kind,
            request.application,
            request.applies_when.as_deref(),
        )?;

        if let Some(matter) = &request.belongs_to {
            self.open_matter(matter).await?;
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
                // 随事项结案归档的：用户又说了一遍，要么这次给了新的归属（挂回活的事），要么
                // 该先把事项重开——不悄悄让它回到无归属的推送里。
                MemoryState::Archived if request.belongs_to.is_none() => {
                    return Err(MemoryError::Invalid(format!(
                        "这一条随事项结案归档了（{}@{}）：要用它，reopen 那件事，或 \
                         `komo memory classify {} --for <事项>`",
                        existing.id, existing.revision, existing.id
                    )));
                }
                MemoryState::Active
                | MemoryState::Candidate
                | MemoryState::Contested
                | MemoryState::Archived => {
                    let item = self
                        .reinforce(
                            existing,
                            evidence,
                            confirmation,
                            (
                                request.application,
                                applies_when,
                                request.belongs_to.clone(),
                            ),
                            now,
                        )
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
            application: Some(request.application.unwrap_or(MemoryApplication::Reference)),
            applies_when,
            belongs_to: request.belongs_to.clone(),
        };
        Ok((self.repo.put(item, None).await?, true))
    }

    /// 库里已有逐字相同的一条：补证据；用户明说过的候选抬成 active + user_statement
    /// （§9.2 的"明确的用户原话"）；操作者自己记的，顺带确认。contested 留着等人裁。
    ///
    /// 这次带的使用方式或条件与它不同时升一版改过来（§9.3）；没带 `--application` 就
    /// 不动它的分类——省略只代表"新建时记成 reference"，不能让一次普通的重复 add 把已有
    /// 的约定降级。归属同理：没带 `--for` 不动它；带了且不同就改过来、升一版（归档的随之回到
    /// active，用户已经把它挂回一件活的事）。
    async fn reinforce(
        &self,
        existing: MemoryItem,
        evidence: Vec<Evidence>,
        confirmation: Confirmation,
        (application, applies_when, belongs_to): (
            Option<MemoryApplication>,
            Option<String>,
            Option<MatterId>,
        ),
        now: OffsetDateTime,
    ) -> Result<MemoryItem, MemoryError> {
        let mut merged = existing.clone();
        let new_application = application.or(existing.application);
        let new_when = applies_when
            .clone()
            .or_else(|| existing.applies_when.clone());
        let rehomed = belongs_to.is_some() && belongs_to != existing.belongs_to;
        let reclassified = (application.is_some() && application != existing.application)
            || (applies_when.is_some() && applies_when != existing.applies_when);
        if reclassified || rehomed {
            // 沿用的条件也要过 convention 的规则：已有条目没条件、这次只改成 convention 时拒绝。
            checked_usage(existing.kind, new_application, new_when.as_deref())?;
            merged.application = new_application;
            merged.applies_when = new_when;
            if rehomed {
                merged.belongs_to = belongs_to;
                if merged.state == MemoryState::Archived {
                    merged.state = MemoryState::Active;
                }
            }
            merged.revision = existing.revision + 1;
        }
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
            // 预期的是库里现在的版本；升版时 `merged.revision` 已经是下一版。
            self.repo.put(merged, Some(existing.revision)).await?
        };
        if confirmation == Confirmation::UserConfirmed
            && item.confirmation != Confirmation::UserConfirmed
        {
            item = self.repo.confirm(&item.id, item.revision, now).await?;
        }
        Ok(item)
    }

    /// 操作者给指定 revision 定使用方式、条件与归属（§9.3「存量分类」、`docs/goals.md` §6）。
    ///
    /// 写成新 revision，证据与确认原样保留（"改 application、applies_when 或归属是内容语义
    /// 变化"，§9.2，所以旧注入的引用随版本失效）；规则同 `add`。与 confirm 同属操作者动作，
    /// Agent 不替用户执行——那一条靠系统提示，不是硬边界（§9.6）。`application` 省略 = 沿用现
    /// 有分类（只改归属时），`applies_when` 省略 = 沿用现有条件，`belongs` 为 `Keep` = 不动归属。
    /// 什么都没变时不升版。
    ///
    /// 随事项结案归档的条目只有改挂到一件进行中的事、或清空归属时才回到 active：那是操作者
    /// 在说"它还有用"；单改分类不改变它是归档的这件事。
    pub async fn classify(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        application: Option<MemoryApplication>,
        applies_when: Option<&str>,
        belongs: BelongsChange,
    ) -> Result<MemoryItem, MemoryError> {
        if !self.enabled() {
            return Err(MemoryError::Disabled);
        }
        if application.is_none() && applies_when.is_none() && belongs == BelongsChange::Keep {
            return Err(MemoryError::Invalid(
                "classify 要给 application / applies_when / 归属（--for）里的至少一个".into(),
            ));
        }
        let existing = self
            .repo
            .get(id)
            .await?
            .ok_or_else(|| RepoError::NotFound {
                what: format!("记忆 {id}"),
            })?;
        if existing.revision != expected_revision {
            return Err(RepoError::VersionConflict {
                expected: expected_revision,
                actual: existing.revision,
            }
            .into());
        }
        if matches!(
            existing.state,
            MemoryState::Forgotten | MemoryState::Superseded
        ) {
            return Err(MemoryError::Invalid(format!(
                "记忆 {id} 现在是 {:?}，不能再分类",
                existing.state
            )));
        }
        if let BelongsChange::Set(matter) = &belongs {
            self.open_matter(matter).await?;
        }
        // 省略的条件沿用现有的；两者合起来要满足 convention 的规则。
        let when = checked_usage(existing.kind, None, applies_when)?
            .or_else(|| existing.applies_when.clone());
        let next_application = application.or(existing.application);
        checked_usage(existing.kind, next_application, when.as_deref())?;
        let next_belongs = match &belongs {
            BelongsChange::Keep => existing.belongs_to.clone(),
            BelongsChange::Clear => None,
            BelongsChange::Set(matter) => Some(matter.clone()),
        };
        if existing.application == next_application
            && existing.applies_when == when
            && existing.belongs_to == next_belongs
        {
            return Ok(existing);
        }
        let mut next = existing.clone();
        next.application = next_application;
        next.applies_when = when;
        if existing.state == MemoryState::Archived && belongs != BelongsChange::Keep {
            next.state = MemoryState::Active;
        }
        next.belongs_to = next_belongs;
        next.revision = existing.revision + 1;
        next.updated_at = self.clock.now();
        Ok(self.repo.put(next, Some(existing.revision)).await?)
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
        let evidence = run_user_evidence(&*self.events, session, run, now).await?;
        if evidence.is_empty() {
            return Err(MemoryError::Invalid(format!(
                "会话 {session} 的 Run {run} 里找不到用户说的话：替用户记一条要有用户原话作证据"
            )));
        }
        Ok(evidence)
    }
}

/// 这个 Run 里用户说的话，一条一份证据（[`user_inputs`] 的口径）；一句都没有是空表，由
/// 调用方决定怎么拒。`memory add`（§9.3）与事项的承诺类操作（`docs/goals.md` §4）共用：
/// 两边说的都是"Agent 在 Run 里替用户改用户的东西，凭的只能是用户原话"。
pub async fn run_user_evidence(
    events: &dyn SessionEvents,
    session: &SessionId,
    run: &RunId,
    now: OffsetDateTime,
) -> Result<Vec<Evidence>, MemoryError> {
    let events = events.events_of(session).await?;
    Ok(user_inputs(&events, run)
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
        .collect())
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
