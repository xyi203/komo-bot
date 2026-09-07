use super::*;
use async_trait::async_trait;
use komo_core::domain::chunk_index::{ChunkHit, IndexedFile};
use std::sync::Mutex;

#[derive(Default)]
struct FakeIndex {
    chunks: Mutex<Vec<IndexedChunk>>,
    resets: Mutex<usize>,
}

#[async_trait]
impl ChunkIndex for FakeIndex {
    async fn upsert(&self, chunks: &[IndexedChunk]) -> anyhow::Result<()> {
        let mut held = self.chunks.lock().unwrap();
        for chunk in chunks {
            held.retain(|c| c.id != chunk.id);
            held.push(chunk.clone());
        }
        Ok(())
    }
    async fn search(&self, _: &[f32], _: &str, _: usize, _: f32) -> anyhow::Result<Vec<ChunkHit>> {
        Ok(Vec::new())
    }
    async fn indexed(&self) -> anyhow::Result<HashMap<String, IndexedFile>> {
        let mut out: HashMap<String, IndexedFile> = HashMap::new();
        for chunk in self.chunks.lock().unwrap().iter() {
            let entry = out.entry(chunk.path.clone()).or_insert(IndexedFile {
                mtime: chunk.mtime,
                chunks: 0,
            });
            entry.chunks += 1;
            entry.mtime = entry.mtime.min(chunk.mtime);
        }
        Ok(out)
    }
    async fn delete_paths(&self, paths: &[String]) -> anyhow::Result<()> {
        self.chunks
            .lock()
            .unwrap()
            .retain(|c| !paths.contains(&c.path));
        Ok(())
    }
    async fn count(&self) -> anyhow::Result<usize> {
        Ok(self.chunks.lock().unwrap().len())
    }
    async fn reset(&self) -> anyhow::Result<()> {
        self.chunks.lock().unwrap().clear();
        *self.resets.lock().unwrap() += 1;
        Ok(())
    }
    async fn vector_spec(&self) -> anyhow::Result<Option<(usize, String)>> {
        Ok(None)
    }
}

struct FakeEmbedder;

#[async_trait]
impl EmbeddingClient for FakeEmbedder {
    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
    }
    fn model_id(&self) -> &str {
        "fake"
    }
}

fn vault_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }
    dir
}

async fn run(index: &FakeIndex, vault: &Path, rebuild: bool) -> IndexOutcome {
    index_vault(index, &FakeEmbedder, vault, "fake", rebuild, |_| {})
        .await
        .unwrap()
}

#[tokio::test]
async fn first_run_indexes_everything() {
    let vault = vault_with(&[("a.md", "甲的正文内容"), ("b.md", "乙的正文内容")]);
    let index = FakeIndex::default();
    let out = run(&index, vault.path(), false).await;
    assert_eq!(out.files_seen, 2);
    assert_eq!(out.files_changed, 2);
    assert!(out.chunks_written >= 2);
    assert_eq!(out.chunks_total, out.chunks_written);
}

/// The whole point of the mtime diff: a second run must embed nothing.
#[tokio::test]
async fn second_run_skips_unchanged_files() {
    let vault = vault_with(&[("a.md", "甲的正文内容")]);
    let index = FakeIndex::default();
    run(&index, vault.path(), false).await;

    let out = run(&index, vault.path(), false).await;
    assert_eq!(out.files_changed, 0);
    assert_eq!(out.chunks_written, 0);
    assert!(out.chunks_total > 0, "index must still hold the chunks");
}

/// A note deleted from the vault must lose its chunks.
#[tokio::test]
async fn removed_files_are_deleted_from_the_index() {
    let vault = vault_with(&[("a.md", "甲的正文内容"), ("b.md", "乙的正文内容")]);
    let index = FakeIndex::default();
    run(&index, vault.path(), false).await;

    std::fs::remove_file(vault.path().join("b.md")).unwrap();
    let out = run(&index, vault.path(), false).await;
    assert_eq!(out.files_removed, 1);
    let indexed = index.indexed().await.unwrap();
    assert!(!indexed.contains_key("b.md"), "{indexed:?}");
}

/// A shortened note must not keep the tail chunks of its longer version.
#[tokio::test]
async fn a_shortened_note_drops_its_orphaned_chunks() {
    let long = "很长的一段内容。".repeat(300);
    let vault = vault_with(&[("a.md", long.as_str())]);
    let index = FakeIndex::default();
    let first = run(&index, vault.path(), false).await;
    assert!(first.chunks_written > 1);

    std::fs::write(vault.path().join("a.md"), "短".repeat(20)).unwrap();
    // mtime resolution is one second, so a rewrite within the same second
    // would look unchanged; stamp an explicit time instead of sleeping.
    let file = std::fs::File::options()
        .write(true)
        .open(vault.path().join("a.md"))
        .unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000)),
    )
    .unwrap();
    let second = run(&index, vault.path(), false).await;
    assert_eq!(second.chunks_total, second.chunks_written);
    assert!(second.chunks_total < first.chunks_written);
}

#[tokio::test]
async fn rebuild_resets_the_store() {
    let vault = vault_with(&[("a.md", "甲的正文内容")]);
    let index = FakeIndex::default();
    run(&index, vault.path(), false).await;
    run(&index, vault.path(), true).await;
    assert_eq!(*index.resets.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_missing_vault_is_an_error() {
    let index = FakeIndex::default();
    let err = index_vault(
        &index,
        &FakeEmbedder,
        Path::new("/definitely/not/here"),
        "fake",
        false,
        |_| {},
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("vault not found"), "{err}");
}

#[tokio::test]
async fn dot_directories_are_skipped() {
    let vault = vault_with(&[
        ("a.md", "甲的正文内容"),
        (".obsidian/workspace.md", "不该被索引"),
    ]);
    let index = FakeIndex::default();
    let out = run(&index, vault.path(), false).await;
    assert_eq!(out.files_seen, 1);
}
