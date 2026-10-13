//! codemode 子调用的 journal（`docs/codemode.md` §7）：按顺序、逐阶段持久化成
//! `codemode.journal` 事件，正文外置到 `tool-output/`。kernel 的 fold 不看它：进模型消息的
//! 只有外层 codemode 结果。

use std::collections::BTreeMap;

use komo_kernel::events::{CodemodeJournal, EventPayload};
use komo_kernel::types::ids::{AttemptId, EventId, Seq, ToolCallId};
use komo_kernel::types::plan::{ExecutionPlan, Replay};
use komo_kernel::types::refs::{AttemptRef, OutputRef, ToolResultBody, ToolResultStatus};
use komo_kernel::types::tool::{ToolContext, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ToolExecutor;

/// journal 的一条记录（`codemode.journal` 事件的 `record`）。正文都外置，这里只有引用。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(super) enum Record {
    Start { fingerprint: Value },
    Intent { index: usize, output: OutputRef },
    Started { index: usize, attempt: AttemptRef },
    Delivered { index: usize, output: OutputRef },
    Finished { output: OutputRef },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Intent {
    pub name: String,
    pub args: Value,
    pub plan: Option<ExecutionPlan>,
    pub error: Option<String>,
    pub replay: Replay,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Reply {
    pub value: Option<Value>,
    pub error: Option<String>,
    pub uncertain: bool,
    pub execution: Option<OutputRef>,
}

impl Reply {
    pub fn result(&self) -> Result<Value, String> {
        match (&self.value, &self.error) {
            (Some(value), None) => Ok(value.clone()),
            (None, Some(error)) => Err(error.clone()),
            _ => Err("codemode journal reply 损坏".into()),
        }
    }
}

#[derive(Default, Clone)]
pub(super) struct Child {
    pub intent: Option<OutputRef>,
    pub started: bool,
    pub attempt: Option<AttemptRef>,
    pub delivered: Option<OutputRef>,
}

#[derive(Default)]
pub(super) struct Journal {
    pub fingerprint: Option<Value>,
    pub children: BTreeMap<usize, Child>,
    pub finished: Option<OutputRef>,
    pub cursor: usize,
    pub recovering: bool,
    /// 宿主的致命错误锁住不放：Python 接住 `ToolError` 或自己伪造 done 都解不开。
    pub fatal: Option<ToolError>,
}

pub(super) fn uncertain(message: impl Into<String>) -> ToolError {
    ToolError::Uncertain {
        message: message.into(),
    }
}

impl Journal {
    fn apply(&mut self, record: Record) -> Result<(), ToolError> {
        let invalid = || uncertain("codemode journal 顺序/重复记录损坏，停止执行");
        if self.finished.is_some() {
            return Err(invalid());
        }
        match record {
            Record::Start { fingerprint } => {
                if self.fingerprint.replace(fingerprint).is_some() {
                    return Err(invalid());
                }
            }
            Record::Intent { index, output } => {
                if self.fingerprint.is_none()
                    || index != self.children.len() + 1
                    || self.children.values().any(|c| c.delivered.is_none())
                {
                    return Err(invalid());
                }
                self.children.insert(
                    index,
                    Child {
                        intent: Some(output),
                        ..Child::default()
                    },
                );
            }
            Record::Started { index, attempt } => {
                let child = self.children.get_mut(&index).ok_or_else(invalid)?;
                if child.delivered.is_some() {
                    return Err(invalid());
                }
                child.started = true;
                child.attempt = Some(attempt);
            }
            Record::Delivered { index, output } => {
                let child = self.children.get_mut(&index).ok_or_else(invalid)?;
                if child.delivered.replace(output).is_some() {
                    return Err(invalid());
                }
            }
            Record::Finished { output } => {
                if self.fingerprint.is_none()
                    || self.children.values().any(|c| c.delivered.is_none())
                {
                    return Err(invalid());
                }
                self.finished = Some(output);
            }
        }
        Ok(())
    }

    pub fn next(&mut self, name: &str, args: &Value) -> Result<(usize, Option<Child>), ToolError> {
        if let Some(error) = &self.fatal {
            return Err(error.clone());
        }
        self.cursor += 1;
        if self.cursor > 256 {
            return Err(uncertain("codemode 子调用超过 256 次"));
        }
        let child = self.children.get(&self.cursor).cloned();
        if self.recovering && child.is_none() {
            return Err(uncertain(format!(
                "codemode 回放分歧/未提交窗口：子调用 {} ({name}, {args}) 超出已记录前缀；禁止新增调用",
                self.cursor
            )));
        }
        Ok((self.cursor, child))
    }

    pub fn check_end(&self) -> Result<(), ToolError> {
        if let Some(error) = &self.fatal {
            return Err(error.clone());
        }
        if self.recovering && self.cursor != self.children.len() {
            return Err(uncertain(
                "codemode 回放分歧：脚本提前结束，未消费全部子调用",
            ));
        }
        if self
            .children
            .values()
            .any(|child| child.delivered.is_none())
        {
            return Err(uncertain(
                "codemode 子调用被中断且未提交结果交付；停止脚本，禁止自动续写",
            ));
        }
        Ok(())
    }
}

impl ToolExecutor {
    pub(super) async fn codemode_load(&self, ctx: &ToolContext) -> Result<Journal, ToolError> {
        let mut journal = Journal::default();
        let mut from = Seq(0);
        loop {
            let events = self
                .ledger
                .read(&ctx.session, from, 256)
                .await
                .map_err(|e| uncertain(format!("codemode journal 无法读取：{e}")))?;
            if events.events.is_empty() {
                break;
            }
            for event in &events.events {
                from = event.seq;
                if let EventPayload::CodemodeJournal(body) = &event.payload {
                    if body.run == ctx.run && body.parent == ctx.call {
                        let record = serde_json::from_value(body.record.clone())
                            .map_err(|e| uncertain(format!("codemode journal 损坏：{e}")))?;
                        journal.apply(record)?;
                    }
                }
            }
        }
        let interrupted = ctx
            .resumed
            .as_ref()
            .is_some_and(|state| !state.is_known_not_to_have_run());
        journal.recovering = journal.fingerprint.is_some() || interrupted;
        if interrupted && journal.fingerprint.is_none() {
            return Err(uncertain(
                "codemode 缺少 durable journal（旧版本或开始记录未提交）；不重跑脚本",
            ));
        }
        Ok(journal)
    }

    pub(super) async fn codemode_record(
        &self,
        ctx: &ToolContext,
        key: &str,
        record: Record,
    ) -> Result<(), ToolError> {
        // Coordinator 先 append + sync JSONL，再提交日志索引事务（§8.5）。不建子调用的
        // 调度行、不写 tool.result、不经 control_outbox。
        self.ledger
            .append_audit(
                &ctx.session,
                &EventId::from_raw(format!("codemode:{}:{}:{key}", ctx.run, ctx.call)),
                EventPayload::CodemodeJournal(CodemodeJournal {
                    run: ctx.run.clone(),
                    parent: ctx.call.clone(),
                    record: serde_json::to_value(record).map_err(|e| uncertain(e.to_string()))?,
                }),
                self.clock.now(),
            )
            .await
            .map_err(|e| uncertain(format!("codemode journal 提交失败：{e}；禁止继续执行")))?;
        Ok(())
    }

    pub(super) fn codemode_attempt(ctx: &ToolContext, index: &str) -> AttemptRef {
        // 用哈希：不含路径分隔符，也不会撞上 provider 给的父 Call ID。
        let key = komo_kernel::types::digest::ContentHash::of_str(&format!(
            "codemode:{}:{index}",
            ctx.call
        ));
        AttemptRef {
            session: ctx.session.clone(),
            run: ctx.run.clone(),
            call: ToolCallId::from_raw(format!("cm-{key}")),
            // FileToolOutputStore 按 AttemptId（不是完整路径）索引打开着的流：每个子调用 /
            // 正文的尝试都带上命名空间，连父调用的 attempt 也算进去，发布 journal 正文才不会
            // 拿走父调用正在写的 writer。
            attempt: AttemptId::from_raw(format!(
                "cm-{}",
                komo_kernel::types::digest::ContentHash::of_str(&format!(
                    "codemode:{}:{}:{index}",
                    ctx.call, ctx.attempt
                ))
            )),
        }
    }

    pub(super) async fn codemode_save<T: Serialize>(
        &self,
        ctx: &ToolContext,
        key: &str,
        value: &T,
    ) -> Result<OutputRef, ToolError> {
        let writer = self
            .outputs
            .begin(&Self::codemode_attempt(ctx, key))
            .await
            .map_err(|e| uncertain(e.to_string()))?;
        let result = serde_json::to_value(value).map_err(|e| uncertain(e.to_string()))?;
        let output = self
            .outputs
            .publish(
                writer,
                ToolResultBody {
                    status: ToolResultStatus::Completed,
                    result,
                    error: None,
                    exit_code: None,
                    artifacts: vec![],
                    preview: None,
                },
            )
            .await
            .map_err(|e| uncertain(e.to_string()))?;
        Ok(output.output)
    }

    /// 中断的子调用留下的部分输出写进外层 uncertain 正文：交给模型时仍只经外层那一次投影。
    pub(super) async fn codemode_partial(&self, child: &Child) -> String {
        let Some(attempt) = &child.attempt else {
            return String::new();
        };
        match self.outputs.interrupted(attempt, 4096).await {
            Ok(output) if !output.is_empty() => format!(
                "\n子调用部分输出（tool-output/{}/{}/{}）：\nstdout ({} bytes):\n{}\nstderr ({} bytes):\n{}",
                attempt.run,
                attempt.call,
                attempt.attempt,
                output.stdout_bytes,
                output.stdout_tail,
                output.stderr_bytes,
                output.stderr_tail
            ),
            Ok(_) => String::new(),
            Err(error) => format!("\n子调用部分输出无法读取：{error}"),
        }
    }

    pub(super) async fn codemode_open<T: serde::de::DeserializeOwned>(
        &self,
        output: &OutputRef,
    ) -> Result<T, ToolError> {
        let verified = self
            .outputs
            .open(output)
            .await
            .map_err(|e| uncertain(format!("codemode 结果校验失败：{e}")))?;
        serde_json::from_value(verified.body.result)
            .map_err(|e| uncertain(format!("codemode 结果解析失败：{e}")))
    }
}

#[cfg(test)]
mod tests;
