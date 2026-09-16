//! 按尝试身份在 Session 目录里找那份没人认领的 `output.json`。
//!
//! 这是 [`OrphanOutputs`](super::OrphanOutputs) 的生产实现。它读的就是 store 写下的那份
//! 文档——**身份在文件里，不只在路径里**（`OutputDocument` 的 session / run / call /
//! attempt 四个字段），所以「不能只凭文件存在判断」（§14 故障注入表）在这里是逐字段
//! 核对，不是一次 `exists()`。
//!
//! 引用（大小 + 哈希）是从字节**算**出来的，不是从哪个事件里读的——正因为没有那条事件，
//! 才需要它。算完之后 [`ToolOutputStore::open`](komo_kernel::traits::ToolOutputStore::open)
//! 会拿同一份字节再核对一遍，于是"自己算的哈希自己认"这条捷径走不通：真正决定成不成立
//! 的是文件内容与文档里的身份。

use std::path::PathBuf;

use async_trait::async_trait;
use komo_kernel::traits::StoreError;
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::refs::{AttemptRef, ContentRef, OutputRef, PublishedOutput};
use komo_store::session_log::SessionPaths;
use komo_store::tool_output::OutputDocument;

use super::OrphanOutputs;

/// 在 `<sessions_dir>/<session>/tool-output/<run>/<call>/<attempt>/output.json` 里找。
#[derive(Debug, Clone)]
pub struct SessionDirOrphanOutputs {
    sessions_dir: PathBuf,
}

impl SessionDirOrphanOutputs {
    /// `sessions_dir` 就是快照里的 [`PathsConfig::sessions_dir`](komo_kernel::protocol::config::PathsConfig::sessions_dir)。
    pub fn new(sessions_dir: impl Into<PathBuf>) -> Self {
        SessionDirOrphanOutputs {
            sessions_dir: sessions_dir.into(),
        }
    }

    /// 这次尝试的输出在 Session 目录里的相对路径——`OutputRef` 里存的就是它（§8.3）。
    pub fn relative(attempt: &AttemptRef) -> String {
        format!(
            "tool-output/{}/{}/{}/output.json",
            attempt.run, attempt.call, attempt.attempt
        )
    }
}

#[async_trait]
impl OrphanOutputs for SessionDirOrphanOutputs {
    async fn find(&self, attempt: &AttemptRef) -> Result<Option<PublishedOutput>, StoreError> {
        let relative = Self::relative(attempt);
        let path = SessionPaths::new(&self.sessions_dir, &attempt.session)
            .root()
            .join(&relative);

        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            // 没有这份输出——那就是"没跑到落盘那一步"，交给工具自己的核对。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(StoreError::Io(format!(
                    "读 {} 失败：{error}",
                    path.display()
                )));
            }
        };

        let document: OutputDocument = serde_json::from_slice(&bytes)
            .map_err(|e| StoreError::Corrupt(format!("{relative} 不是 output.json：{e}")))?;

        // 身份逐字段核对。对不上的是**另一次**执行留下的东西，不是这次的证据。
        let identity = [
            (
                "session",
                document.session.as_str(),
                attempt.session.as_str(),
            ),
            ("run", document.run.as_str(), attempt.run.as_str()),
            ("call", document.call.as_str(), attempt.call.as_str()),
            (
                "attempt",
                document.attempt.as_str(),
                attempt.attempt.as_str(),
            ),
        ];
        for (field, found, expected) in identity {
            if found != expected {
                return Err(StoreError::Corrupt(format!(
                    "{relative} 里的 {field} 是 {found}，不是 {expected}"
                )));
            }
        }

        Ok(Some(PublishedOutput {
            output: OutputRef(ContentRef {
                path: relative,
                size: bytes.len() as u64,
                hash: ContentHash::of_bytes(&bytes),
                pointer: None,
            }),
            status: document.status,
            elapsed_ms: document.elapsed_ms,
            // 预览是给模型看的那一小段；补记时宁可没有，也不在这里凭空造一段
            // （完整正文在 `output.json` 里，读得到）。
            preview: None,
            stdout: document.stdout,
            stderr: document.stderr,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::traits::ToolOutputStore;
    use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
    use komo_kernel::types::refs::{ToolResultBody, ToolResultStatus};
    use komo_store::tool_output::FileToolOutputStore;

    fn attempt_ref(session: &SessionId) -> AttemptRef {
        AttemptRef {
            session: session.clone(),
            run: RunId::from_raw("run-1"),
            call: ToolCallId::from_raw("call-1"),
            attempt: AttemptId::from_raw("attempt-1"),
        }
    }

    async fn published(dir: &std::path::Path, attempt: &AttemptRef) -> PublishedOutput {
        let paths = SessionPaths::new(dir, &attempt.session);
        let store = FileToolOutputStore::new(paths, attempt.session.clone());
        let writer = store.begin(attempt).await.unwrap();
        store
            .publish(
                writer,
                ToolResultBody {
                    status: ToolResultStatus::Completed,
                    result: serde_json::json!({ "stdout": "hi" }),
                    error: None,
                    exit_code: Some(0),
                    artifacts: vec![],
                },
            )
            .await
            .unwrap()
    }

    /// 发布一份、把引用扔掉、再按身份找回来——找回来的引用要和原来那份一模一样。
    #[tokio::test]
    async fn a_published_output_is_found_again_by_identity_alone() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::from_raw("01a0a414-7800-7bbd-8fa1-632b66973666");
        let attempt = attempt_ref(&session);
        let original = published(dir.path(), &attempt).await;

        let found = SessionDirOrphanOutputs::new(dir.path())
            .find(&attempt)
            .await
            .unwrap()
            .expect("这份输出就在那儿");

        assert_eq!(found.output, original.output, "引用逐字相同");
        assert_eq!(found.status, ToolResultStatus::Completed);

        // 而且 store 认这个引用——哈希是从同一份字节算出来的。
        let store =
            FileToolOutputStore::new(SessionPaths::new(dir.path(), &session), session.clone());
        let verified = store.open(&found.output).await.unwrap();
        assert_eq!(verified.body.exit_code, Some(0));
    }

    #[tokio::test]
    async fn nothing_on_disk_is_not_an_error_it_is_an_absence() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::from_raw("01a0a414-7800-7bbd-8fa1-632b66973666");
        assert_eq!(
            SessionDirOrphanOutputs::new(dir.path())
                .find(&attempt_ref(&session))
                .await
                .unwrap(),
            None
        );
    }

    /// 「不能只凭文件存在判断」：身份对不上就是损坏。
    #[tokio::test]
    async fn a_document_belonging_to_another_attempt_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::from_raw("01a0a414-7800-7bbd-8fa1-632b66973666");
        let attempt = attempt_ref(&session);
        published(dir.path(), &attempt).await;

        // 把文件搬到另一次尝试的目录下：路径像，文件里的身份不像。
        let root = SessionPaths::new(dir.path(), &session).root().to_path_buf();
        let stranger = AttemptRef {
            attempt: AttemptId::from_raw("attempt-2"),
            ..attempt.clone()
        };
        let target = root.join(SessionDirOrphanOutputs::relative(&stranger));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(
            root.join(SessionDirOrphanOutputs::relative(&attempt)),
            &target,
        )
        .unwrap();

        let error = SessionDirOrphanOutputs::new(dir.path())
            .find(&stranger)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, StoreError::Corrupt(reason) if reason.contains("attempt")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_file_that_is_not_an_output_document_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::from_raw("01a0a414-7800-7bbd-8fa1-632b66973666");
        let attempt = attempt_ref(&session);
        let path = SessionPaths::new(dir.path(), &session)
            .root()
            .join(SessionDirOrphanOutputs::relative(&attempt));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ 半行").unwrap();

        assert!(matches!(
            SessionDirOrphanOutputs::new(dir.path())
                .find(&attempt)
                .await,
            Err(StoreError::Corrupt(_))
        ));
    }
}
