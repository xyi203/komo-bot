//! `sessions/<id>/payloads/`：超限的模型消息或执行计划正文（§8.3）。
//!
//! JSONL 只保留引用与内容哈希；**同一参数不再另存重复全文**——所以文件按内容哈希命名，
//! 写第二遍就是同一个文件。
//!
//! 单次参数超过 [`INLINE_ARGUMENT_LIMIT_BYTES`]（4 KiB）时外置，较大的准备计划同样外置。
//! 引用里带相对该 Session 目录的受控路径、大小、内容哈希，以及必要的字段定位
//! （[`ContentRef::pointer`]）。

use komo_kernel::traits::StoreError;
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::refs::{ContentRef, INLINE_ARGUMENT_LIMIT_BYTES, PayloadRef};

use crate::session_log::{SessionPaths, create_dir_all_synced, sync_dir};

/// 一个 Session 的外置正文目录。
#[derive(Debug, Clone)]
pub struct PayloadStore {
    paths: SessionPaths,
}

impl PayloadStore {
    pub fn new(paths: SessionPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &SessionPaths {
        &self.paths
    }

    /// 这段正文该不该外置。
    pub fn should_externalize(len: usize) -> bool {
        len > INLINE_ARGUMENT_LIMIT_BYTES
    }

    /// 存一段字节，返回引用。已经存在同样内容的文件就直接复用。
    pub async fn put(&self, bytes: &[u8]) -> Result<PayloadRef, StoreError> {
        self.put_with_pointer(bytes, None).await
    }

    /// 存一段字节，并在引用上记一个字段定位（例如 `payloads` 里某条消息的某个参数）。
    pub async fn put_with_pointer(
        &self,
        bytes: &[u8],
        pointer: Option<String>,
    ) -> Result<PayloadRef, StoreError> {
        let dir = self.paths.payloads();
        create_dir_all_synced(&dir).await?;

        let hash = ContentHash::of_bytes(bytes);
        let name = format!("{}.bin", hash.as_str());
        let relative = format!("payloads/{name}");
        let path = dir.join(&name);

        // 内容寻址：同样的字节已经在了就不再写一遍。
        let already = match tokio::fs::metadata(&path).await {
            Ok(meta) => meta.len() == bytes.len() as u64,
            Err(_) => false,
        };
        if !already {
            let temp = dir.join(format!("{name}.partial"));
            tokio::fs::write(&temp, bytes)
                .await
                .map_err(|e| StoreError::Io(format!("写 {} 失败：{e}", temp.display())))?;
            sync_file(&temp).await?;
            tokio::fs::rename(&temp, &path)
                .await
                .map_err(|e| StoreError::Io(format!("发布 {} 失败：{e}", path.display())))?;
            sync_dir(&dir).await?;
        }

        Ok(PayloadRef(ContentRef {
            path: relative,
            size: bytes.len() as u64,
            hash,
            pointer,
        }))
    }

    /// 存一个 JSON 值。
    pub async fn put_json<T: serde::Serialize>(
        &self,
        value: &T,
        pointer: Option<String>,
    ) -> Result<PayloadRef, StoreError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|e| StoreError::Other(format!("外置正文序列化失败：{e}")))?;
        self.put_with_pointer(&bytes, pointer).await
    }

    /// 按引用读回正文并**校验哈希**；不匹配返回 [`StoreError::Corrupt`]，不返回内容。
    pub async fn open(&self, reference: &PayloadRef) -> Result<Vec<u8>, StoreError> {
        let path = self.paths.resolve(&reference.0.path)?;
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::Corrupt(format!("引用的正文不在：{}", reference.0.path))
            } else {
                StoreError::Io(format!("读 {} 失败：{e}", path.display()))
            }
        })?;
        if bytes.len() as u64 != reference.0.size {
            return Err(StoreError::Corrupt(format!(
                "{} 的大小是 {}，引用说 {}",
                reference.0.path,
                bytes.len(),
                reference.0.size
            )));
        }
        if ContentHash::of_bytes(&bytes) != reference.0.hash {
            return Err(StoreError::Corrupt(format!(
                "{} 的内容哈希与引用不符",
                reference.0.path
            )));
        }
        Ok(bytes)
    }

    /// 按引用读回一个 JSON 值。
    pub async fn open_json<T: serde::de::DeserializeOwned>(
        &self,
        reference: &PayloadRef,
    ) -> Result<T, StoreError> {
        let bytes = self.open(reference).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| StoreError::Corrupt(format!("{} 不是预期的 JSON：{e}", reference.0.path)))
    }
}

pub(crate) async fn sync_file(path: &std::path::Path) -> Result<(), StoreError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| StoreError::Io(format!("打开 {} 失败：{e}", path.display())))?;
    file.sync_all()
        .await
        .map_err(|e| StoreError::Io(format!("同步 {} 失败：{e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::SessionId;

    fn store() -> (tempfile::TempDir, PayloadStore) {
        let dir = tempfile::tempdir().expect("临时目录");
        let paths = SessionPaths::new(dir.path(), &SessionId::from_raw("sess-1"));
        (dir, PayloadStore::new(paths))
    }

    #[tokio::test]
    async fn a_payload_round_trips_and_verifies_its_hash() {
        let (_dir, store) = store();
        let bytes = "汉字".repeat(4096);
        let reference = store.put(bytes.as_bytes()).await.unwrap();
        assert!(reference.path().starts_with("payloads/"));
        assert_eq!(reference.0.size, bytes.len() as u64);
        assert_eq!(store.open(&reference).await.unwrap(), bytes.as_bytes());
    }

    #[tokio::test]
    async fn the_same_content_is_only_stored_once() {
        let (dir, store) = store();
        let first = store.put(b"same").await.unwrap();
        let second = store.put(b"same").await.unwrap();
        assert_eq!(first, second, "内容寻址：同一参数不再另存重复全文");

        let files = std::fs::read_dir(dir.path().join("sess-1").join("payloads"))
            .unwrap()
            .count();
        assert_eq!(files, 1);
    }

    #[tokio::test]
    async fn a_tampered_payload_is_reported_as_corrupt_and_its_content_is_not_returned() {
        let (_dir, store) = store();
        let reference = store.put(b"original").await.unwrap();
        let path = store.paths().resolve(reference.path()).unwrap();
        tokio::fs::write(&path, b"tampered").await.unwrap();

        let error = store.open(&reference).await.unwrap_err();
        assert!(
            matches!(&error, StoreError::Corrupt(message) if message.contains("哈希")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_missing_payload_is_corruption_not_an_empty_body() {
        let (_dir, store) = store();
        let reference = store.put(b"gone").await.unwrap();
        tokio::fs::remove_file(store.paths().resolve(reference.path()).unwrap())
            .await
            .unwrap();
        assert!(matches!(
            store.open(&reference).await,
            Err(StoreError::Corrupt(_))
        ));
    }

    #[tokio::test]
    async fn a_pointer_survives_on_the_reference() {
        let (_dir, store) = store();
        let reference = store
            .put_with_pointer(b"args", Some("/tool_calls/0/arguments".into()))
            .await
            .unwrap();
        assert_eq!(
            reference.0.pointer.as_deref(),
            Some("/tool_calls/0/arguments")
        );
    }

    #[tokio::test]
    async fn json_round_trips() {
        let (_dir, store) = store();
        let value = serde_json::json!({"a": 1, "b": [1, 2, 3]});
        let reference = store.put_json(&value, None).await.unwrap();
        let back: serde_json::Value = store.open_json(&reference).await.unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn only_bodies_over_the_inline_limit_are_externalised() {
        assert!(!PayloadStore::should_externalize(
            INLINE_ARGUMENT_LIMIT_BYTES
        ));
        assert!(PayloadStore::should_externalize(
            INLINE_ARGUMENT_LIMIT_BYTES + 1
        ));
    }
}
