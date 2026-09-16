//! `sessions/<id>/tool-output/<run>/<call>/<attempt>/`：一次尝试的完整输出（§8.3）。
//!
//! 所有工具的完整输出都单独保存，不区分大小。stdout / stderr 在运行时流式写入
//! `.partial`，避免在 Gateway 内存里积累完整输出；进程结束并收齐输出后，**同步并完成
//! 文件，再原子写入 `output.json`；最后才能发布 JSONL 结果引用**。
//!
//! `output.json` 绑定 Session / Run / ToolCall / attempt、计划哈希和完成状态——§8.5 的
//! "只有完整校验文件内的调用身份、计划、完成状态与内容后才能补记结果；仅凭路径存在不
//! 足以宣告成功"要的就是这些字段。
//!
//! 因中断只留下的 `.partial` 文件可以用于诊断，**不能当作完成结果**。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, StoreError, ToolOutputStore};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::refs::{
    AttemptRef, ContentRef, OutputRef, PREVIEW_LIMIT_BYTES, PublishedOutput, ToolResultBody,
    ToolResultStatus, VerifiedOutput,
};
use serde::{Deserialize, Serialize};

use crate::payloads::sync_file;
use crate::session_log::{SessionPaths, create_dir_all_synced, sync_dir};

/// `output.json` 的磁盘格式。
///
/// 身份在**文件里**，不只在路径里：补记结果时要校验的是这些字段，路径存在证明不了什么。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputDocument {
    /// 格式版本。
    pub v: u32,
    pub session: String,
    pub run: String,
    pub call: String,
    pub attempt: String,
    /// 完成状态。**`uncertain` 是一个真状态**，不是"失败"的委婉说法（§8.6）。
    pub status: ToolResultStatus,
    pub body: ToolResultBody,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<ContentRef>,
}

/// 当前 `output.json` 的格式版本。
pub const OUTPUT_FORMAT_VERSION: u32 = 1;

/// 一个 Session 的工具输出目录。
///
/// **按 Session 构造**：`OutputRef` 里的路径相对该 Session 目录（§8.3），而
/// [`ToolOutputStore::open`] 的签名只有一个引用，所以"是哪个 Session"必须由存储自己
/// 带着。Gateway 每个 Session 一个实例，和 [`crate::coordinator::Coordinator`] 同寿。
///
/// 打开中的 `.partial` 句柄由**存储**持有，不由写入器持有：`publish` 收到的是
/// `Box<dyn OutputWriter>`，从一个 trait 对象里把具体类型拿回来只能靠 downcast，而
/// `OutputWriter` 不是 `Any`。存储按 attempt ID 记着自己发出去的那几个流，`publish`
/// 按同一个 ID 取回来——写入器于是只需要带着自己的身份。
#[derive(Debug, Clone)]
pub struct FileToolOutputStore {
    paths: SessionPaths,
    session: SessionId,
    streams: Arc<tokio::sync::Mutex<HashMap<String, Streams>>>,
}

/// 一次尝试正在写的两条流。
#[derive(Debug)]
struct Streams {
    stdout: Option<tokio::fs::File>,
    stderr: Option<tokio::fs::File>,
    stdout_bytes: u64,
    stderr_bytes: u64,
    started: std::time::Instant,
}

impl FileToolOutputStore {
    pub fn new(paths: SessionPaths, session: SessionId) -> Self {
        Self {
            paths,
            session,
            streams: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    pub fn paths(&self) -> &SessionPaths {
        &self.paths
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    fn attempt_dir(&self, attempt: &AttemptRef) -> std::path::PathBuf {
        self.paths
            .tool_output()
            .join(attempt.run.as_str())
            .join(attempt.call.as_str())
            .join(attempt.attempt.as_str())
    }

    fn attempt_relative(attempt: &AttemptRef) -> String {
        format!(
            "tool-output/{}/{}/{}",
            attempt.run, attempt.call, attempt.attempt
        )
    }
}

/// 一次尝试的流式写入器。真正的文件句柄在 [`FileToolOutputStore`] 里，按 attempt ID 存。
#[derive(Debug)]
pub struct FileOutputWriter {
    attempt: AttemptRef,
    dir: std::path::PathBuf,
    bytes: u64,
    streams: Arc<tokio::sync::Mutex<HashMap<String, Streams>>>,
}

impl FileOutputWriter {
    async fn stream(&mut self, name: &str, chunk: &[u8]) -> Result<(), StoreError> {
        use tokio::io::AsyncWriteExt;
        let path = self.dir.join(format!("{name}.partial"));
        let mut guard = self.streams.lock().await;
        let entry = guard
            .get_mut(self.attempt.attempt.as_str())
            .ok_or_else(|| {
                StoreError::Other(format!(
                    "尝试 {} 的流已经发布或不属于本存储",
                    self.attempt.attempt
                ))
            })?;
        let (slot, counter) = if name == "stdout.txt" {
            (&mut entry.stdout, &mut entry.stdout_bytes)
        } else {
            (&mut entry.stderr, &mut entry.stderr_bytes)
        };
        if slot.is_none() {
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(|e| StoreError::Io(format!("打开 {} 失败：{e}", path.display())))?;
            *slot = Some(file);
        }
        slot.as_mut()
            .expect("刚刚放进去的")
            .write_all(chunk)
            .await
            .map_err(|e| StoreError::Io(format!("写 {} 失败：{e}", path.display())))?;
        *counter += chunk.len() as u64;
        self.bytes += chunk.len() as u64;
        Ok(())
    }
}

#[async_trait]
impl OutputWriter for FileOutputWriter {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.stream("stdout.txt", chunk).await
    }

    async fn write_stderr(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.stream("stderr.txt", chunk).await
    }

    fn attempt(&self) -> &AttemptRef {
        &self.attempt
    }

    fn bytes_written(&self) -> u64 {
        self.bytes
    }
}

#[async_trait]
impl ToolOutputStore for FileToolOutputStore {
    async fn begin(&self, attempt: &AttemptRef) -> Result<Box<dyn OutputWriter>, StoreError> {
        if attempt.session != self.session {
            return Err(StoreError::Other(format!(
                "这个输出存储是 {} 的，收到的尝试属于 {}",
                self.session, attempt.session
            )));
        }
        let dir = self.attempt_dir(attempt);
        create_dir_all_synced(&dir).await?;
        self.streams.lock().await.insert(
            attempt.attempt.to_string(),
            Streams {
                stdout: None,
                stderr: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
                started: std::time::Instant::now(),
            },
        );
        Ok(Box::new(FileOutputWriter {
            attempt: attempt.clone(),
            dir,
            bytes: 0,
            streams: self.streams.clone(),
        }))
    }

    async fn publish(
        &self,
        writer: Box<dyn OutputWriter>,
        result: ToolResultBody,
    ) -> Result<PublishedOutput, StoreError> {
        let attempt = writer.attempt().clone();
        drop(writer);

        let streams = self
            .streams
            .lock()
            .await
            .remove(attempt.attempt.as_str())
            .ok_or_else(|| {
                StoreError::Other(format!("尝试 {} 没有在本存储上开过流", attempt.attempt))
            })?;
        let elapsed_ms = streams.started.elapsed().as_millis() as u64;
        let dir = self.attempt_dir(&attempt);
        let relative = FileToolOutputStore::attempt_relative(&attempt);

        // 先完成 stdout / stderr：同步、原子改名、同步目录。
        let stdout = finish_stream(
            streams.stdout,
            &dir,
            &relative,
            "stdout.txt",
            streams.stdout_bytes,
        )
        .await?;
        let stderr = finish_stream(
            streams.stderr,
            &dir,
            &relative,
            "stderr.txt",
            streams.stderr_bytes,
        )
        .await?;

        let status = result.status;
        let preview = preview_of(&result);
        let document = OutputDocument {
            v: OUTPUT_FORMAT_VERSION,
            session: attempt.session.to_string(),
            run: attempt.run.to_string(),
            call: attempt.call.to_string(),
            attempt: attempt.attempt.to_string(),
            status,
            body: result,
            elapsed_ms,
            stdout: stdout.clone(),
            stderr: stderr.clone(),
        };
        let bytes = serde_json::to_vec(&document)
            .map_err(|e| StoreError::Other(format!("output.json 序列化失败：{e}")))?;

        // 再原子写 output.json。发布之后才允许追加 JSONL 的 tool.result（§8.5）。
        let final_path = dir.join("output.json");
        let temp_path = dir.join("output.json.partial");
        tokio::fs::write(&temp_path, &bytes)
            .await
            .map_err(|e| StoreError::Io(format!("写 {} 失败：{e}", temp_path.display())))?;
        sync_file(&temp_path).await?;
        tokio::fs::rename(&temp_path, &final_path)
            .await
            .map_err(|e| StoreError::Io(format!("发布 {} 失败：{e}", final_path.display())))?;
        sync_dir(&dir).await?;

        Ok(PublishedOutput {
            output: OutputRef(ContentRef {
                path: format!("{relative}/output.json"),
                size: bytes.len() as u64,
                hash: ContentHash::of_bytes(&bytes),
                pointer: None,
            }),
            status,
            elapsed_ms,
            preview,
            stdout,
            stderr,
        })
    }

    async fn open(&self, output: &OutputRef) -> Result<VerifiedOutput, StoreError> {
        let path = self.paths.resolve(&output.0.path)?;
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::Corrupt(format!("引用的输出不在：{}", output.0.path))
            } else {
                StoreError::Io(format!("读 {} 失败：{e}", path.display()))
            }
        })?;
        if bytes.len() as u64 != output.0.size {
            return Err(StoreError::Corrupt(format!(
                "{} 的大小是 {}，引用说 {}",
                output.0.path,
                bytes.len(),
                output.0.size
            )));
        }
        // 不匹配返回损坏，**不返回内容**（§8.3）。
        if ContentHash::of_bytes(&bytes) != output.0.hash {
            return Err(StoreError::Corrupt(format!(
                "{} 的内容哈希与引用不符",
                output.0.path
            )));
        }
        let document: OutputDocument = serde_json::from_slice(&bytes)
            .map_err(|e| StoreError::Corrupt(format!("{} 不是 output.json：{e}", output.0.path)))?;
        if document.session != self.session.as_str() {
            return Err(StoreError::Corrupt(format!(
                "{} 里的 session 是 {}，不是 {}",
                output.0.path, document.session, self.session
            )));
        }
        Ok(VerifiedOutput {
            reference: output.clone(),
            body: document.body,
        })
    }
}

async fn finish_stream(
    file: Option<tokio::fs::File>,
    dir: &std::path::Path,
    relative: &str,
    name: &str,
    bytes: u64,
) -> Result<Option<ContentRef>, StoreError> {
    use tokio::io::AsyncWriteExt;
    let Some(mut file) = file else {
        return Ok(None);
    };
    file.flush()
        .await
        .map_err(|e| StoreError::Io(format!("清空 {name} 缓冲失败：{e}")))?;
    file.sync_all()
        .await
        .map_err(|e| StoreError::Io(format!("同步 {name} 失败：{e}")))?;
    drop(file);

    let temp = dir.join(format!("{name}.partial"));
    let final_path = dir.join(name);
    tokio::fs::rename(&temp, &final_path)
        .await
        .map_err(|e| StoreError::Io(format!("发布 {} 失败：{e}", final_path.display())))?;
    sync_dir(dir).await?;

    let content = tokio::fs::read(&final_path)
        .await
        .map_err(|e| StoreError::Io(format!("读 {} 失败：{e}", final_path.display())))?;
    Ok(Some(ContentRef {
        path: format!("{relative}/{name}"),
        size: bytes,
        hash: ContentHash::of_bytes(&content),
        pointer: None,
    }))
}

/// JSONL 里只留最多 1 KiB 的预览（§8.3）。
fn preview_of(result: &ToolResultBody) -> Option<String> {
    let text = match (&result.error, &result.result) {
        (Some(error), _) => error.clone(),
        (None, serde_json::Value::Null) => return None,
        (None, serde_json::Value::String(text)) => text.clone(),
        (None, value) => value.to_string(),
    };
    if text.is_empty() {
        return None;
    }
    Some(truncate_chars(&text, PREVIEW_LIMIT_BYTES))
}

/// 按**字符边界**截断到最多 `limit` 字节——从中间切开一个 UTF-8 序列会产生一个读不回
/// 来的预览。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::{AttemptId, RunId, ToolCallId};

    fn attempt(session: &SessionId) -> AttemptRef {
        AttemptRef {
            session: session.clone(),
            run: RunId::from_raw("run-1"),
            call: ToolCallId::from_raw("call-7"),
            attempt: AttemptId::from_raw("attempt-1"),
        }
    }

    fn body(status: ToolResultStatus) -> ToolResultBody {
        ToolResultBody {
            status,
            result: serde_json::json!({"ok": true}),
            error: None,
            exit_code: Some(0),
            artifacts: vec![],
        }
    }

    async fn store() -> (tempfile::TempDir, FileToolOutputStore, SessionId) {
        let dir = tempfile::tempdir().expect("临时目录");
        let session = SessionId::from_raw("sess-1");
        let paths = SessionPaths::new(dir.path(), &session);
        (
            dir,
            FileToolOutputStore::new(paths, session.clone()),
            session,
        )
    }

    #[tokio::test]
    async fn streams_land_in_partial_files_and_are_published_atomically() {
        let (_dir, store, session) = store().await;
        let attempt = attempt(&session);

        let mut writer = store.begin(&attempt).await.unwrap();
        writer.write_stdout(b"hello ").await.unwrap();
        writer.write_stdout(b"world").await.unwrap();
        writer.write_stderr(b"warn").await.unwrap();
        assert_eq!(writer.bytes_written(), 15);

        let dir = store
            .paths()
            .resolve("tool-output/run-1/call-7/attempt-1")
            .unwrap();
        // 运行中只有 .partial：中断只留下它的话可以用于诊断，**不能当作完成结果**。
        assert!(dir.join("stdout.txt.partial").exists());
        assert!(!dir.join("stdout.txt").exists());
        assert!(!dir.join("output.json").exists());

        let published = store
            .publish(writer, body(ToolResultStatus::Completed))
            .await
            .unwrap();

        assert!(dir.join("stdout.txt").exists());
        assert!(!dir.join("stdout.txt.partial").exists());
        assert!(dir.join("output.json").exists());
        assert!(!dir.join("output.json.partial").exists());

        assert_eq!(
            published.output.path(),
            "tool-output/run-1/call-7/attempt-1/output.json"
        );
        assert_eq!(published.stdout.as_ref().unwrap().size, 11);
        assert_eq!(published.stderr.as_ref().unwrap().size, 4);
        assert_eq!(published.status, ToolResultStatus::Completed);

        let verified = store.open(&published.output).await.unwrap();
        assert_eq!(verified.body, body(ToolResultStatus::Completed));
    }

    /// 没有 stdout / stderr 的工具不该留下空文件。
    #[tokio::test]
    async fn a_tool_that_printed_nothing_leaves_no_stream_files() {
        let (_dir, store, session) = store().await;
        let writer = store.begin(&attempt(&session)).await.unwrap();
        let published = store
            .publish(writer, body(ToolResultStatus::Completed))
            .await
            .unwrap();
        assert!(published.stdout.is_none());
        assert!(published.stderr.is_none());
    }

    /// 验收 ⑨：`open` 校验哈希；不符返回 [`StoreError::Corrupt`]，**不返回内容**。
    #[tokio::test]
    async fn open_refuses_to_return_content_whose_hash_does_not_match() {
        let (_dir, store, session) = store().await;
        let writer = store.begin(&attempt(&session)).await.unwrap();
        let published = store
            .publish(writer, body(ToolResultStatus::Completed))
            .await
            .unwrap();

        // 有人在发布之后改了 output.json。
        let path = store.paths().resolve(published.output.path()).unwrap();
        let mut bytes = tokio::fs::read(&path).await.unwrap();
        let position = bytes.len() - 2;
        bytes[position] ^= 0x20;
        tokio::fs::write(&path, &bytes).await.unwrap();

        let error = store.open(&published.output).await.unwrap_err();
        assert!(
            matches!(&error, StoreError::Corrupt(message) if message.contains("内容哈希")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn open_reports_a_missing_file_as_corruption_not_as_an_empty_result() {
        let (_dir, store, session) = store().await;
        let writer = store.begin(&attempt(&session)).await.unwrap();
        let published = store
            .publish(writer, body(ToolResultStatus::Completed))
            .await
            .unwrap();
        let path = store.paths().resolve(published.output.path()).unwrap();
        tokio::fs::remove_file(&path).await.unwrap();

        let error = store.open(&published.output).await.unwrap_err();
        assert!(matches!(error, StoreError::Corrupt(_)), "{error}");
    }

    /// `uncertain` 一路保留到 `output.json` 里——它是一个真状态，不是"失败"的委婉说法。
    #[tokio::test]
    async fn an_uncertain_result_stays_uncertain_on_disk() {
        let (_dir, store, session) = store().await;
        let writer = store.begin(&attempt(&session)).await.unwrap();
        let mut uncertain = body(ToolResultStatus::Uncertain);
        uncertain.error = Some("进程被杀，副作用未知".into());
        let published = store.publish(writer, uncertain.clone()).await.unwrap();

        assert_eq!(published.status, ToolResultStatus::Uncertain);
        assert_eq!(published.preview.as_deref(), Some("进程被杀，副作用未知"));
        let verified = store.open(&published.output).await.unwrap();
        assert_eq!(verified.body.status, ToolResultStatus::Uncertain);
    }

    /// `output.json` 里带着身份：仅凭路径存在不足以宣告成功（§8.5）。
    #[tokio::test]
    async fn the_output_document_binds_the_identity_of_the_call() {
        let (_dir, store, session) = store().await;
        let writer = store.begin(&attempt(&session)).await.unwrap();
        let published = store
            .publish(writer, body(ToolResultStatus::Completed))
            .await
            .unwrap();
        let path = store.paths().resolve(published.output.path()).unwrap();
        let document: OutputDocument =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        assert_eq!(document.session, "sess-1");
        assert_eq!(document.run, "run-1");
        assert_eq!(document.call, "call-7");
        assert_eq!(document.attempt, "attempt-1");
        assert_eq!(document.v, OUTPUT_FORMAT_VERSION);
    }

    /// 别的 Session 的尝试递到这个存储上是编程错误，要说出来。
    #[tokio::test]
    async fn a_store_refuses_an_attempt_from_another_session() {
        let (_dir, store, _session) = store().await;
        let other = attempt(&SessionId::from_raw("sess-2"));
        assert!(store.begin(&other).await.is_err());
    }

    #[test]
    fn a_preview_is_cut_on_a_character_boundary() {
        let text = "汉".repeat(500);
        let cut = truncate_chars(&text, PREVIEW_LIMIT_BYTES);
        assert!(cut.len() <= PREVIEW_LIMIT_BYTES);
        assert!(text.starts_with(&cut));
        // 切得回来才算是预览。
        assert!(cut.chars().count() > 0);
    }
}
