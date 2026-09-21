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
    /// 工具写给模型看的那段正文（§8.3）。
    ///
    /// **JSONL 里的 `tool.result` 只留它的前 1 KiB**（行要小），完整这一份在 `output.json`
    /// 里。模型上下文的投影读的是这一份，不是那个 1 KiB 的副本——否则"给模型多少"就被
    /// 账本的存储预算绑死了，而那是两件事。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
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

impl ToolResultBody {
    /// 事件里那份 ≤1 KiB 的预览（§8.3）。
    ///
    /// 工具自己写的那段正文（[`Self::preview`]）优先——它知道该给模型看什么；没有那一段时
    /// 才从结构化结果里拼，恢复与核对那几条路径走的就是这里。
    ///
    /// **一处实现，两处读**：文件存储与内存替身都用它。各写一份的话，"刚跑完"与测试里看到
    /// 的账本就会不一样，而那种漂只有真机上才看得出来。
    pub fn event_preview(&self) -> Option<String> {
        let text = match (&self.preview, &self.error, &self.result) {
            (Some(text), _, _) => text.clone(),
            (None, Some(error), _) => error.clone(),
            (None, None, serde_json::Value::Null) => return None,
            (None, None, serde_json::Value::String(text)) => text.clone(),
            (None, None, value) => value.to_string(),
        };
        if text.is_empty() {
            return None;
        }
        Some(truncate_chars(&text, PREVIEW_LIMIT_BYTES))
    }
}

/// 按**字符边界**截断到最多 `limit` 字节——从中间切开一个 UTF-8 序列会产生一个读不回来的
/// 预览。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut cut = limit;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_string()
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

    /// 老 `output.json` 里没有 `preview` 这一格（2026-09-21 之前写的），读出来是 `None`
    /// ——**加字段只能是加法**，旧文件照样解得开（§8.2）。
    #[test]
    fn a_result_body_written_before_previews_existed_reads_with_none() {
        let old = r#"{"status":"completed","result":{"ok":true}}"#;
        let body: ToolResultBody = serde_json::from_str(old).unwrap();
        assert_eq!(body.status, ToolResultStatus::Completed);
        assert!(body.artifacts.is_empty());
        assert!(body.error.is_none());
        assert!(body.preview.is_none());
    }

    #[test]
    fn the_event_preview_prefers_what_the_tool_wrote_and_falls_back_to_the_result() {
        let body = |preview: Option<&str>, error: Option<&str>, result: serde_json::Value| {
            ToolResultBody {
                status: ToolResultStatus::Completed,
                result,
                error: error.map(str::to_string),
                exit_code: None,
                artifacts: vec![],
                preview: preview.map(str::to_string),
            }
        };

        // 工具自己写的那段优先——它知道该给模型看什么。
        assert_eq!(
            body(Some("工具写的"), None, serde_json::json!("结果里的"))
                .event_preview()
                .as_deref(),
            Some("工具写的")
        );
        // 没有那一段时：错误 → 字符串结果 → JSON。
        assert_eq!(
            body(None, Some("失败了"), serde_json::Value::Null)
                .event_preview()
                .as_deref(),
            Some("失败了")
        );
        assert_eq!(
            body(None, None, serde_json::json!("一段正文"))
                .event_preview()
                .as_deref(),
            Some("一段正文")
        );
        assert_eq!(
            body(None, None, serde_json::json!({ "ok": true }))
                .event_preview()
                .as_deref(),
            Some("{\"ok\":true}")
        );
        // 什么都没有就是没有：不该凭空造一句。
        assert_eq!(
            body(None, None, serde_json::Value::Null).event_preview(),
            None
        );
    }

    #[test]
    fn a_preview_is_cut_on_a_character_boundary() {
        let body = ToolResultBody {
            status: ToolResultStatus::Completed,
            result: serde_json::Value::Null,
            error: None,
            exit_code: None,
            artifacts: vec![],
            preview: Some("汉".repeat(500)),
        };
        let cut = body.event_preview().expect("有预览");
        assert!(cut.len() <= PREVIEW_LIMIT_BYTES);
        assert!(cut.chars().count() > 0, "切得回来才算是预览");
        assert!("汉".repeat(500).starts_with(&cut));
    }
}
