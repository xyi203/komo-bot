//! 正文与输出的引用（§8.3）。
//!
//! 路径一律**相对于该 Session 目录**解析，不依赖进程工作目录；解析时禁止越出该
//! 目录。引用带大小与内容哈希，读取时校验，不匹配就返回损坏而不是内容。

use serde::{Deserialize, Serialize};

use super::digest::ContentHash;
use super::ids::{AttemptId, RunId, SessionId, ToolCallId};

/// JSONL 里 `tool.result` 允许内联的预览上限（§8.3）。
pub const PREVIEW_LIMIT_BYTES: usize = 1024;

/// 单次参数超过这个大小时，把包含它的模型消息正文外置到 `payloads/`（§8.3）。
pub const INLINE_ARGUMENT_LIMIT_BYTES: usize = 4096;

/// 一份 Session 目录内的受控引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRef {
    /// 相对于该 Session 目录的路径。
    pub path: String,
    pub size: u64,
    pub hash: ContentHash,
    /// 引用文件内部的字段定位（例如 `payloads` 里某条消息的某个参数）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

/// 外置的模型消息 / 执行计划正文（`payloads/`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PayloadRef(pub ContentRef);

/// 一次尝试的完整输出目录（`tool-output/{run}/{call}/{attempt}/`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputRef(pub ContentRef);

/// 一次尝试的身份，`ToolOutputStore::begin` 用它决定写到哪个目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRef {
    pub session: SessionId,
    pub run: RunId,
    pub call: ToolCallId,
    pub attempt: AttemptId,
}

/// `output.json` 的正文：结构化结果或错误正文（§8.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultBody {
    pub status: ToolResultStatus,
    /// 脚本设置的 `result`，或工具的结构化返回。
    #[serde(default)]
    pub result: serde_json::Value,
    /// 失败或不确定时的具体原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// 产物引用（`artifacts/` 下），不重复复制到 tool-output。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ContentRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Completed,
    Failed,
    /// 副作用是否发生未知——不能在这里假装失败（§8.6）。
    Uncertain,
}

/// `ToolOutputStore::publish` 的返回：已经同步并原子发布的输出。只有拿到它才能
/// 追加 `tool.result`（§8.5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedOutput {
    pub output: OutputRef,
    pub status: ToolResultStatus,
    /// 实际执行耗时，毫秒。
    pub elapsed_ms: u64,
    /// 最多 [`PREVIEW_LIMIT_BYTES`] 的可选预览。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<ContentRef>,
}

/// `ToolOutputStore::open` 的返回：哈希已核对过的输出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedOutput {
    pub reference: OutputRef,
    pub body: ToolResultBody,
}

impl PayloadRef {
    pub fn path(&self) -> &str {
        &self.0.path
    }
}

impl OutputRef {
    pub fn path(&self) -> &str {
        &self.0.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_round_trips_through_json() {
        let reference = OutputRef(ContentRef {
            path: "tool-output/run-1/call-7/attempt-1/output.json".into(),
            size: 42,
            hash: ContentHash::of_str("x"),
            pointer: None,
        });
        let text = serde_json::to_string(&reference).unwrap();
        assert!(!text.contains("pointer"), "缺省字段不占地方：{text}");
        let back: OutputRef = serde_json::from_str(&text).unwrap();
        assert_eq!(back, reference);
    }

    #[test]
    fn a_result_body_written_before_artifacts_existed_reads_with_an_empty_list() {
        let old = r#"{"status":"completed","result":{"ok":true}}"#;
        let body: ToolResultBody = serde_json::from_str(old).unwrap();
        assert_eq!(body.status, ToolResultStatus::Completed);
        assert!(body.artifacts.is_empty());
        assert!(body.error.is_none());
    }
}
