//! `sessions` 与 `session_log_index`：store 内部的具体类型，只被 Coordinator 用（§13.5）。
//!
//! 这里的函数都接 `&mut dyn Executor`，所以调用方能把它们和别的写一起放进**同一个**
//! 事务——§8.5 的「state.db 事务写入事件引用、queued 与 applied_seq」说的就是一个事务。

use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{Seq, SessionId};
use komo_kernel::types::status::SessionState;
use std::collections::BTreeMap;
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, map_toasty, to_ts};
use crate::models::{SessionKind, SessionLogIndexRow, SessionRow};
use crate::session_log::AppendedEvent;

/// 一个 Session 的元数据，读出来的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub session: SessionId,
    pub title: String,
    pub origin: String,
    /// 归属的助手（`AgentProfile.id`）。**空串 = 还没有归属**（升级前建的行），不是
    /// "默认 Agent"。
    pub agent_id: String,
    /// 用途（[`SessionKind`]）。
    pub kind: SessionKind,
    pub workdir: Option<String>,
    pub current_run: Option<String>,
    pub jsonl_path: String,
    pub applied_seq: Seq,
    pub applied_bytes: u64,
    /// 生命周期状态（§8.10）。
    pub state: SessionState,
    /// 状态变更时刻（写库时间；`0` 读成 Unix 纪元）。
    pub state_changed_at: OffsetDateTime,
}

impl SessionRecord {
    /// 从行读出来。**`state` / `kind` 认不出就是损坏**（§8.10 第 1 条）：默认成 `active`
    /// 会把 `deleted` / `purged` 的墓碑读成活的，默认成 `normal` 会让一条主会话看起来是
    /// 普通会话。
    fn try_from_row(row: &SessionRow) -> Result<SessionRecord, StoreError> {
        Ok(SessionRecord {
            session: SessionId::from_raw(row.id.clone()),
            title: row.title.clone(),
            origin: row.origin.clone(),
            agent_id: row.agent_id.clone(),
            kind: kind_of_row(row)?,
            workdir: row.workdir.clone(),
            current_run: row.current_run.clone(),
            jsonl_path: row.jsonl_path.clone(),
            applied_seq: Seq(row.applied_seq.max(0) as u64),
            applied_bytes: row.applied_bytes.max(0) as u64,
            state: state_of_row(row)?,
            state_changed_at: crate::db::from_ts(row.state_changed_at),
        })
    }

    /// 这个会话接不接受新输入（§8.10）。
    pub fn accepts_input(&self) -> bool {
        self.state.accepts_input()
    }
}

/// 一行里的用途。**认不出的值报错**，不挑默认值。
pub fn kind_of_row(row: &SessionRow) -> Result<SessionKind, StoreError> {
    SessionKind::parse(&row.kind).ok_or_else(|| {
        StoreError::Corrupt(format!(
            "sessions.kind 认不出的值 {:?}（会话 {}）：只认 main / normal / task",
            row.kind, row.id
        ))
    })
}

/// 一行里的生命周期状态。**认不出的值报错**，不挑默认值。
pub fn state_of_row(row: &SessionRow) -> Result<SessionState, StoreError> {
    SessionState::parse(&row.state).ok_or_else(|| {
        StoreError::Corrupt(format!(
            "sessions.state 认不出的值 {:?}（会话 {}）：只认 active / closing / deleted / purged",
            row.state, row.id
        ))
    })
}

/// 读一个 Session 的元数据。
pub async fn get(db: &Db, session: &SessionId) -> Result<Option<SessionRecord>, StoreError> {
    let id = session.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let row = SessionRow::filter_by_id(&id)
                .first()
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            match row {
                Some(row) => Ok(Some(SessionRecord::try_from_row(&row)?)),
                None => Ok(None),
            }
        }) as BoxFuture<'_, Result<Option<SessionRecord>, StoreError>>
    })
    .await
}

/// 在事务里读一个 Session。
pub async fn get_in(
    ex: &mut dyn Executor,
    session: &SessionId,
) -> Result<Option<SessionRow>, StoreError> {
    SessionRow::filter_by_id(session.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 没有就建一行。**幂等**：已经在了就原样返回。
pub async fn ensure_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    origin: &str,
    jsonl_path: &str,
    now: OffsetDateTime,
) -> Result<SessionRow, StoreError> {
    ensure_owned_in(
        ex,
        session,
        origin,
        jsonl_path,
        "",
        SessionKind::Normal,
        now,
    )
    .await
}

/// 同上，但把**归属**一起写上（`docs/bot.md` §4.2）。
///
/// **幂等**，而且对归属是"先到先得"：行已经在了就原样返回，`agent_id` / `kind` 都不改写
/// ——一个会话的 `agent_id` 创建后不随消息路由变化，换助手要换会话，不是给一个会话换
/// 人格却保留它的全部上下文。`agent_id = ""` 是"还没有归属"：只有
/// [`set_agent_in`] / [`ensure_main_in`] 能把它认出去，而且只认一次。
pub async fn ensure_owned_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    origin: &str,
    jsonl_path: &str,
    agent_id: &str,
    kind: SessionKind,
    now: OffsetDateTime,
) -> Result<SessionRow, StoreError> {
    if let Some(row) = get_in(ex, session).await? {
        return Ok(row);
    }
    toasty::create!(SessionRow {
        id: session.as_str(),
        title: String::new(),
        origin,
        agent_id,
        kind: kind.as_str(),
        workdir: None as Option<String>,
        current_run: None as Option<String>,
        jsonl_path,
        applied_seq: 0_i64,
        applied_bytes: 0_i64,
        created_at: to_ts(now),
        updated_at: to_ts(now),
        // 新会话一律 `active`；状态变更时刻与建行时刻一致（§8.10 第 1 条）。
        state: SessionState::Active.as_str(),
        state_changed_at: to_ts(now),
    })
    .exec(ex)
    .await
    .map_err(map_toasty)
}

/// 把一个还没有归属的会话认给一个 Agent。**只写一次**。
///
/// 返回 `true` = 这次调用写进去了，`false` = 它已经有主（或者给的是空 `agent_id`：空串
/// 就是"还没有归属"，拿它当认领是调用方的错，这里当没写）。
///
/// §4.2 的「创建后不随消息路由变化」就落在这一条上：争的人再多，只有一个能写进去，后来
/// 的人读回原主而不是把这条会话改写一遍。会话不在 = [`StoreError::NotFound`]。
///
/// **这是 raw SQL 的第五处**，理由与 [`set_state_in`] 一样：toasty 的类型化 `UPDATE`
/// 拿不到受影响行数，而"我写进去了还是别人抢先了"只有行数答得出来（§8.2 那张表）。
pub async fn set_agent_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    agent_id: &str,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    let at = to_ts(now);
    let affected = toasty::sql::statement(
        r#"UPDATE sessions
              SET agent_id = ?1, updated_at = ?2
            WHERE id = ?3 AND agent_id = '' AND ?1 <> ''"#,
    )
    .bind(agent_id)
    .bind(at)
    .bind(session.as_str())
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    if affected == 1 {
        return Ok(true);
    }
    // 没写进去有两种：已经有人认过了（正常），或者根本没有这一行（调用方的错）。
    if get_in(ex, session).await?.is_none() {
        return Err(StoreError::NotFound {
            what: format!("session {session}"),
        });
    }
    Ok(false)
}

/// 认领会话（不带事务的入口），走 [`Db::with_write_retry`]。
pub async fn set_agent(db: &Db, session: &SessionId, agent_id: &str) -> Result<bool, StoreError> {
    let session = session.clone();
    let agent_id = agent_id.to_string();
    let now = OffsetDateTime::now_utc();
    db.with_write_retry(move |ex| {
        let (session, agent_id) = (session.clone(), agent_id.clone());
        Box::pin(async move { set_agent_in(ex, &session, &agent_id, now).await })
            as BoxFuture<'_, Result<bool, StoreError>>
    })
    .await
}

/// 一个 Agent 的主会话（[`main_for_agent_in`] 的答案）。
#[derive(Debug)]
pub struct MainSession {
    /// 那**一条**主会话。多条时是 id 最小的那条——id 是 UUIDv7，也就是最早建的那条。
    pub row: SessionRow,
    /// 多出来的主会话 id。索引（`models::INDEXES` 的 `sessions_main_per_agent`）建起来
    /// 之后正常情况下是空的；不为空只可能是升级过来的旧库在说话。
    pub duplicates: Vec<SessionId>,
}

/// 这个 Agent 的**主会话**：`kind = 'main'`、`agent_id` 相等、**未删除**（§4.2 的
/// `main_session(agent_id)`）。
///
/// **多于一条时取最早的那条，并把其余的报出来**（[`MainSession::duplicates`]）——不静默
/// 地在里面挑一条：两个不同的入口各建过一个主会话是操作者该知道的事，而不是库替它选。
/// `deleted` / `purged` 的不算：墓碑不是"有效主会话"，删掉之后要能再建一条。
pub async fn main_for_agent_in(
    ex: &mut dyn Executor,
    agent_id: &str,
) -> Result<Option<MainSession>, StoreError> {
    let rows = SessionRow::filter(
        SessionRow::fields()
            .agent_id()
            .eq(agent_id)
            .and(SessionRow::fields().kind().eq(SessionKind::Main.as_str())),
    )
    .exec(ex)
    .await
    .map_err(map_toasty)?;

    let mut live: Vec<SessionRow> = Vec::new();
    for row in rows {
        if matches!(
            state_of_row(&row)?,
            SessionState::Deleted | SessionState::Purged
        ) {
            continue;
        }
        live.push(row);
    }
    live.sort_by(|a, b| a.id.cmp(&b.id));

    let mut live = live.into_iter();
    let Some(row) = live.next() else {
        return Ok(None);
    };
    let duplicates = live
        .map(|row| SessionId::from_raw(row.id))
        .collect::<Vec<_>>();
    Ok(Some(MainSession { row, duplicates }))
}

/// 读这个 Agent 的主会话（不带事务的入口）。
pub async fn main_for_agent(db: &Db, agent_id: &str) -> Result<Option<MainSession>, StoreError> {
    let agent_id = agent_id.to_string();
    db.read(move |ex| {
        let agent_id = agent_id.clone();
        Box::pin(async move { main_for_agent_in(ex, &agent_id).await })
            as BoxFuture<'_, Result<Option<MainSession>, StoreError>>
    })
    .await
}

/// 这个 Agent 的主会话——没有就建一条。**并发唯一**。
///
/// `session` / `origin` / `jsonl_path` 是**候选**：只有真的要建、或者要把一行已经在库里的
/// 候选认领成主会话时才用得上。库里算数的是 `kind = 'main'` 那一行，不是候选 id。
///
/// 唯一性有两层，它们管的是两件不同的事：
///
/// 1. `sessions_main_per_agent`（`models::INDEXES`，部分唯一索引）是**存储层**的保证：
///    同一个 Agent 的第二条活主会话在库里写不进去，无论有几个入口、代码怎么写。
/// 2. 上面那次先查是**快路**：绝大多数调用在这一步就拿到已有那条，索引只是兜底。
///
/// 两层都要：光靠"先查再插"只是一段谁都能绕过去的代码——`ensure_owned_in` 就直接往表里写，
/// 丢掉索引之后同一 Agent 能留下好几条活主会话（见
/// `the_earliest_main_session_wins_and_the_rest_are_reported` 里丢掉索引的那一手）。
///
/// 输掉竞争的那个不报错：索引的违反是一条普通错误（不是可以重试的序列化失败），所以插入
/// 失败之后**再看一次后置条件**——真的存在一条活主会话就返回它，没有就原样报出去。判据是
/// 后置条件，不是错误文本（§8.2 说过不拿字符串当判据）。
pub async fn ensure_main_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    agent_id: &str,
    origin: &str,
    jsonl_path: &str,
    now: OffsetDateTime,
) -> Result<SessionRow, StoreError> {
    if let Some(main) = main_for_agent_in(ex, agent_id).await? {
        return Ok(main.row);
    }

    // 候选 id 已经有一行：升级路径上的那个全局主会话就是这样（它比 Agent 归属早存在）。
    // 认领它，而不是在旁边再建一个——否则老库升级之后会同时有两个"操作者的会话"。
    if get_in(ex, session).await?.is_some() {
        if !claim_main_in(ex, session, agent_id, now).await? {
            let owner = get_in(ex, session)
                .await?
                .map(|row| row.agent_id)
                .unwrap_or_default();
            return Err(StoreError::Other(format!(
                "会话 {session} 已经属于 Agent {owner}，不能再当 {agent_id} 的主会话"
            )));
        }
        return get_in(ex, session)
            .await?
            .ok_or_else(|| StoreError::NotFound {
                what: format!("session {session}"),
            });
    }

    match ensure_owned_in(
        ex,
        session,
        origin,
        jsonl_path,
        agent_id,
        SessionKind::Main,
        now,
    )
    .await
    {
        Ok(row) => Ok(row),
        Err(error) => settle_on_the_main_session(ex, agent_id, error).await,
    }
}

/// 建主会话那一步失败之后**看一眼后置条件**：这个 Agent 的主会话是不是已经有人建好了。
///
/// 是 → 返回它：输掉竞争**不是这次调用失败**——"这个 Agent 的主会话是哪条"已经有答案了。
/// 不是 → 把原来的错误原样报出去，一个字的失败都不吞。
///
/// 判据是后置条件，**不是错误文本**（§8.2 说过不拿字符串当判据）：索引的违反是一条普通
/// 错误（`UNIQUE constraint failed: sessions.agent_id`），[`Db::with_write_retry`] 不会重试
/// 它——它只认序列化失败。
async fn settle_on_the_main_session(
    ex: &mut dyn Executor,
    agent_id: &str,
    error: StoreError,
) -> Result<SessionRow, StoreError> {
    match main_for_agent_in(ex, agent_id).await {
        Ok(Some(main)) => Ok(main.row),
        _ => Err(error),
    }
}

/// 把一行已经在库里的候选**认成**这个 Agent 的主会话。
///
/// 返回 `false` = 它已经是**别人**的主会话（`agent_id` 是别人；空串不算，空串是"还没有
/// 归属"，正是要认的那一种）。已经是这个 Agent 的主会话时也返回 `true`。
async fn claim_main_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    agent_id: &str,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    let at = to_ts(now);
    let affected = toasty::sql::statement(
        r#"UPDATE sessions
              SET agent_id = ?1, kind = ?2, updated_at = ?3
            WHERE id = ?4 AND (agent_id = '' OR agent_id = ?1)"#,
    )
    .bind(agent_id)
    .bind(SessionKind::Main.as_str())
    .bind(at)
    .bind(session.as_str())
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(affected == 1)
}

/// 拿这个 Agent 的主会话（没有就建一条）——不带事务的入口，走 [`Db::with_write_retry`]。
///
/// 失败之后再看一眼后置条件（与 [`ensure_main_in`] 同一个道理，但这次是**新的一次读**：
/// 竞争的赢家可能是在我们那个快照**之后**才提交的，事务里那一次读看不见它）。真的存在一条
/// 活主会话就返回它，否则把原来的错误报出去。
pub async fn ensure_main(
    db: &Db,
    session: &SessionId,
    agent_id: &str,
    origin: &str,
    jsonl_path: &str,
) -> Result<SessionRow, StoreError> {
    let session = session.clone();
    let agent_id = agent_id.to_string();
    let origin = origin.to_string();
    let jsonl_path = jsonl_path.to_string();
    let now = OffsetDateTime::now_utc();
    let for_closure = agent_id.clone();
    let outcome = db
        .with_write_retry(move |ex| {
            let (session, agent_id, origin, jsonl_path) = (
                session.clone(),
                for_closure.clone(),
                origin.clone(),
                jsonl_path.clone(),
            );
            Box::pin(async move {
                ensure_main_in(ex, &session, &agent_id, &origin, &jsonl_path, now).await
            }) as BoxFuture<'_, Result<SessionRow, StoreError>>
        })
        .await;
    match outcome {
        Ok(row) => Ok(row),
        Err(error) => match main_for_agent(db, &agent_id).await {
            Ok(Some(main)) => Ok(main.row),
            _ => Err(error),
        },
    }
}

/// 在事务里读一个 Session 的生命周期状态。行不在 = `None`。
pub async fn state_in(
    ex: &mut dyn Executor,
    session: &SessionId,
) -> Result<Option<SessionState>, StoreError> {
    let Some(row) = get_in(ex, session).await? else {
        return Ok(None);
    };
    state_of_row(&row).map(Some)
}

/// 读一个 Session 的生命周期状态。行不在 = `None`；值认不出 = 错误（§8.10）。
pub async fn state(db: &Db, session: &SessionId) -> Result<Option<SessionState>, StoreError> {
    let id = session.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let Some(row) = SessionRow::filter_by_id(&id)
                .first()
                .exec(ex)
                .await
                .map_err(map_toasty)?
            else {
                return Ok(None);
            };
            state_of_row(&row).map(Some)
        }) as BoxFuture<'_, Result<Option<SessionState>, StoreError>>
    })
    .await
}

/// **条件**推进生命周期状态：只有当前状态**恰好是** `from` 才写成 `to`。
///
/// 返回 `true` = 这次调用推进了它（`rows affected == 1`），`false` = 状态已经不是 `from`
/// 了，什么都没改。`from` 是 CAS：重跑一次不会把 `deleted` 盖回 `closing`——`komo
/// session delete`（`active → closing`）与 `purge`（`deleted → purged`）都可能被重试或
/// 并发调用，只有受影响行数能分辨"我推进了"与"别人已经推过了"（§8.10）。
///
/// **`purged` 是墓碑，没有出口**：内容已经删了，把它推回任何一个前面的状态都只会得到一个
/// 说不清自己内容的会话，所以那一类请求一律 `false`。§8.10 的"`purged` 之后没有任何路径再
/// 创建那个目录"要从这里就开始兜住，不能只靠调用方自觉。
///
/// **这是 raw SQL 的第四处**，理由与 §8.2 表里那句一样：toasty 的类型化 `UPDATE` 拿不到
/// 受影响行数，`rows affected` 是这里唯一可用的信号。
pub async fn set_state_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    from: SessionState,
    to: SessionState,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    if from == SessionState::Purged {
        return Ok(false);
    }
    let at = to_ts(now);
    let affected = toasty::sql::statement(
        r#"UPDATE sessions
              SET state = ?1, state_changed_at = ?2, updated_at = ?2
            WHERE id = ?3 AND state = ?4"#,
    )
    .bind(to.as_str())
    .bind(at)
    .bind(session.as_str())
    .bind(from.as_str())
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(affected == 1)
}

/// 推进 `applied_seq` / `applied_bytes`。
///
/// **只能推进连续、已校验的前缀**（§8.5），所以它只往前走：给一个比现在小的 seq 是
/// 调用方的错误，这里直接忽略而不是往回退——回退会让一段已经索引过的历史看起来又没
/// 索引过。
pub async fn advance_applied_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    seq: Seq,
    bytes: u64,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let Some(mut row) = get_in(ex, session).await? else {
        return Err(StoreError::NotFound {
            what: format!("session {session}"),
        });
    };
    let next = i64::try_from(seq.0).unwrap_or(i64::MAX);
    if next <= row.applied_seq {
        return Ok(());
    }
    row.update()
        .applied_seq(next)
        .applied_bytes(i64::try_from(bytes).unwrap_or(i64::MAX))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 记一个 Session 的工作目录。
///
/// `ensure_in` 建行时永远写 `None`（它不知道），所以这条路是后补的那一次。**幂等且
/// 只在有值时写**：`None` 是"不改"，不是"清空"——一个已经绑好目录的会话不该因为下一
/// 条输入没带目录就回到 `workspaces/`。
pub async fn set_workdir_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    workdir: &str,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let Some(mut row) = get_in(ex, session).await? else {
        return Err(StoreError::NotFound {
            what: format!("session {session}"),
        });
    };
    if row.workdir.as_deref() == Some(workdir) {
        return Ok(());
    }
    row.update()
        .workdir(Some(workdir.to_string()))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 记一个 Session 的工作目录（不带事务的入口）。
///
/// 与 [`set_workdir_in`] 是同一件事，给手里只有一个 [`Db`] 的调用方用（`POST /v1/sessions`
/// 创建会话时那一次）。它走 [`Db::with_write_retry`]，每个写都在一个 `BEGIN CONCURRENT`
/// 事务里（§8.2），与显式事务里的那一份写法一致。
pub async fn set_workdir(db: &Db, session: &SessionId, workdir: &str) -> Result<(), StoreError> {
    let session = session.clone();
    let workdir = workdir.to_string();
    let now = OffsetDateTime::now_utc();
    db.with_write_retry(move |ex| {
        let (session, workdir) = (session.clone(), workdir.clone());
        Box::pin(async move { set_workdir_in(ex, &session, &workdir, now).await })
            as BoxFuture<'_, Result<(), StoreError>>
    })
    .await
}

/// 会话的标题**只写一次**：空着才写（§12 的 `sessions.title`，`komo session list` 那一列）。
///
/// 已经有标题就不动：后来的消息不该把这一行改掉——它是这个会话在列表里的名字，不是
/// 最后一次说话的内容。
pub async fn set_title_if_empty_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    title: &str,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let Some(mut row) = get_in(ex, session).await? else {
        return Err(StoreError::NotFound {
            what: format!("session {session}"),
        });
    };
    if !row.title.trim().is_empty() {
        return Ok(());
    }
    row.update()
        .title(title)
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 记一个 Session 的当前 Run。
pub async fn set_current_run_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    run: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let Some(mut row) = get_in(ex, session).await? else {
        return Err(StoreError::NotFound {
            what: format!("session {session}"),
        });
    };
    row.update()
        .current_run(run)
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 把一条已经落盘的事件记进 `session_log_index`。
///
/// 按 `event_id` 幂等：重复提交同一 ID 不报错，也不改坐标（§8.3）。
pub async fn index_event_in(
    ex: &mut dyn Executor,
    appended: &AppendedEvent,
) -> Result<(), StoreError> {
    let id = appended.event.event_id.to_string();
    let existing = SessionLogIndexRow::filter_by_id(&id)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    if existing.is_some() {
        return Ok(());
    }
    toasty::create!(SessionLogIndexRow {
        id,
        session_id: appended.event.session.as_str(),
        seq: i64::try_from(appended.event.seq.0).unwrap_or(i64::MAX),
        run_id: appended.event.run.as_ref().map(|r| r.to_string()),
        event_type: appended.event.type_name(),
        byte_offset: i64::try_from(appended.byte_offset).unwrap_or(i64::MAX),
        byte_len: i64::try_from(appended.byte_len).unwrap_or(i64::MAX),
        digest: appended.digest.clone(),
    })
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(())
}

/// 一个 Session 已提交记录的摘要，给 [`crate::session_log::TailExpectation`] 用。
pub async fn digests(db: &Db, session: &SessionId) -> Result<BTreeMap<Seq, String>, StoreError> {
    let id = session.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let rows = SessionLogIndexRow::filter(
                SessionLogIndexRow::fields().session_id().eq(id.as_str()),
            )
            .exec(ex)
            .await
            .map_err(map_toasty)?;
            Ok(rows
                .into_iter()
                .map(|row| (Seq(row.seq.max(0) as u64), row.digest))
                .collect::<BTreeMap<_, _>>())
        }) as BoxFuture<'_, Result<BTreeMap<Seq, String>, StoreError>>
    })
    .await
}

/// Session 列表（按 id，也就是按创建时间——UUIDv7）。
///
/// `all = false`（默认）只列 `active` 与 `closing`：逻辑删除过的会话默认不列（§8.10；
/// `closing` 要列出来并标注"正在关闭"，它还在服务）。`all = true` 把 `deleted` 也带上
/// ——它是墓碑但还在列表语义里（可 `show`、可 `purge`）。**`purged` 两种都不列**：那一行
/// 只是"这个会话曾经存在"的记录，只在显式查看单个会话时可见（§8.10 的列表列）。
pub async fn list(db: &Db, all: bool) -> Result<Vec<SessionRecord>, StoreError> {
    db.read(move |ex| {
        Box::pin(async move {
            let rows = SessionRow::all().exec(ex).await.map_err(map_toasty)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                let record = SessionRecord::try_from_row(row)?;
                let visible = match record.state {
                    SessionState::Active | SessionState::Closing => true,
                    SessionState::Deleted => all,
                    SessionState::Purged => false,
                };
                if visible {
                    out.push(record);
                }
            }
            out.sort_by(|a, b| a.session.as_str().cmp(b.session.as_str()));
            Ok(out)
        }) as BoxFuture<'_, Result<Vec<SessionRecord>, StoreError>>
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::{EVENT_FORMAT_VERSION, EventPayload, MessageUser};
    use komo_kernel::types::ids::EventId;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    fn appended(seq: u64, event: &str) -> AppendedEvent {
        AppendedEvent {
            event: komo_kernel::events::Event {
                v: EVENT_FORMAT_VERSION,
                seq: Seq(seq),
                event_id: EventId::from_raw(event),
                session: SessionId::from_raw("sess-1"),
                run: None,
                ts: NOW,
                payload: EventPayload::MessageUser(MessageUser {
                    text: Some("你好".into()),
                    text_ref: None,
                }),
            },
            byte_offset: seq * 100,
            byte_len: 100,
            digest: format!("{seq:064}"),
        }
    }

    async fn write<F>(db: &Db, op: F)
    where
        F: for<'a> Fn(&'a mut dyn Executor) -> BoxFuture<'a, Result<(), StoreError>>
            + Send
            + Sync
            + Clone
            + 'static,
    {
        db.with_write_retry(move |ex| op(ex)).await.unwrap();
    }

    #[tokio::test]
    async fn ensuring_a_session_twice_keeps_one_row() {
        let (db, _dir) = temp().await;
        write(&db, |ex| {
            Box::pin(async move {
                ensure_in(
                    ex,
                    &SessionId::from_raw("sess-1"),
                    "api",
                    "sessions/sess-1/events.jsonl",
                    NOW,
                )
                .await
                .map(|_| ())
            })
        })
        .await;
        write(&db, |ex| {
            Box::pin(async move {
                ensure_in(
                    ex,
                    &SessionId::from_raw("sess-1"),
                    "feishu",
                    "别的路径",
                    NOW,
                )
                .await
                .map(|_| ())
            })
        })
        .await;

        let all = list(&db, false).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].origin, "api", "已经在了就原样返回，不改写来源");
    }

    /// `applied_seq` 只能推进连续、已校验的前缀——**不往回退**（§8.5）。
    #[tokio::test]
    async fn applied_seq_only_moves_forward() {
        let (db, _dir) = temp().await;
        write(&db, |ex| {
            Box::pin(async move {
                ensure_in(ex, &SessionId::from_raw("sess-1"), "api", "p", NOW)
                    .await
                    .map(|_| ())
            })
        })
        .await;

        write(&db, |ex| {
            Box::pin(async move {
                advance_applied_in(ex, &SessionId::from_raw("sess-1"), Seq(5), 500, NOW).await
            })
        })
        .await;
        write(&db, |ex| {
            Box::pin(async move {
                advance_applied_in(ex, &SessionId::from_raw("sess-1"), Seq(2), 200, NOW).await
            })
        })
        .await;

        let record = get(&db, &SessionId::from_raw("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.applied_seq, Seq(5));
        assert_eq!(record.applied_bytes, 500);
    }

    /// 索引按 `event_id` 幂等：重复提交同一 ID 不改坐标（§8.3）。
    #[tokio::test]
    async fn indexing_the_same_event_twice_keeps_the_first_coordinates() {
        let (db, _dir) = temp().await;
        write(&db, |ex| {
            Box::pin(async move { index_event_in(ex, &appended(1, "evt-1")).await })
        })
        .await;

        let mut moved = appended(1, "evt-1");
        moved.byte_offset = 999;
        moved.digest = "f".repeat(64);
        let moved_for_tx = moved.clone();
        db.with_write_retry(move |ex| {
            let moved = moved_for_tx.clone();
            Box::pin(async move { index_event_in(ex, &moved).await })
                as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let digests = digests(&db, &SessionId::from_raw("sess-1")).await.unwrap();
        assert_eq!(digests.get(&Seq(1)).unwrap(), &format!("{:064}", 1));
    }

    #[tokio::test]
    async fn digests_are_keyed_by_seq() {
        let (db, _dir) = temp().await;
        for seq in 1..=3 {
            let event = appended(seq, &format!("evt-{seq}"));
            db.with_write_retry(move |ex| {
                let event = event.clone();
                Box::pin(async move { index_event_in(ex, &event).await })
                    as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .unwrap();
        }
        let digests = digests(&db, &SessionId::from_raw("sess-1")).await.unwrap();
        assert_eq!(digests.len(), 3);
        assert_eq!(digests[&Seq(2)], format!("{:064}", 2));
    }

    async fn ensure(db: &Db, id: &str) {
        let id = id.to_string();
        db.with_write_retry(move |ex| {
            let id = id.clone();
            Box::pin(async move {
                ensure_in(ex, &SessionId::from_raw(id), "api", "p", NOW)
                    .await
                    .map(|_| ())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
    }

    async fn set_state(db: &Db, id: &str, from: SessionState, to: SessionState) -> bool {
        let id = id.to_string();
        db.with_write_retry(move |ex| {
            let id = id.clone();
            Box::pin(async move { set_state_in(ex, &SessionId::from_raw(id), from, to, NOW).await })
                as BoxFuture<'_, Result<bool, StoreError>>
        })
        .await
        .unwrap()
    }

    /// 新建的会话是 `active`，而且状态变更时刻就是建行时刻（§8.10 第 1 条）。
    #[tokio::test]
    async fn a_new_session_starts_active() {
        let (db, _dir) = temp().await;
        ensure(&db, "sess-1").await;
        let record = get(&db, &SessionId::from_raw("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, SessionState::Active);
        assert!(record.accepts_input());
        assert_eq!(record.state_changed_at, NOW);
    }

    /// `set_state_in` 是 CAS：`from` 对不上就什么都不改。
    ///
    /// 重跑一次逻辑删除（`active → closing`）不能再改一次时间；`purge` 的重跑更不能把
    /// `deleted` 盖回 `closing`——那会让一个已经删掉的会话看起来又在服务（§8.10）。
    #[tokio::test]
    async fn set_state_only_advances_from_the_expected_state() {
        let (db, _dir) = temp().await;
        ensure(&db, "sess-1").await;

        assert!(
            set_state(&db, "sess-1", SessionState::Active, SessionState::Closing).await,
            "第一次受理逻辑删除：推进了"
        );
        assert!(
            !set_state(&db, "sess-1", SessionState::Active, SessionState::Closing).await,
            "重跑：状态已经不是 active，什么都不改"
        );
        assert!(
            set_state(&db, "sess-1", SessionState::Closing, SessionState::Deleted).await,
            "没有未完成的 Run 时由 reconcile 推进到 deleted"
        );
        assert!(
            !set_state(&db, "sess-1", SessionState::Closing, SessionState::Deleted).await,
            "重跑：不重复推进"
        );
        // `komo session delete` 的重跑是 `active → closing`：对已经 `deleted` 的行不生效，
        // 所以它**不会把墓碑盖回 `closing`**（§8.10）。
        assert!(
            !set_state(&db, "sess-1", SessionState::Active, SessionState::Closing).await,
            "**不许把 deleted 盖回 closing**"
        );

        // 墓碑只能往回收方向走，而且重跑幂等。
        assert!(
            set_state(&db, "sess-1", SessionState::Deleted, SessionState::Purged).await,
            "deleted → purged 是最后一步"
        );
        assert!(
            !set_state(&db, "sess-1", SessionState::Deleted, SessionState::Purged).await,
            "回收重跑什么都不改"
        );
        assert!(!set_state(&db, "sess-1", SessionState::Purged, SessionState::Active).await);

        let record = get(&db, &SessionId::from_raw("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, SessionState::Purged);
        assert!(!record.accepts_input());
    }

    /// 列里出现认不出的状态值 = 损坏，**不默认成 active**：默认会把墓碑读成活会话。
    #[tokio::test]
    async fn an_unknown_state_value_is_corruption_not_active() {
        let (db, _dir) = temp().await;
        ensure(&db, "sess-1").await;
        let bad = db.clone();
        bad.with_write_retry(|ex| {
            Box::pin(async move {
                toasty::sql::statement("UPDATE sessions SET state = 'zombie' WHERE id = 'sess-1'")
                    .exec(ex)
                    .await
                    .map(|_| ())
                    .map_err(map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let error = get(&db, &SessionId::from_raw("sess-1"))
            .await
            .expect_err("认不出的状态要报损坏");
        assert!(
            matches!(&error, StoreError::Corrupt(why) if why.contains("zombie")),
            "错误要说清是哪个值：{error}"
        );
        assert!(state(&db, &SessionId::from_raw("sess-1")).await.is_err());
    }

    /// 默认只列 `active` / `closing`；`all` 带上 `deleted`；`purged` 两种都不列（§8.10）。
    #[tokio::test]
    async fn deleted_sessions_are_hidden_unless_asked_for() {
        let (db, _dir) = temp().await;
        for id in ["sess-active", "sess-closing", "sess-deleted", "sess-purged"] {
            ensure(&db, id).await;
        }
        assert!(
            set_state(
                &db,
                "sess-closing",
                SessionState::Active,
                SessionState::Closing
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-deleted",
                SessionState::Active,
                SessionState::Closing
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-deleted",
                SessionState::Closing,
                SessionState::Deleted
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-purged",
                SessionState::Active,
                SessionState::Closing
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-purged",
                SessionState::Closing,
                SessionState::Deleted
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-purged",
                SessionState::Deleted,
                SessionState::Purged
            )
            .await
        );

        let visible: Vec<String> = list(&db, false)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.session.to_string())
            .collect();
        assert_eq!(visible, vec!["sess-active", "sess-closing"]);

        let all: Vec<String> = list(&db, true)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.session.to_string())
            .collect();
        assert_eq!(
            all,
            vec!["sess-active", "sess-closing", "sess-deleted"],
            "墓碑行只在显式查看单个会话时可见"
        );
    }

    /// 建一条带**归属**的会话（测试用的快捷方式：`ensure_owned_in` 在一个写事务里跑）。
    async fn ensure_owned(
        db: &Db,
        id: &str,
        agent_id: &str,
        kind: SessionKind,
    ) -> Result<(), StoreError> {
        let (id, agent_id) = (id.to_string(), agent_id.to_string());
        db.with_write_retry(move |ex| {
            let (id, agent_id) = (id.clone(), agent_id.clone());
            Box::pin(async move {
                ensure_owned_in(
                    ex,
                    &SessionId::from_raw(id),
                    "api",
                    "p",
                    &agent_id,
                    kind,
                    NOW,
                )
                .await
                .map(|_| ())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
    }

    /// 库里这个 Agent 有多少条**活的**主会话行。
    ///
    /// 走原始 SQL 是有意的：这里要问的正是"库里到底有几条"，经过
    /// [`main_for_agent_in`] 就会被它自己的取舍（取最早的那条）盖过去。
    async fn live_main_rows(db: &Db, agent_id: &str) -> Vec<String> {
        let agent_id = agent_id.to_string();
        db.read(move |ex| {
            let agent_id = agent_id.clone();
            Box::pin(async move {
                let rows = toasty::sql::query(
                    "SELECT id FROM sessions
                      WHERE kind = 'main' AND agent_id = ?1 AND state NOT IN ('deleted', 'purged')
                      ORDER BY id",
                )
                .bind(agent_id)
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                Ok(rows
                    .iter()
                    .filter_map(|row| crate::db::column_string(row, 0))
                    .collect::<Vec<_>>())
            }) as BoxFuture<'_, Result<Vec<String>, StoreError>>
        })
        .await
        .unwrap()
    }

    /// `set_agent_in` **只写一次**：认给 `a` 之后再认给 `b`，读回来还是 `a`，而且第二次
    /// 返回 `false`（§4.2：一个会话的 `agent_id` 创建后不随消息路由变化——换助手是换会话，
    /// 不是给一个会话换人格却留着它的全部上下文）。
    #[tokio::test]
    async fn set_agent_in_writes_the_owner_once() {
        let (db, _dir) = temp().await;
        ensure(&db, "sess-1").await;

        assert!(
            set_agent(&db, &SessionId::from_raw("sess-1"), "a")
                .await
                .unwrap(),
            "还没有归属：第一次认领写进去了"
        );
        assert_eq!(
            get(&db, &SessionId::from_raw("sess-1"))
                .await
                .unwrap()
                .unwrap()
                .agent_id,
            "a"
        );

        assert!(
            !set_agent(&db, &SessionId::from_raw("sess-1"), "b")
                .await
                .unwrap(),
            "已经有主：第二次什么都没写"
        );
        assert_eq!(
            get(&db, &SessionId::from_raw("sess-1"))
                .await
                .unwrap()
                .unwrap()
                .agent_id,
            "a",
            "后到的那个不该把这一行改写一遍"
        );

        // 空串是"还没有归属"，不是"认给谁"：拿它认领等于什么都没说。
        ensure(&db, "sess-2").await;
        assert!(
            !set_agent(&db, &SessionId::from_raw("sess-2"), "")
                .await
                .unwrap()
        );
        assert_eq!(
            get(&db, &SessionId::from_raw("sess-2"))
                .await
                .unwrap()
                .unwrap()
                .agent_id,
            ""
        );

        // 认领一个不存在的会话是调用方的错，不是"没写进去"。
        assert!(matches!(
            set_agent(&db, &SessionId::from_raw("没有这个会话"), "a").await,
            Err(StoreError::NotFound { .. })
        ));
    }

    /// 两个 Agent 各有各的主会话，互不干扰；再问一次拿到的是**同一条**（幂等），第二个
    /// 候选 id 根本用不上。
    #[tokio::test]
    async fn two_agents_have_one_main_session_each() {
        let (db, _dir) = temp().await;
        let assistant = ensure_main(
            &db,
            &SessionId::from_raw("s-assistant"),
            "assistant",
            "home",
            "p",
        )
        .await
        .unwrap();
        let coder = ensure_main(&db, &SessionId::from_raw("s-coder"), "coder", "home", "p")
            .await
            .unwrap();
        assert_ne!(assistant.id, coder.id);
        assert_eq!(assistant.kind, SessionKind::Main.as_str());
        assert_eq!(assistant.agent_id, "assistant");
        assert_eq!(coder.agent_id, "coder");

        let again = ensure_main(
            &db,
            &SessionId::from_raw("s-别的候选"),
            "assistant",
            "home",
            "p",
        )
        .await
        .unwrap();
        assert_eq!(again.id, "s-assistant", "已经有了就不建第二条");

        assert_eq!(live_main_rows(&db, "assistant").await, vec!["s-assistant"]);
        assert_eq!(live_main_rows(&db, "coder").await, vec!["s-coder"]);

        // 普通会话不是谁的主会话；空 `agent_id` 也没有隐含的"默认 Agent"。
        ensure(&db, "s-plain").await;
        assert!(main_for_agent(&db, "").await.unwrap().is_none());
    }

    /// 同一 Agent 的两个**并发**调用只留一条（§4.2：「创建过程也应具有并发唯一性，而不是
    /// 多个入口各建一个，再挑最早的」）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_ensure_main_keeps_one_row() {
        let (db, _dir) = temp().await;

        let mut handles = Vec::new();
        for round in 0..4 {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                // 每个调用带自己的候选 id：赢的那个说了算。
                ensure_main(
                    &db,
                    &SessionId::from_raw(format!("cand-{round}")),
                    "assistant",
                    "home",
                    "p",
                )
                .await
            }));
        }
        let mut ids = Vec::new();
        for handle in handles {
            ids.push(
                handle
                    .await
                    .expect("并发任务没有 panic")
                    .expect("并发建主会话"),
            );
        }
        assert!(
            ids.windows(2).all(|pair| pair[0].id == pair[1].id),
            "几个并发调用说的必须是同一条主会话：{:?}",
            ids.iter().map(|row| row.id.clone()).collect::<Vec<_>>()
        );
        assert_eq!(
            live_main_rows(&db, "assistant").await,
            vec![ids[0].id.clone()],
            "库里只有一条"
        );
    }

    /// 唯一性不只是"调用方先查一遍"：**库里**也写不进第二条活主会话（§4.2「数据库应保证
    /// 每个 Agent 只有一个有效主会话」）。
    ///
    /// 这条与上一条管的是两件事：上一条是"两个并发调用最后说同一条"，这一条是"绕过先查再建
    /// 也塞不进第二条"——`ensure_owned_in` 不查主会话，直接把行写进去。
    #[tokio::test]
    async fn the_index_rejects_a_second_live_main_session() {
        let (db, _dir) = temp().await;
        ensure_main(&db, &SessionId::from_raw("m1"), "assistant", "home", "p")
            .await
            .unwrap();

        let second = ensure_owned(&db, "m2", "assistant", SessionKind::Main).await;
        assert!(
            second.is_err(),
            "第二条活主会话应当被 sessions_main_per_agent 挡下：{second:?}"
        );
        assert_eq!(live_main_rows(&db, "assistant").await, vec!["m1"]);
    }

    /// 主会话删掉之后**要能再建一条**：墓碑不是"有效主会话"（主会话索引里 `state` 那一条）。
    #[tokio::test]
    async fn a_deleted_main_session_does_not_block_a_new_one() {
        let (db, _dir) = temp().await;
        let first = ensure_main(&db, &SessionId::from_raw("m1"), "assistant", "home", "p")
            .await
            .unwrap();
        assert_eq!(first.id, "m1");
        assert!(set_state(&db, "m1", SessionState::Active, SessionState::Closing).await);
        assert!(set_state(&db, "m1", SessionState::Closing, SessionState::Deleted).await);
        assert!(main_for_agent(&db, "assistant").await.unwrap().is_none());

        let second = ensure_main(&db, &SessionId::from_raw("m2"), "assistant", "home", "p")
            .await
            .unwrap();
        assert_eq!(second.id, "m2");
        assert_eq!(live_main_rows(&db, "assistant").await, vec!["m2"]);
    }

    /// 候选 id 已经有一行时**认领**它，而不是在旁边再建一条：升级路径上那个
    /// `origin = 'home'` 的全局主会话就是这样（它比 Agent 归属早存在）。
    #[tokio::test]
    async fn ensure_main_adopts_the_session_under_the_candidate_id() {
        let (db, _dir) = temp().await;
        write(&db, |ex| {
            Box::pin(async move {
                ensure_in(ex, &SessionId::from_raw("legacy-home"), "home", "p", NOW)
                    .await
                    .map(|_| ())
            })
        })
        .await;

        let row = ensure_main(
            &db,
            &SessionId::from_raw("legacy-home"),
            "assistant",
            "home",
            "p",
        )
        .await
        .unwrap();
        assert_eq!(row.id, "legacy-home");
        assert_eq!(row.agent_id, "assistant");
        assert_eq!(row.kind, SessionKind::Main.as_str(), "认领之后它就是主会话");
        let record = get(&db, &SessionId::from_raw("legacy-home"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.kind, SessionKind::Main);
        assert_eq!(record.origin, "home", "认领不该动来源");

        // 幂等：再问一次还是它，不会又建一条。
        let again = ensure_main(
            &db,
            &SessionId::from_raw("另一个候选"),
            "assistant",
            "home",
            "p",
        )
        .await
        .unwrap();
        assert_eq!(again.id, "legacy-home");
        assert!(
            main_for_agent(&db, "assistant")
                .await
                .unwrap()
                .unwrap()
                .duplicates
                .is_empty()
        );
    }

    /// 候选 id 已经是**别人**的主会话时不挑一条了事，也不抢：把冲突说清楚。
    #[tokio::test]
    async fn ensure_main_refuses_a_candidate_that_belongs_to_another_agent() {
        let (db, _dir) = temp().await;
        ensure_main(
            &db,
            &SessionId::from_raw("coder-main"),
            "coder",
            "home",
            "p",
        )
        .await
        .unwrap();

        let stolen = ensure_main(
            &db,
            &SessionId::from_raw("coder-main"),
            "assistant",
            "home",
            "p",
        )
        .await;
        assert!(
            matches!(&stolen, Err(StoreError::Other(why)) if why.contains("coder")),
            "错误要说清它归谁：{stolen:?}"
        );
        assert!(
            live_main_rows(&db, "assistant").await.is_empty(),
            "冲突归冲突，别顺手给 assistant 建一条"
        );
    }

    /// 输掉竞争的那个不报错：插入失败之后**后置条件**成立（主会话已经有了），返回它。
    ///
    /// 竞争本身没法在测试里插进去：一个还没提交、又读过 `sessions` 的事务会把并发的写挤成
    /// `Contended`（turso 的读集校验；`with_write_retry` 会重跑它），摆不出"这个事务的
    /// 插入撞上别人刚提交的那一行"那一刻。所以这里直接喂一个"插入失败"，问那段判定该怎么
    /// 收场——它判的是后置条件，错误长什么样无关。
    #[tokio::test]
    async fn a_failed_insert_settles_on_the_main_session_that_is_already_there() {
        let (db, _dir) = temp().await;
        ensure_main(
            &db,
            &SessionId::from_raw("winner"),
            "assistant",
            "home",
            "p",
        )
        .await
        .unwrap();

        let settled = db
            .with_write_retry(|ex| {
                Box::pin(async move {
                    settle_on_the_main_session(
                        ex,
                        "assistant",
                        StoreError::Other("UNIQUE constraint failed: sessions.agent_id".into()),
                    )
                    .await
                }) as BoxFuture<'_, Result<SessionRow, StoreError>>
            })
            .await
            .expect("后置条件成立时这不是失败");
        assert_eq!(settled.id, "winner");

        // 后置条件**不**成立时原样报出去：别把真正的失败吞掉。
        let propagated = db
            .with_write_retry(|ex| {
                Box::pin(async move {
                    settle_on_the_main_session(ex, "coder", StoreError::Other("别的毛病".into()))
                        .await
                }) as BoxFuture<'_, Result<SessionRow, StoreError>>
            })
            .await;
        assert!(
            matches!(&propagated, Err(StoreError::Other(why)) if why == "别的毛病"),
            "没有主会话就该原样报错：{propagated:?}"
        );
    }

    /// 一个 Agent 的主会话多于一条时取**最早**的那条，并把其余的报出来——**不静默**地在
    /// 里面挑一条（§4.2）。
    ///
    /// 正常库里到不了这里（部分唯一索引拦着），所以这个测试先把索引丢掉，造出升级前的
    /// 那种库：`ensure_owned_in` 不经过 `ensure_main_in` 的"先查再建"，两条都写得进去。
    /// 内存库 + `pool_size = 1`：MVCC 下 DDL 要求这个库上没有别的连接（见 `Db::build`）。
    #[tokio::test]
    async fn the_earliest_main_session_wins_and_the_rest_are_reported() {
        let db = crate::db::Db::open_memory_with(crate::db::DbOptions {
            pool_size: 1,
            ..Default::default()
        })
        .await
        .unwrap();
        {
            let mut conn = db.handle().connection().await.unwrap();
            toasty::sql::statement(r#"DROP INDEX IF EXISTS "sessions_main_per_agent""#)
                .exec(&mut conn)
                .await
                .unwrap();
        }
        ensure_owned(&db, "m-late", "assistant", SessionKind::Main)
            .await
            .unwrap();
        ensure_owned(&db, "m-early", "assistant", SessionKind::Main)
            .await
            .unwrap();
        ensure_owned(&db, "m-other", "coder", SessionKind::Main)
            .await
            .unwrap();

        let main = main_for_agent(&db, "assistant")
            .await
            .unwrap()
            .expect("有主会话");
        assert_eq!(main.row.id, "m-early", "id 是 UUIDv7：最早建的那条最小");
        assert_eq!(main.duplicates, vec![SessionId::from_raw("m-late")]);
        let coder = main_for_agent(&db, "coder").await.unwrap().unwrap();
        assert_eq!(coder.row.id, "m-other");
        assert!(coder.duplicates.is_empty(), "另一个 Agent 的不算它的重复");
    }
}
