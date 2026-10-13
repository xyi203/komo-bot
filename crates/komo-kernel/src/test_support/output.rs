//! 内存里的 [`ToolOutputStore`] 与 [`OutputWriter`]。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::traits::*;

use crate::types::refs::{
    AttemptRef, ContentRef, InterruptedOutput, OutputRef, PublishedOutput, ToolResultBody,
    VerifiedOutput,
};

/// 内存里的 [`ToolOutputStore`]。
#[derive(Debug, Clone, Default)]
pub struct MemOutputStore {
    published: Arc<Mutex<BTreeMap<String, ToolResultBody>>>,
    /// 按 attempt ID：上一个进程被打断时留下的部分输出。
    interrupted: Arc<Mutex<BTreeMap<String, InterruptedOutput>>>,
}

impl MemOutputStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 让某次尝试看起来像是被打断过、留下了这些部分输出。
    pub fn leave_interrupted(&self, attempt: &str, output: InterruptedOutput) {
        self.interrupted
            .lock()
            .expect("输出存储")
            .insert(attempt.to_string(), output);
    }

    /// 某个引用路径上已发布的正文。
    pub fn published_body(&self, path: &str) -> Option<ToolResultBody> {
        self.published.lock().expect("输出存储").get(path).cloned()
    }
}

/// 内存里的 [`OutputWriter`]。
#[derive(Debug)]
pub struct MemOutputWriter {
    attempt: AttemptRef,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl MemOutputWriter {
    pub fn new(attempt: AttemptRef) -> Self {
        Self {
            attempt,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }
}

#[async_trait]
impl OutputWriter for MemOutputWriter {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.stdout.extend_from_slice(chunk);
        Ok(())
    }

    async fn write_stderr(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.stderr.extend_from_slice(chunk);
        Ok(())
    }

    fn attempt(&self) -> &AttemptRef {
        &self.attempt
    }

    fn bytes_written(&self) -> u64 {
        (self.stdout.len() + self.stderr.len()) as u64
    }
}

#[async_trait]
impl ToolOutputStore for MemOutputStore {
    async fn begin(&self, attempt: &AttemptRef) -> Result<Box<dyn OutputWriter>, StoreError> {
        Ok(Box::new(MemOutputWriter::new(attempt.clone())))
    }

    async fn publish(
        &self,
        writer: Box<dyn OutputWriter>,
        result: ToolResultBody,
    ) -> Result<PublishedOutput, StoreError> {
        let attempt = writer.attempt().clone();
        let path = format!(
            "tool-output/{}/{}/{}/output.json",
            attempt.run, attempt.call, attempt.attempt
        );
        let body = serde_json::to_string(&result).map_err(|e| StoreError::Io(e.to_string()))?;
        let status = result.status;
        // 与文件存储同一处推导：账本里那份预览是"事实的一部分"，替身不能自己发明一份。
        let preview = result.event_preview();
        self.published
            .lock()
            .expect("输出存储")
            .insert(path.clone(), result.clone());
        Ok(PublishedOutput {
            body: Some(Box::new(result.clone())),
            output: OutputRef(ContentRef {
                path,
                size: body.len() as u64,
                hash: crate::types::digest::ContentHash::of_str(&body),
                pointer: None,
            }),
            status,
            elapsed_ms: 0,
            preview,
            stdout: None,
            stderr: None,
        })
    }

    async fn open(&self, output: &OutputRef) -> Result<VerifiedOutput, StoreError> {
        let body = self
            .published
            .lock()
            .expect("输出存储")
            .get(output.path())
            .cloned()
            .ok_or_else(|| StoreError::Corrupt(format!("{} 不在存储里", output.path())))?;
        let serialized = serde_json::to_string(&body).map_err(|e| StoreError::Io(e.to_string()))?;
        if crate::types::digest::ContentHash::of_str(&serialized) != output.0.hash {
            return Err(StoreError::Corrupt("内容哈希不符".into()));
        }
        Ok(VerifiedOutput {
            reference: output.clone(),
            body,
        })
    }

    async fn interrupted(
        &self,
        attempt: &AttemptRef,
        _tail_bytes: usize,
    ) -> Result<InterruptedOutput, StoreError> {
        Ok(self
            .interrupted
            .lock()
            .expect("输出存储")
            .get(attempt.attempt.as_str())
            .cloned()
            .unwrap_or_default())
    }
}
