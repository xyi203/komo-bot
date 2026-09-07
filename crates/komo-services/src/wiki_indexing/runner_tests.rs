use super::*;

fn runner() -> WikiIndexRunner {
    // The handles are never touched: every test here is about the gate, and
    // claiming does no I/O.
    struct NoIndex;
    #[async_trait::async_trait]
    impl ChunkIndex for NoIndex {
        async fn upsert(&self, _chunks: &[IndexedChunk]) -> anyhow::Result<()> {
            unreachable!()
        }
        async fn delete_paths(&self, _paths: &[String]) -> anyhow::Result<()> {
            unreachable!()
        }
        async fn indexed(
            &self,
        ) -> anyhow::Result<HashMap<String, komo_core::domain::chunk_index::IndexedFile>> {
            unreachable!()
        }
        async fn search(
            &self,
            _vector: &[f32],
            _query: &str,
            _limit: usize,
            _floor: f32,
        ) -> anyhow::Result<Vec<komo_core::domain::chunk_index::ChunkHit>> {
            unreachable!()
        }
        async fn count(&self) -> anyhow::Result<usize> {
            unreachable!()
        }
        async fn reset(&self) -> anyhow::Result<()> {
            unreachable!()
        }
        async fn vector_spec(&self) -> anyhow::Result<Option<(usize, String)>> {
            unreachable!()
        }
    }
    struct NoEmbed;
    #[async_trait::async_trait]
    impl EmbeddingClient for NoEmbed {
        async fn embed(&self, _texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            unreachable!()
        }
        fn model_id(&self) -> &str {
            "test-model"
        }
    }
    WikiIndexRunner::new(
        Arc::new(NoIndex),
        Arc::new(NoEmbed),
        PathBuf::from("/nowhere"),
        "test-model".to_string(),
    )
}

#[test]
fn a_second_claim_is_refused_while_one_is_held() {
    let r = runner();
    let first = r.claim(true, 1000).expect("the first claim wins");
    let Err(busy) = r.claim(false, 1005) else {
        panic!("the second claim must be refused");
    };
    assert_eq!(busy.since, 1000);
    assert!(busy.rebuild, "the caller must learn a rebuild is running");
    drop(first);
    // Freed once the claim is gone.
    assert!(r.claim(false, 1010).is_ok());
}

/// The reason a claim is a guard: an abandoned background run must not lock
/// indexing out for the life of the process.
#[test]
fn dropping_a_claim_frees_the_slot_and_records_a_failure() {
    let r = runner();
    drop(r.claim(true, 1000).unwrap());
    let snapshot = r.snapshot();
    assert!(snapshot.running_since.is_none(), "slot must be free");
    let last = snapshot.last.expect("an abandoned run is still a run");
    assert!(last.rebuild);
    assert!(
        last.result.is_err(),
        "an abandoned rebuild must not read as a success — the store is emptied"
    );
}

#[test]
fn snapshot_reports_the_in_flight_run() {
    let r = runner();
    assert!(r.snapshot().running_since.is_none());
    let _claim = r.claim(false, 2000).unwrap();
    let snapshot = r.snapshot();
    assert_eq!(snapshot.running_since, Some(2000));
    assert!(!snapshot.running_rebuild);
}
