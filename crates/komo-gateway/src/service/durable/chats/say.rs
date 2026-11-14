//! 往对话里投一句话（`docs/redesign.md` §2.1、§2.7）：停住的运行、定时结果在对话里"开口"用的那一个原语。
//!
//! # entry 形状
//!
//! 复用 Router 时代的展示消息 `main.reply`（[`MAIN_REPLY_ENTRY`]，事件流里是一条 `LineReply`，不挂任何
//! Run）。区别只有一处：Router 时代的没有 `model`，这里带一条 user 消息——模型要在下一轮读到 Komo 自己
//! 说过什么，操作者回一句「发吧」才接得上。它是 write submission（不是输入）：**不启动 Run**；对话空闲时
//! 立刻落，对话正忙（有 Run 在跑）时排进收件箱、在下一个 boundary 落（write 的通常规则）。
//!
//! `data`：`text`（界面上那句话）、`at`（落下的时刻，事件投影读它）、`source`（`intervention` / `cron`）、
//! `key`（关联键，同时是请求键）与调用方给的别的关联字段（介入的 `handle`、定时任务 `job` / `firing`）。
//! `model` 里的文字比 `text` 多一行只给模型看的附注（内部句柄、能答什么），界面不显示。
//!
//! # 判重
//!
//! 关联键就是 write submission 的请求键（`submissions.request_id`，落库），在**目标 Session 的全部
//! conversation** 里按键找（与输入的请求键同一个口径）：同一个键投第二次什么都不写，返回
//! [`Said::Duplicate`]。判重靠已提交的 submission，所以重启后补投（介入清单回放、cron 看客重挂）不会重复；
//! 键换了目标（例如绑定的支线变了）算新的一句。

use komo_kernel::durable::{Message, SessionKind};
use komo_runtime::durable::DurableError;
use komo_runtime::durable::harness::inbox::Admitted;
use komo_runtime::durable::harness::{SubmissionDraft, WriteDraft};
use serde_json::{Map, Value, json};

use super::super::DurableCore;
use super::super::sessions::{SessionError, ensure_open};
use crate::push::PushMessage;

pub use komo_kernel::durable::legacy_events::MAIN_REPLY_ENTRY;

/// 这句话从哪来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaySource {
    /// 停住的运行（介入）。
    Intervention,
    /// 定时任务的结果。
    Cron,
}

impl SaySource {
    pub fn as_str(self) -> &'static str {
        match self {
            SaySource::Intervention => "intervention",
            SaySource::Cron => "cron",
        }
    }
}

/// 要投的一句话。
#[derive(Debug, Clone)]
pub struct Say {
    /// 界面上显示的话。
    pub text: String,
    /// 只给模型看的附注（接在 `text` 后面）。
    pub model_note: Option<String>,
    pub source: SaySource,
    /// 关联键 = 请求键：同一个键在同一个 Session 里只投一次。
    pub key: String,
    /// 别的关联字段（对象），并进 `data`。
    pub data: Value,
}

/// 投的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said {
    /// 落下了（对话忙时是排进了收件箱）。
    Posted,
    /// 这个键已经投过，什么都没写。
    Duplicate,
}

/// 给模型看的那条 user 消息：先说明这是 Komo 自己对操作者说的话，免得被当成操作者的发言。
fn model_text(say: &Say) -> String {
    let mut text = format!(
        "（以下是你，Komo，刚才主动对操作者说的一句话，不是操作者说的。）\n{}",
        say.text
    );
    if let Some(note) = &say.model_note {
        text.push_str("\n\n");
        text.push_str(note);
    }
    text
}

impl DurableCore {
    /// 在 `session` 的当前分支里追加一句 Komo 说的话（见模块文档）。不启动 Run；同一个关联键只投一次；
    /// 在同一次提交里更新会话行的 `last_message_at`。会话不收输入（`closing` / `deleted` / `purged`）是
    /// [`SessionError::NotOpen`]。
    pub async fn say_in(&self, session: &str, say: Say) -> Result<Said, SessionError> {
        let storage = self.storage.clone();
        let harness = self.harness.clone();
        let id = session.to_owned();
        // 提交的闭包会把 `say` 搬走；推送要用的几样先留一份。
        let announced = (say.source, say.text.clone(), say.key.clone());
        let said = self
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    // 先读后写：会话行与按键找的已有 submission 都在第一次表写之前读。
                    let record = match tx.session(&id).await? {
                        Some(record) => record,
                        None => return Ok(Err(SessionError::NotFound(format!("会话 {id}")))),
                    };
                    if let Err(error) = ensure_open(&record) {
                        return Ok(Err(error));
                    }
                    if storage
                        .submission_by_session_request(&id, &say.key)
                        .await?
                        .is_some()
                    {
                        return Ok(Ok(Said::Duplicate));
                    }
                    let now = tx.now();
                    let mut data = match &say.data {
                        Value::Object(map) => map.clone(),
                        _ => Map::new(),
                    };
                    data.insert("text".into(), json!(say.text));
                    data.insert("at".into(), json!(now));
                    data.insert("source".into(), json!(say.source.as_str()));
                    data.insert("key".into(), json!(say.key));
                    let entry = WriteDraft {
                        kind: MAIN_REPLY_ENTRY.to_owned(),
                        model: Some(vec![Message::user(model_text(&say), now)]),
                        data: Some(Value::Object(data)),
                        head: None,
                        edits: None,
                    };
                    let draft = SubmissionDraft::Write {
                        entry,
                        request_id: Some(say.key.clone()),
                    };
                    match harness
                        .admit_in_tx(tx, record.current_branch, &draft)
                        .await?
                    {
                        Admitted::Id(_) => {}
                        Admitted::Busy => {
                            return Err(DurableError::rejected("conversation 在受理时忙了"));
                        }
                    }
                    let mut record = record;
                    record.last_message_at = now;
                    tx.put_session(record);
                    Ok(Ok(Said::Posted))
                })
            })
            .await??;
        // 落下的才推：判重挡掉的（重启后补投）不会再响一次。
        if said == Said::Posted {
            self.push_say(session, announced).await;
        }
        Ok(said)
    }

    /// 这句话是要操作者看的（停住的提问、定时结果）：往订阅的设备推一条系统通知。后台发，失败不影响投递。
    async fn push_say(&self, session: &str, (source, text, key): (SaySource, String, String)) {
        let Some(state) = self.gateway() else { return };
        let Ok(record) = self.session(session).await else {
            return;
        };
        state.push.notify(PushMessage {
            title: match source {
                SaySource::Intervention => "Komo 在等你".to_owned(),
                SaySource::Cron => "Komo 的定时任务".to_owned(),
            },
            body: text,
            url: chat_url(&record),
            tag: Some(key),
        });
    }
}

/// 一段对话在网页里的站内路径：主对话 `/`，支线 `/chats/<会话 id>`（`apps/client` 的路由）。
pub(in crate::service::durable) fn chat_url(
    record: &komo_kernel::durable::SessionRecord,
) -> String {
    if record.kind == SessionKind::Main {
        "/".to_owned()
    } else {
        format!("/chats/{}", record.id)
    }
}
