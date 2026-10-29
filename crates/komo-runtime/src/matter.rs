//! 目标与领域的操作（`docs/goals.md` §4）：`komo goal` / `komo area` 经 Gateway 落到这里。
//!
//! 状态机在 kernel（[`transition`]），这一层只做三件事：把请求的文字规整成一次
//! [`MatterChange`]（长度、凭证、哪个动词认哪些字段），按调用方决定要不要用户原话作证据，
//! 再带着预期版本读改写。
//!
//! **谁能改什么**按改的是不是"承诺"分（[`MatterVerb::is_commitment`]）：add / done / drop /
//! park / retire / reopen / edit 改的是用户要做、要管什么，Agent 在 Run 里改要有这个 Run 里
//! 用户说的话（规则同 `memory add`，§9.3），后台任务会话一律不行；next / wait / resume 是推进，
//! Agent 自己的工作。和 `memory add` 一样这不是安全边界（bash 去掉两个变量就走终端那条路，
//! §7 没有 Policy），分的是常态下谁在说话。

use std::sync::Arc;

use komo_kernel::protocol::http::{MatterActionRequest, MatterAddRequest};
use komo_kernel::traits::{Clock, MatterRepo, RepoError};
use komo_kernel::types::ids::{MatterId, RunId, SessionId};
use komo_kernel::types::matter::{
    MAX_TITLE_CHARS, Matter, MatterChange, MatterError, MatterKind, MatterVerb, NewMatter,
    transition,
};
use komo_kernel::types::memory::Evidence;

use crate::memory::{
    MAX_CONTENT_CHARS, MemoryError, SessionEvents, looks_like_secret, run_user_evidence,
};

/// 事项操作的失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatterFailure {
    /// 状态机不允许（起点不对、种类不对、字段不对）。
    #[error(transparent)]
    Rule(#[from] MatterError),
    /// 请求本身不成立：文字不合格、session/run 只给一个、Run 里没有用户原话。
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error("读不出会话：{0}")]
    Ledger(String),
}

impl From<MemoryError> for MatterFailure {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::Repo(error) => MatterFailure::Repo(error),
            MemoryError::Invalid(message) => MatterFailure::Invalid(message),
            other => MatterFailure::Ledger(other.to_string()),
        }
    }
}

/// 谁在改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// 操作者在终端：直接改，不带证据。
    Operator,
    /// Agent 在某个 Run 里（bash 子进程带着 `KOMO_SESSION` / `KOMO_RUN`）。
    Run {
        session: SessionId,
        run: RunId,
        /// 这个会话是后台任务（`kind = task`）：任务正文是主 Agent 写的，不是用户原话。
        background_task: bool,
    },
}

impl Caller {
    /// session 与 run 要么都给、要么都不给（同 `memory add`）。
    pub fn new(
        session: Option<SessionId>,
        run: Option<RunId>,
        background_task: bool,
    ) -> Result<Caller, MatterFailure> {
        match (session, run) {
            (None, None) => Ok(Caller::Operator),
            (Some(session), Some(run)) => Ok(Caller::Run {
                session,
                run,
                background_task,
            }),
            _ => Err(MatterFailure::Invalid(
                "session 与 run 要么都给（Agent 在 Run 里改），要么都不给（操作者自己改）".into(),
            )),
        }
    }
}

/// 事项的读写入口。
pub struct MatterManager {
    repo: Arc<dyn MatterRepo>,
    events: Arc<dyn SessionEvents>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for MatterManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatterManager").finish_non_exhaustive()
    }
}

impl MatterManager {
    pub fn new(
        repo: Arc<dyn MatterRepo>,
        events: Arc<dyn SessionEvents>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        MatterManager {
            repo,
            events,
            clock,
        }
    }

    pub async fn list(
        &self,
        kind: Option<MatterKind>,
        include_closed: bool,
    ) -> Result<Vec<Matter>, MatterFailure> {
        Ok(self.repo.list(kind, include_closed).await?)
    }

    pub async fn get(&self, id: &MatterId) -> Result<Matter, MatterFailure> {
        self.repo.get(id).await?.ok_or_else(|| {
            MatterFailure::Repo(RepoError::NotFound {
                what: format!("事项 {id}"),
            })
        })
    }

    /// 新建一条，`active`、revision 1。新建是承诺类操作。
    pub async fn add(
        &self,
        request: &MatterAddRequest,
        caller: &Caller,
    ) -> Result<Matter, MatterFailure> {
        let draft = NewMatter {
            kind: request.kind,
            title: line("标题", &request.title, MAX_TITLE_CHARS)?,
            next: optional_line("下一步", request.next.as_deref())?.flatten(),
            due: request.due,
            standard: optional_line("标准", request.standard.as_deref())?.flatten(),
        };
        let now = self.clock.now();
        let mut matter = draft.build(MatterId::new_at(now), now)?;
        matter.evidence = self.commitment_evidence(caller, "add").await?;
        Ok(self.repo.put(matter, None).await?)
    }

    /// 一次 `POST /v1/matters/{id}/{action}`。状态没变返回原样，不升版、不追加证据。
    pub async fn apply(
        &self,
        id: &MatterId,
        verb: MatterVerb,
        request: &MatterActionRequest,
        caller: &Caller,
    ) -> Result<Matter, MatterFailure> {
        let change = change_of(verb, request)?;
        let existing = self.get(id).await?;
        if let Some(kind) = request.kind
            && kind != existing.kind
        {
            return Err(MatterFailure::Invalid(format!(
                "{id} 是{}，不是{}：用 `komo {}`",
                existing.kind.label(),
                kind.label(),
                existing.kind
            )));
        }
        if let Some(expected) = request.expected_revision
            && expected != existing.revision
        {
            return Err(RepoError::VersionConflict {
                expected,
                actual: existing.revision,
            }
            .into());
        }
        let now = self.clock.now();
        // 状态不允许的先报：比"缺原话"更说明问题。
        let changed = transition(&existing, &change, now)?;
        let evidence = if verb.is_commitment() {
            self.commitment_evidence(caller, verb.as_str()).await?
        } else {
            Vec::new()
        };
        let Some(mut next) = changed else {
            return Ok(existing);
        };
        for item in evidence {
            if !next
                .evidence
                .iter()
                .any(|have| have.reference == item.reference)
            {
                next.evidence.push(item);
            }
        }
        Ok(self.repo.put(next, Some(existing.revision)).await?)
    }

    /// 改承诺要的证据：终端里不要；Run 里要这个 Run 里用户说的话，一句都没有就拒；
    /// 后台任务会话直接拒（§4「谁能改什么」）。Cron 的 Run 不必单列：它的提示词不算用户原话
    /// （[`run_user_evidence`] 的口径），一句都没有，照样被拒。
    async fn commitment_evidence(
        &self,
        caller: &Caller,
        verb: &str,
    ) -> Result<Vec<Evidence>, MatterFailure> {
        let Caller::Run {
            session,
            run,
            background_task,
        } = caller
        else {
            return Ok(Vec::new());
        };
        if *background_task {
            return Err(MatterFailure::Invalid(format!(
                "会话 {session} 是后台任务：任务正文不是用户原话，不能 {verb}；把要改的交回主会话"
            )));
        }
        let evidence = run_user_evidence(&*self.events, session, run, self.clock.now()).await?;
        if evidence.is_empty() {
            return Err(MatterFailure::Invalid(format!(
                "会话 {session} 的 Run {run} 里找不到用户说的话：{verb} 改的是用户要做、要管什么，\
                 要有用户原话作证据（推进用的 next / wait / resume 不需要）"
            )));
        }
        Ok(evidence)
    }
}

/// 一个动词认哪些字段（[`MatterActionRequest`] 的表），带了别的报错。
fn change_of(
    verb: MatterVerb,
    request: &MatterActionRequest,
) -> Result<MatterChange, MatterFailure> {
    let given: Vec<&str> = [
        ("title", request.title.is_some()),
        ("next", request.next.is_some()),
        ("waiting_for", request.waiting_for.is_some()),
        ("due", request.due.is_some()),
        ("clear_due", request.clear_due),
        ("standard", request.standard.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect();
    let accepted: &[&str] = match verb {
        MatterVerb::Next | MatterVerb::Resume => &["next"],
        MatterVerb::Wait => &["waiting_for"],
        MatterVerb::Edit => &["title", "due", "clear_due", "standard"],
        _ => &[],
    };
    if let Some(extra) = given.iter().find(|name| !accepted.contains(name)) {
        return Err(MatterFailure::Invalid(format!(
            "{verb} 不认 `{extra}` 这个字段"
        )));
    }
    Ok(match verb {
        MatterVerb::Next => {
            let next = optional_line("下一步", request.next.as_deref())?
                .ok_or_else(|| MatterFailure::Invalid("next 要给下一步（空 = 清空）".into()))?;
            MatterChange::Next(next)
        }
        MatterVerb::Wait => MatterChange::Wait(line(
            "在等什么",
            request.waiting_for.as_deref().unwrap_or_default(),
            MAX_CONTENT_CHARS,
        )?),
        MatterVerb::Resume => MatterChange::Resume {
            next: optional_line("下一步", request.next.as_deref())?.flatten(),
        },
        MatterVerb::Park => MatterChange::Park,
        MatterVerb::Done => MatterChange::Done,
        MatterVerb::Drop => MatterChange::Drop,
        MatterVerb::Retire => MatterChange::Retire,
        MatterVerb::Reopen => MatterChange::Reopen,
        MatterVerb::Edit => {
            if request.due.is_some() && request.clear_due {
                return Err(MatterFailure::Invalid(
                    "--due 与 --no-due 只能给一个".into(),
                ));
            }
            MatterChange::Edit {
                title: request
                    .title
                    .as_deref()
                    .map(|title| line("标题", title, MAX_TITLE_CHARS))
                    .transpose()?,
                due: if request.clear_due {
                    Some(None)
                } else {
                    request.due.map(Some)
                },
                standard: optional_line("标准", request.standard.as_deref())?,
            }
        }
    })
}

/// 一行必填文字：去首尾空白、不空、不换行、不超长、不像凭证（同记忆的闸，§9.3）。
fn line(what: &str, raw: &str, max: usize) -> Result<String, MatterFailure> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(MatterFailure::Invalid(format!("{what}是空的")));
    }
    // 清单一条一行（§4），注入也是一条一行（P1b）：换行会把一条拆成两条。
    if text.contains(['\n', '\r']) {
        return Err(MatterFailure::Invalid(format!("{what}写成一行")));
    }
    let chars = text.chars().count();
    if chars > max {
        return Err(MatterFailure::Invalid(format!(
            "{what} {chars} 字，超过 {max}：写成一句能独立看懂的话"
        )));
    }
    if looks_like_secret(text) {
        return Err(MatterFailure::Invalid(format!(
            "{what}里像是有密钥、令牌或密码，不记"
        )));
    }
    Ok(text.to_string())
}

/// 可空的一行：`None` = 没给；`Some(None)` = 给了空串（清空）；`Some(Some(..))` = 新值。
fn optional_line(what: &str, raw: Option<&str>) -> Result<Option<Option<String>>, MatterFailure> {
    match raw {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(Some(None)),
        Some(raw) => Ok(Some(Some(line(what, raw, MAX_CONTENT_CHARS)?))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use komo_kernel::events::{Event, EventPayload, MessageUser};
    use komo_kernel::types::ids::{EventId, Seq};
    use komo_kernel::types::matter::MatterState;
    use komo_kernel::types::memory::EvidenceRef;
    use komo_store::{Db, TursoMatterRepo};
    use time::OffsetDateTime;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-29 08:00 UTC);

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> OffsetDateTime {
            NOW
        }
    }

    /// `sess-1` 里两个 Run：`run-user` 有一句用户原话，`run-silent` 一句都没有。
    struct Events;

    #[async_trait]
    impl SessionEvents for Events {
        async fn events_of(&self, session: &SessionId) -> Result<Vec<Event>, MemoryError> {
            if session.as_str() != "sess-1" {
                return Ok(Vec::new());
            }
            Ok(vec![Event {
                v: 1,
                seq: Seq(1),
                event_id: EventId::from_raw("evt-1"),
                session: session.clone(),
                run: Some(RunId::from_raw("run-user")),
                ts: NOW,
                payload: EventPayload::MessageUser(MessageUser {
                    text: Some("开始做 owl 的发布观察".into()),
                    text_ref: None,
                    project_update: false,
                }),
                entry: None,
            }])
        }
    }

    async fn manager() -> (MatterManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("aux.db"))
            .await
            .expect("打开库");
        let manager = MatterManager::new(
            Arc::new(TursoMatterRepo::new(db)),
            Arc::new(Events),
            Arc::new(FixedClock),
        );
        (manager, dir)
    }

    fn in_run(run: &str) -> Caller {
        Caller::new(
            Some(SessionId::from_raw("sess-1")),
            Some(RunId::from_raw(run)),
            false,
        )
        .unwrap()
    }

    fn add_goal(title: &str) -> MatterAddRequest {
        MatterAddRequest {
            kind: MatterKind::Goal,
            title: title.into(),
            next: Some("看 AB 组的 5xx".into()),
            due: None,
            standard: None,
            session: None,
            run: None,
            request_key: None,
        }
    }

    fn cited(matter: &Matter) -> Vec<&str> {
        matter
            .evidence
            .iter()
            .map(|evidence| match &evidence.reference {
                EvidenceRef::Event { event, .. } => event.as_str(),
                EvidenceRef::Memos { .. } => "memos",
            })
            .collect()
    }

    /// Run 里新建：引这个 Run 里用户说的话作证据；终端里新建不带证据。
    #[tokio::test]
    async fn an_add_inside_a_run_cites_the_users_words() {
        let (manager, _dir) = manager().await;
        let matter = manager
            .add(&add_goal("这周把 owl 的发布观察做完"), &in_run("run-user"))
            .await
            .unwrap();
        assert_eq!(matter.state, MatterState::Active);
        assert_eq!(cited(&matter), ["evt-1"]);
        let operator = manager
            .add(&add_goal("把周报模板改好"), &Caller::Operator)
            .await
            .unwrap();
        assert!(operator.evidence.is_empty());
    }

    /// Run 里一句用户原话都没有：承诺类操作被拒，推进类照常。
    #[tokio::test]
    async fn a_commitment_without_user_words_is_refused_but_progress_is_not() {
        let (manager, _dir) = manager().await;
        let silent = in_run("run-silent");
        assert!(matches!(
            manager.add(&add_goal("发布观察"), &silent).await,
            Err(MatterFailure::Invalid(message)) if message.contains("找不到用户说的话")
        ));
        let goal = manager
            .add(&add_goal("发布观察"), &Caller::Operator)
            .await
            .unwrap();

        // 推进：next / wait / resume 不要原话。
        let request = |next: Option<&str>, waiting_for: Option<&str>| MatterActionRequest {
            next: next.map(str::to_string),
            waiting_for: waiting_for.map(str::to_string),
            ..MatterActionRequest::default()
        };
        let next = manager
            .apply(
                &goal.id,
                MatterVerb::Next,
                &request(Some("提工单"), None),
                &silent,
            )
            .await
            .unwrap();
        assert_eq!(next.revision, 2);
        let waiting = manager
            .apply(
                &goal.id,
                MatterVerb::Wait,
                &request(None, Some("SRE 审批")),
                &silent,
            )
            .await
            .unwrap();
        assert_eq!(waiting.state, MatterState::Waiting);
        let resumed = manager
            .apply(&goal.id, MatterVerb::Resume, &request(None, None), &silent)
            .await
            .unwrap();
        assert_eq!(resumed.state, MatterState::Active);
        assert!(resumed.evidence.is_empty(), "推进不追加证据");

        // 承诺：park / done / drop / edit 都要原话。
        for verb in [MatterVerb::Park, MatterVerb::Done, MatterVerb::Drop] {
            assert!(
                matches!(
                    manager
                        .apply(&goal.id, verb, &MatterActionRequest::default(), &silent)
                        .await,
                    Err(MatterFailure::Invalid(_))
                ),
                "{verb}"
            );
        }
        // 有原话的 Run：done 落库，证据追加。
        let done = manager
            .apply(
                &goal.id,
                MatterVerb::Done,
                &MatterActionRequest::default(),
                &in_run("run-user"),
            )
            .await
            .unwrap();
        assert_eq!(done.state, MatterState::Done);
        assert_eq!(cited(&done), ["evt-1"]);
        assert_eq!(manager.get(&goal.id).await.unwrap(), done);
    }

    /// 后台任务会话：承诺类操作直接拒（哪怕有"用户输入"——那是主 Agent 写的任务正文），推进照常。
    #[tokio::test]
    async fn a_background_task_session_cannot_change_a_commitment() {
        let (manager, _dir) = manager().await;
        let task = Caller::new(
            Some(SessionId::from_raw("sess-1")),
            Some(RunId::from_raw("run-user")),
            true,
        )
        .unwrap();
        assert!(matches!(
            manager.add(&add_goal("发布观察"), &task).await,
            Err(MatterFailure::Invalid(message)) if message.contains("后台任务")
        ));
        let goal = manager
            .add(&add_goal("发布观察"), &Caller::Operator)
            .await
            .unwrap();
        assert!(
            manager
                .apply(
                    &goal.id,
                    MatterVerb::Park,
                    &MatterActionRequest::default(),
                    &task
                )
                .await
                .is_err()
        );
        let waiting = manager
            .apply(
                &goal.id,
                MatterVerb::Wait,
                &MatterActionRequest {
                    waiting_for: Some("审批".into()),
                    ..MatterActionRequest::default()
                },
                &task,
            )
            .await
            .unwrap();
        assert_eq!(waiting.state, MatterState::Waiting);
    }

    /// session / run 只给一个、文字不合格、动词带了别人的字段、预期版本不符、种类对不上：都拒。
    #[tokio::test]
    async fn malformed_requests_are_refused() {
        let (manager, _dir) = manager().await;
        assert!(Caller::new(Some(SessionId::from_raw("sess-1")), None, false).is_err());
        assert!(Caller::new(None, Some(RunId::from_raw("run-user")), false).is_err());

        for title in [
            "   ",
            "两行\n标题",
            "OpenAI key 是 sk-abc123",
            &"长".repeat(81),
        ] {
            assert!(
                manager
                    .add(&add_goal(title), &Caller::Operator)
                    .await
                    .is_err(),
                "{title:?}"
            );
        }
        assert!(manager.list(None, true).await.unwrap().is_empty());

        let goal = manager
            .add(&add_goal("发布观察"), &Caller::Operator)
            .await
            .unwrap();
        let park_with_title = MatterActionRequest {
            title: Some("x".into()),
            ..MatterActionRequest::default()
        };
        assert!(
            manager
                .apply(
                    &goal.id,
                    MatterVerb::Park,
                    &park_with_title,
                    &Caller::Operator
                )
                .await
                .is_err()
        );
        let stale = MatterActionRequest {
            expected_revision: Some(9),
            ..MatterActionRequest::default()
        };
        assert!(matches!(
            manager
                .apply(&goal.id, MatterVerb::Park, &stale, &Caller::Operator)
                .await,
            Err(MatterFailure::Repo(RepoError::VersionConflict {
                expected: 9,
                actual: 1
            }))
        ));
        let as_area = MatterActionRequest {
            kind: Some(MatterKind::Area),
            title: Some("新标题".into()),
            ..MatterActionRequest::default()
        };
        assert!(
            manager
                .apply(&goal.id, MatterVerb::Edit, &as_area, &Caller::Operator)
                .await
                .is_err()
        );
        let wait_without_what = MatterActionRequest::default();
        assert!(
            manager
                .apply(
                    &goal.id,
                    MatterVerb::Wait,
                    &wait_without_what,
                    &Caller::Operator
                )
                .await
                .is_err()
        );
        assert_eq!(manager.get(&goal.id).await.unwrap().revision, 1);
    }

    /// 对 parked 再 park：原样返回、不升版，Run 里也不追加证据。
    #[tokio::test]
    async fn an_unchanged_state_returns_the_matter_as_is() {
        let (manager, _dir) = manager().await;
        let goal = manager
            .add(&add_goal("发布观察"), &Caller::Operator)
            .await
            .unwrap();
        let park = MatterActionRequest::default();
        let parked = manager
            .apply(&goal.id, MatterVerb::Park, &park, &Caller::Operator)
            .await
            .unwrap();
        assert_eq!(parked.revision, 2);
        let again = manager
            .apply(&goal.id, MatterVerb::Park, &park, &in_run("run-user"))
            .await
            .unwrap();
        assert_eq!(again, parked);
        assert!(again.evidence.is_empty());
    }
}
