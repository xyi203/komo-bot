//! 五个基础工具（§4）与它们共用的那点东西。
//!
//! 搜索走 shell 或 Python，HTTP 走 Python 库或命令；Git、构建、测试、HA、网页搜索和
//! 记录查询都是这五个的组合，**不新增第六个**。

pub mod edit;
pub mod paths;
pub mod process;
pub mod python;
pub mod read;
pub mod shell;
pub mod write;

use std::path::Path;
use std::time::SystemTime;

use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::tool::ToolError;
use serde::{Deserialize, Serialize};

pub use edit::EditTool;
pub use python::PythonTool;
pub use read::ReadTool;
pub use shell::ShellTool;
pub use write::WriteTool;

/// 一个文件的版本（§4：`read` 返回它，`write` / `edit` 覆盖时核对它）。
///
/// 内容哈希是权威，mtime 只是给人看的旁证：两台机器的时钟、复制文件的工具、编辑器
/// 的保存方式都能改 mtime 而不改内容，反过来也有一秒内改两次的情形。核对**只比
/// 哈希**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVersion {
    pub hash: ContentHash,
    /// Unix 毫秒。读不到就是 `None`——不编一个。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<i64>,
    pub size: u64,
}

impl FileVersion {
    pub fn of(bytes: &[u8], metadata: Option<&std::fs::Metadata>) -> Self {
        Self {
            hash: ContentHash::of_bytes(bytes),
            modified_ms: metadata.and_then(modified_ms),
            size: bytes.len() as u64,
        }
    }
}

fn modified_ms(metadata: &std::fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    match modified.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .map(|ms| -ms),
    }
}

/// 参数里给出的"预期版本"。
///
/// 两种写法都收：`read` 原样返回的那个对象，和只有哈希的那个串。让模型把读到的东西
/// 原样传回来是最不容易出错的那条路，而手写一个哈希串也应该能用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExpectedVersion {
    Hash(String),
    Full(FileVersion),
}

impl ExpectedVersion {
    pub fn hash(&self) -> ContentHash {
        match self {
            ExpectedVersion::Hash(raw) => ContentHash::from_raw(raw.clone()),
            ExpectedVersion::Full(version) => version.hash.clone(),
        }
    }
}

/// 当前磁盘上这个文件的正文与版本。文件不存在 → `Ok(None)`。
pub fn current(path: &Path) -> Result<Option<(Vec<u8>, FileVersion)>, ToolError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let metadata = std::fs::metadata(path).ok();
            let version = FileVersion::of(&bytes, metadata.as_ref());
            Ok(Some((bytes, version)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ToolError::Failed {
            message: format!("读取 {} 失败：{error}", path.display()),
        }),
    }
}

/// 正文必须是 UTF-8——五个基础工具处理的是文本；二进制走 shell 或 Python。
pub fn as_text(bytes: Vec<u8>, path: &Path) -> Result<String, ToolError> {
    String::from_utf8(bytes).map_err(|_| ToolError::InvalidArguments {
        message: format!(
            "{} 不是 UTF-8 文本；二进制内容请用 shell 或 python 处理",
            path.display()
        ),
    })
}

/// 解析工具参数，失败就是 [`ToolError::InvalidArguments`]——模型可以改了再来。
pub fn parse_args<T: serde::de::DeserializeOwned>(
    args: serde_json::Value,
    tool: &str,
) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|error| ToolError::InvalidArguments {
        message: format!("{tool} 的参数不合法：{error}"),
    })
}

/// 规范化后的参数：计划里存的是它，审批绑定的也是它，所以它必须来自结构体而不是
/// 模型给的原始 JSON（多一个未知字段就会改哈希）。
pub fn normalized<T: Serialize>(args: &T) -> Result<serde_json::Value, ToolError> {
    serde_json::to_value(args).map_err(|error| ToolError::InvalidArguments {
        message: format!("参数无法规范化：{error}"),
    })
}

/// 同一目录下不会撞名的一个后缀。
///
/// 不用 `tempfile`：它在这个 crate 里是 **dev-dependency**（§13.4 的依赖清单），只在
/// 测试里有。进程 ID + 单调计数 + 纳秒在同一台机器上足够区分，而"同目录里的临时文件"
/// 本来就只需要和自己的其它写入区分开。
pub fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos() as u64);
    format!(
        "{}-{}-{nanos}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// 新 `operation_id` 的时间戳。
///
/// `Tool::prepare` 的签名里没有 [`Clock`](komo_kernel::traits::Clock)，而这个时间只
/// 用来让 ID 按生成顺序排，不参与任何判决（判决的 `now` 由 executor 从时钟取）。
pub fn plan_time() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}

#[cfg(test)]
pub(crate) mod test_support {
    use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
    use komo_kernel::types::plan::{ApprovedPlan, ExecutionPlan, PlanSource};
    use komo_kernel::types::tool::{CancelToken, ToolContext, WorkspaceRoot};
    use std::path::Path;

    /// 一个以 `dir` 为可写根和 cwd 的上下文。
    pub fn context(dir: &Path) -> ToolContext {
        context_with_cancel(dir, CancelToken::new())
    }

    pub fn context_with_cancel(dir: &Path, cancel: CancelToken) -> ToolContext {
        let real = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        ToolContext {
            session: SessionId::from_raw("sess-1"),
            run: RunId::from_raw("run-1"),
            call: ToolCallId::from_raw("call-1"),
            attempt: AttemptId::from_raw("attempt-1"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            cwd: real.clone(),
            roots: vec![WorkspaceRoot {
                path: real,
                writable: true,
                label: "workspace".into(),
            }],
            env_version: None,
            resumed: None,
            cancel,
        }
    }

    /// 只在工具自己的单元测试里用：这里测的是**执行**，不是放行——放行在 executor
    /// 的测试里。
    pub fn approved(plan: ExecutionPlan) -> ApprovedPlan {
        ApprovedPlan::new(plan, komo_kernel::test_support::proof())
    }

    /// 这次尝试的流式写入器。生产里由 `ToolOutputStore::begin` 开、executor 借给
    /// `execute`；测试里用内存替身。
    pub fn writer(ctx: &ToolContext) -> komo_kernel::test_support::MemOutputWriter {
        komo_kernel::test_support::MemOutputWriter::new(komo_kernel::types::refs::AttemptRef {
            session: ctx.session.clone(),
            run: ctx.run.clone(),
            call: ctx.call.clone(),
            attempt: ctx.attempt.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_the_content_hash_first() {
        let one = FileVersion::of(b"hello", None);
        let two = FileVersion::of(b"hello", None);
        assert_eq!(one, two);
        assert_ne!(one.hash, FileVersion::of(b"hello!", None).hash);
        assert_eq!(one.size, 5);
    }

    #[test]
    fn an_expected_version_reads_from_a_bare_hash_or_the_whole_object() {
        let bare: ExpectedVersion = serde_json::from_str("\"abc\"").unwrap();
        assert_eq!(bare.hash().as_str(), "abc");

        let full: ExpectedVersion =
            serde_json::from_str(r#"{"hash":"abc","size":5,"modified_ms":1}"#).unwrap();
        assert_eq!(full.hash().as_str(), "abc");
    }

    #[test]
    fn a_missing_file_is_not_an_error_it_is_an_absence() {
        let dir = tempfile::tempdir().unwrap();
        assert!(current(&dir.path().join("nope.txt")).unwrap().is_none());
    }

    #[test]
    fn binary_content_is_refused_with_a_usable_message() {
        let error = as_text(vec![0xff, 0xfe], Path::new("/tmp/x.bin")).unwrap_err();
        let ToolError::InvalidArguments { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("shell"), "{message}");
    }
}
