//! codemode 嵌套调用的数据与"嵌套这一侧"的收尾（`docs/storage-durable.md` §4.7；pi-durable
//! spec §7.3 的 `NestedCallDoc`）。
//!
//! 脚本里的每个子调用是一个 `komo.tool` 任务，输入 `kind: "nested"`，**由外层 codemode 任务拥有**
//! （`Ownership::Task`）；它走和顶层调用同一条 `call → execute → 结果` 路径（意图 checkpoint、能力面、
//! `Tool::prepare`、§5 的恢复表），只是结果不进 transcript：写进外层任务名下的 `komo.nested`
//! document 里它自己的那一员（key 是调用在脚本里的序号 `c1`、`c2`……），外层读到后交给脚本。
//!
//! ```json
//! { "taskId": 12, "name": "read", "arguments": {…},
//!   "result": { "value": { "status": "completed", "text": "…", "result": {…} },
//!               "uncertain": false, "images": [], "output": "artifact://…/result" } }
//! ```
//!
//! 一次创建提交里同时有嵌套任务与它的 document 成员（`result` 还空着）；嵌套任务终结的那笔提交把
//! `result` 写进去。外层终结时（任务名下的 document 随之退役）这些结果一起消失——外层结果里的
//! `result.calls` 才是留给模型的摘要。

use komo_kernel::durable::{DocumentAddress, DocumentScope, TaskId};
use komo_kernel::projection::{ProjectionContext, project};
use komo_kernel::types::refs::{AttemptRef, ImageRef, PublishedOutput, ToolResultStatus};
use komo_kernel::types::resource::{OutputFile, ResourceUri};
use komo_kernel::types::tool::{ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::live::{self, SlotStatus};
use super::outcome::{aborted, completed, failed};
use super::tool::{Ending, ToolTask};
use crate::durable::error::{DurableError, TaskError};
use crate::durable::runtime::TaskRuntime;

pub const NESTED_DOC: &str = "komo.nested";
pub const NESTED_VERSION: u32 = 1;

/// 外层任务名下 `key` 这个调用的 document 地址。
pub fn nested_address(parent: TaskId, key: &str) -> DocumentAddress {
    DocumentAddress {
        kind: NESTED_DOC.to_owned(),
        scope: DocumentScope::Task { task: parent },
        key: Some(key.to_owned()),
    }
}

/// 第 `index` 个子调用（从 1 数）在外层任务里的 key。
pub fn nested_key(index: usize) -> String {
    format!("c{index}")
}

/// 嵌套任务输入里指回外层的那一部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedMeta {
    pub parent: TaskId,
    pub key: String,
}

/// `komo.nested` 的一个成员：一个子调用的槽位与它交付的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedDoc {
    pub task_id: TaskId,
    pub name: String,
    pub arguments: Value,
    /// 任务终结的那笔提交写进来；之前是空的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<NestedResult>,
}

/// 一个子调用交给脚本的结果（`docs/codemode.md` §5）：成功时是 `{status, text, result}`，普通
/// 错误是一句话（脚本里的 `ToolError`），`uncertain` 是宿主终止条件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 结果不明（unsafe 调用被打断、取消、超时）：外层据此锁住后面所有调用。
    #[serde(default)]
    pub uncertain: bool,
    /// 这个调用交出来的图：脚本只拿得到文字，图随外层结果交给模型。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
    /// 完整原始输出的入口（`artifact://…/result`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

impl NestedResult {
    /// 脚本收到的东西：`Ok` 是 `{status, text, result}`，`Err` 是 `ToolError` 的正文。
    pub fn script_result(&self) -> Result<Value, String> {
        match (&self.value, &self.error) {
            (Some(value), None) => Ok(value.clone()),
            (None, Some(error)) => Err(error.clone()),
            _ => Err("codemode 子调用的结果记录损坏".into()),
        }
    }

    /// 工具自己报的状态（摘要用）。
    pub fn status(&self) -> Option<ToolResultStatus> {
        self.value
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(|status| serde_json::from_value(status.clone()).ok())
    }

    /// 刚执行完：照工具交回的东西。文字取 `result.text`，没有就是流进 stdout 的全部字节，再没有
    /// 才是预览——脚本拿到的是完整正文，不是模型视图。
    pub fn from_outcome(
        outcome: &Result<ToolOutput, ToolError>,
        captured: &[u8],
        published: &PublishedOutput,
        attempt: &AttemptRef,
    ) -> Self {
        let images = published_images(published);
        let output = Some(output_uri(attempt));
        match outcome {
            Ok(result) => {
                let text = match result.result.get("text").and_then(Value::as_str) {
                    Some(text) => text.to_owned(),
                    None if !captured.is_empty() => String::from_utf8_lossy(captured).into_owned(),
                    None => result.preview.clone().unwrap_or_default(),
                };
                Self {
                    value: Some(json!({
                        "status": result.status,
                        "text": text,
                        "result": result.result,
                    })),
                    error: None,
                    uncertain: result.status == ToolResultStatus::Uncertain,
                    images,
                    output,
                }
            }
            Err(error) => Self {
                value: None,
                error: Some(error.to_string()),
                uncertain: error.is_uncertain()
                    || matches!(error, ToolError::Cancelled | ToolError::Timeout { .. }),
                images,
                output,
            },
        }
    }

    /// 只有发布出来的输出（意图之前就结案的调用、恢复时复用盘上的完整输出、中断、取消）：
    /// 失败结果的 `error` 就是那句话；`aborted` 一律结果不明——取消时工具可能已经做了一半。
    pub fn from_published(
        published: &PublishedOutput,
        attempt: &AttemptRef,
        aborted: bool,
    ) -> Self {
        let images = published_images(published);
        let output = Some(output_uri(attempt));
        let body = published.body.as_deref();
        let preview = body.and_then(|body| body.preview.clone());
        if aborted || body.is_some_and(|body| body.error.is_some()) {
            // 预览是写给读的人的那句话（取消时还带着中断前的部分输出），先于裸错误。
            let message = preview
                .or_else(|| body.and_then(|body| body.error.clone()))
                .unwrap_or_else(|| "子调用没有结果".into());
            return Self {
                value: None,
                error: Some(message),
                uncertain: aborted || published.status == ToolResultStatus::Uncertain,
                images,
                output,
            };
        }
        let text = body
            .and_then(|body| body.result.get("text").and_then(Value::as_str))
            .map(str::to_owned)
            .or(preview)
            .unwrap_or_default();
        Self {
            value: Some(json!({
                "status": published.status,
                "text": text,
                "result": body.map(|body| body.result.clone()).unwrap_or(Value::Null),
            })),
            error: None,
            uncertain: published.status == ToolResultStatus::Uncertain,
            images,
            output,
        }
    }

    /// bash 只交付 kernel 的有界 full 视图，事实来自已发布的 output.json 与流引用。
    /// 不缓存完整 stdout；恢复时用同一份落盘事实重建，已交付后则直接回放 document 里的字节。
    pub fn from_bash(
        published: &PublishedOutput,
        attempt: &AttemptRef,
        projection: &ProjectionContext,
        aborted: bool,
    ) -> Result<Self, TaskError> {
        let mut result = Self::from_published(published, attempt, aborted);
        if let Some(value) = result.value.as_mut() {
            value["text"] = Value::String(project(
                &super::tool::result_facts("bash", published)?,
                projection,
            ));
        }
        Ok(result)
    }

    /// 任务被调度器写了 `faulted` / `orphaned`，没来得及写结果：做没做过不知道。
    pub fn from_scheduler(message: &str) -> Self {
        Self {
            value: None,
            error: Some(format!("子调用的任务没有正常收场（{message}），结果不明")),
            uncertain: true,
            images: vec![],
            output: None,
        }
    }
}

fn published_images(published: &PublishedOutput) -> Vec<ImageRef> {
    published
        .body
        .as_deref()
        .map(|body| body.images.clone())
        .unwrap_or_default()
}

fn output_uri(attempt: &AttemptRef) -> String {
    ResourceUri::Output {
        run: attempt.run.clone(),
        call: attempt.call.clone(),
        attempt: attempt.attempt.clone(),
        file: OutputFile::Result,
    }
    .to_string()
}

/// 盘上已发布的输出能不能原样当成"脚本收到的结果"复用：脚本要的文字是 `result.text`，没有就是
/// 流进 stdout 的全部字节；而发布出来的只有预览。stdout 与预览是同一段文字（哈希一样，或者
/// 根本没有 stdout）时才准确——`rg` 那类预览比 stdout 短的没法复原，交给 §5 的表（Safe 就重跑）。
/// bash 的脚本正文是 kernel 的有限视图，完整输出通过验证后可直接重建，不要求预览等于 stdout。
pub fn reusable(published: &PublishedOutput, tool: &str) -> bool {
    let Some(body) = published.body.as_deref() else {
        return false;
    };
    if tool == "bash"
        || body.error.is_some()
        || body.result.get("text").is_some_and(Value::is_string)
    {
        return true;
    }
    match &published.stdout {
        None => true,
        Some(stdout) => {
            stdout.size == 0
                || body.preview.as_deref().is_some_and(|preview| {
                    stdout.hash == komo_kernel::types::digest::ContentHash::of_str(preview)
                })
        }
    }
}

impl ToolTask {
    /// 嵌套任务的收尾：把结果写进外层任务名下的 document 成员，同一次提交里任务终结。
    pub(super) async fn commit_nested(
        &self,
        rt: &TaskRuntime,
        nested: &NestedMeta,
        result: NestedResult,
        ending: Ending,
    ) -> Result<(), TaskError> {
        let address = nested_address(nested.parent, &nested.key);
        let receipt = json!({ "kind": "nested", "key": nested.key });
        let conversation = rt.conversation();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let Some((version, mut doc)) = tx.document(&address).await? else {
                    return Err(DurableError::rejected(format!(
                        "the nested call document {:?} does not exist",
                        address.key
                    )));
                };
                doc["result"] = serde_json::to_value(&result)
                    .map_err(|e| DurableError::rejected(format!("nested result: {e}")))?;
                tx.put_document(address, version, doc).await?;
                let mut live = live::load(tx, conversation).await?;
                if let Some(slot) = live.slot_mut(task.id()) {
                    slot.finish();
                    if result.error.is_some()
                        || result
                            .status()
                            .is_some_and(|status| status != ToolResultStatus::Completed)
                    {
                        slot.status = SlotStatus::Failed;
                    }
                }
                live::store(tx, conversation, &live).await?;
                Ok(Some(match ending {
                    Ending::Completed => completed(receipt),
                    Ending::Failed(message) => failed(message, "interrupted", Some(receipt)),
                    Ending::Aborted => aborted(Some(receipt)),
                }))
            })
        })
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use komo_kernel::types::digest::ContentHash;
    use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
    use komo_kernel::types::refs::{ContentRef, OutputRef, ToolResultBody};

    use super::*;

    fn content(text: &str) -> ContentRef {
        ContentRef {
            path: "tool-output/1/9/1/stdout.txt".into(),
            size: text.len() as u64,
            hash: ContentHash::of_str(text),
            pointer: None,
        }
    }

    fn published(body: ToolResultBody, stdout: Option<ContentRef>) -> PublishedOutput {
        PublishedOutput {
            status: body.status,
            body: Some(Box::new(body)),
            output: OutputRef(ContentRef {
                path: "tool-output/1/9/1/output.json".into(),
                ..content("output")
            }),
            elapsed_ms: 3,
            preview: None,
            stdout,
            stderr: None,
        }
    }

    fn body(preview: &str, result: Value) -> ToolResultBody {
        ToolResultBody {
            status: ToolResultStatus::Completed,
            result,
            error: None,
            exit_code: None,
            artifacts: vec![],
            images: vec![],
            preview: Some(preview.into()),
        }
    }

    fn attempt() -> AttemptRef {
        AttemptRef {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("1"),
            call: ToolCallId::from_raw("9"),
            attempt: AttemptId::from_raw("1"),
        }
    }

    #[test]
    fn a_published_output_is_reusable_when_its_preview_is_the_whole_stdout() {
        let same = published(body("hello", Value::Null), Some(content("hello")));
        assert!(reusable(&same, "rg"));
        // 预览比 stdout 短（`rg` 一类）：脚本要的完整文字复原不了。
        let cut = published(body("hel", Value::Null), Some(content("hello")));
        assert!(!reusable(&cut, "rg"));
        // 结果自带 `text`、或根本没有 stdout：准确。
        let text = published(
            body("hel", json!({ "text": "hello" })),
            Some(content("hello")),
        );
        assert!(reusable(&text, "read"));
        assert!(reusable(&published(body("x", Value::Null), None), "read"));
    }

    #[test]
    fn what_the_script_receives_comes_from_the_published_body_when_nothing_else_is_left() {
        let ok = NestedResult::from_published(
            &published(body("file text", json!({ "n": 1 })), None),
            &attempt(),
            false,
        );
        assert_eq!(
            ok.script_result().unwrap(),
            json!({ "status": "completed", "text": "file text", "result": { "n": 1 } })
        );
        assert!(!ok.uncertain);
        assert_eq!(ok.status(), Some(ToolResultStatus::Completed));
        assert_eq!(ok.output.as_deref(), Some("artifact://1/9/1/result"));

        // 取消：不管正文是什么，结果不明。
        let cancelled = NestedResult::from_published(
            &published(body("工具 read 被取消", Value::Null), None),
            &attempt(),
            true,
        );
        assert!(cancelled.uncertain);
        assert_eq!(cancelled.script_result(), Err("工具 read 被取消".into()));
    }

    #[test]
    fn bash_rebuilds_a_bounded_preview_and_nonzero_exit_remains_a_value() {
        let full = format!("FIRST\n{}LAST\n", "line with content\n".repeat(3000));
        let mut failed = body(&full, json!({"exit_code": 7}));
        failed.status = ToolResultStatus::Failed;
        failed.exit_code = Some(7);
        let output = published(failed, Some(content(&full)));
        let projection = ProjectionContext {
            model_result_bytes: 1024,
            decay: None,
        };
        let result = NestedResult::from_bash(&output, &attempt(), &projection, false).unwrap();
        let value = result.script_result().unwrap();
        let text = value["text"].as_str().unwrap();
        assert!(text.contains("FIRST") && !text.contains("LAST"), "{text}");
        assert!(text.contains("超过单条结果上限"), "{text}");
        assert!(text.contains("artifact://1/9/1/result"));
        assert!(text.len() < 2048);
        assert_eq!(value["status"], "failed");
        assert_eq!(value["result"]["exit_code"], 7);
        assert!(!result.uncertain);
        assert_eq!(
            result,
            NestedResult::from_bash(&output, &attempt(), &projection, false).unwrap()
        );
        // stdout 与 preview 不相等也能复用：bash 给脚本的是可重建的有限视图。
        let truncated = published(body("LAST", Value::Null), Some(content(&full)));
        assert!(reusable(&truncated, "bash"));
        assert!(!reusable(&truncated, "rg"));
    }
}
