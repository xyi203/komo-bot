use super::*;

async fn index(name: &str) -> TursoChunkIndex {
    let path = std::env::temp_dir().join(format!("komo_chunk_{name}.db"));
    crate::persistence::reset_test_db(&path);
    let db = Arc::new(
        turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .unwrap(),
    );
    TursoChunkIndex::open(db, "komo_wiki").await.unwrap()
}

/// Unit vectors, as the `EmbeddingClient` contract guarantees.
fn chunk(path: &str, ordinal: usize, embedding: Vec<f32>) -> IndexedChunk {
    IndexedChunk {
        id: IndexedChunk::make_id(path, ordinal),
        path: path.to_string(),
        heading_path: format!("{path} > 节"),
        ordinal,
        text: format!("{path} 第{ordinal}段的正文"),
        mtime: 1780000000 + ordinal as i64,
        embedding,
        embedding_model: "test-model".into(),
    }
}

fn chunk_with_text(path: &str, text: &str, embedding: Vec<f32>) -> IndexedChunk {
    let mut c = chunk(path, 0, embedding);
    c.text = text.to_string();
    c.heading_path = path.to_string();
    c
}

/// A corpus that was never indexed must answer "empty", not error.
#[tokio::test]
async fn empty_index_reads_as_empty() {
    let index = index("empty").await;
    assert_eq!(index.count().await.unwrap(), 0);
    assert!(
        index
            .search(&[1.0, 0.0], "q", 5, 0.0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(index.indexed().await.unwrap().is_empty());
    assert!(index.vector_spec().await.unwrap().is_none());
    index.delete_paths(&["a.md".into()]).await.unwrap();
}

#[tokio::test]
async fn upsert_then_search_returns_the_nearest_chunk() {
    let index = index("nearest").await;
    index
        .upsert(&[
            chunk("a.md", 0, vec![1.0, 0.0]),
            chunk("b.md", 0, vec![0.0, 1.0]),
        ])
        .await
        .unwrap();

    assert_eq!(index.count().await.unwrap(), 2);
    assert_eq!(index.vector_spec().await.unwrap().unwrap().0, 2);
    assert_eq!(index.vector_spec().await.unwrap().unwrap().1, "test-model");

    let hits = index.search(&[1.0, 0.0], "", 1, 0.0).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chunk.path, "a.md");
    assert!(hits[0].score > 0.9, "score was {}", hits[0].score);
    // Payload survived the round trip; vectors deliberately did not.
    assert_eq!(hits[0].chunk.heading_path, "a.md > 节");
    assert!(hits[0].chunk.embedding.is_empty());
}

/// `min_score` must drop weak neighbours — an unrelated query always has a
/// nearest chunk, and returning it is how a search tool invents relevance.
#[tokio::test]
async fn min_score_filters_weak_hits() {
    let index = index("floor").await;
    index
        .upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
        .await
        .unwrap();
    // Orthogonal query: cosine 0.
    assert!(
        index
            .search(&[0.0, 1.0], "", 5, 0.5)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        index.search(&[0.0, 1.0], "", 5, -1.0).await.unwrap().len(),
        1
    );
}

/// Re-indexing an unchanged file must not duplicate its rows — this is what
/// the derived, stable chunk id buys.
#[tokio::test]
async fn upsert_is_idempotent() {
    let index = index("idempotent").await;
    let chunks = [
        chunk("a.md", 0, vec![1.0, 0.0]),
        chunk("a.md", 1, vec![0.0, 1.0]),
    ];
    index.upsert(&chunks).await.unwrap();
    index.upsert(&chunks).await.unwrap();
    assert_eq!(index.count().await.unwrap(), 2);
}

#[tokio::test]
async fn indexed_groups_by_path_and_delete_removes_a_whole_file() {
    let index = index("grouping").await;
    index
        .upsert(&[
            chunk("a.md", 0, vec![1.0, 0.0]),
            chunk("a.md", 1, vec![0.0, 1.0]),
            chunk("b.md", 0, vec![0.0, 1.0]),
        ])
        .await
        .unwrap();

    let indexed = index.indexed().await.unwrap();
    assert_eq!(indexed.len(), 2);
    assert_eq!(indexed["a.md"].chunks, 2);
    assert_eq!(indexed["b.md"].chunks, 1);
    // Oldest mtime wins, so a half-updated file re-indexes.
    assert_eq!(indexed["a.md"].mtime, 1780000000);

    index.delete_paths(&["a.md".into()]).await.unwrap();
    assert_eq!(index.count().await.unwrap(), 1);
    assert!(index.indexed().await.unwrap().contains_key("b.md"));
}

/// The entire reason hybrid exists: an exact token the dense arm cannot
/// reach. The query vector points *away* from the note holding the id, so
/// only the lexical arm can surface it.
#[tokio::test]
async fn lexical_arm_finds_an_exact_id_the_dense_arm_points_away_from() {
    let index = index("lexical").await;
    index
        .upsert(&[
            chunk_with_text(
                "orders.md",
                "订单 ORD-A1B2C3 在 complete 步骤连续提交失败",
                vec![1.0, 0.0],
            ),
            chunk_with_text(
                "unrelated.md",
                "今天的天气很好，适合出门散步",
                vec![0.0, 1.0],
            ),
        ])
        .await
        .unwrap();

    // Vector points at `unrelated.md`; the text names the id in `orders.md`.
    let hits = index
        .search(&[0.0, 1.0], "ORD-A1B2C3", 5, -1.0)
        .await
        .unwrap();
    let paths: Vec<&str> = hits.iter().map(|h| h.chunk.path.as_str()).collect();
    assert!(
        paths.contains(&"orders.md"),
        "lexical arm did not surface the exact id: {paths:?}"
    );
}

/// CJK must tokenize, or the lexical arm is dead weight on this vault.
#[tokio::test]
async fn chinese_text_matches_a_chinese_query() {
    let index = index("cjk").await;
    index
        .upsert(&[
            chunk_with_text(
                "order.md",
                "订单创建失败的排查记录与链路还原",
                vec![1.0, 0.0],
            ),
            chunk_with_text("weather.md", "今天的天气很好", vec![0.0, 1.0]),
        ])
        .await
        .unwrap();
    let hits = index
        .search(&[0.0, 1.0], "订单创建失败", 5, -1.0)
        .await
        .unwrap();
    assert_eq!(hits[0].chunk.path, "order.md", "{hits:?}");
}

/// A rare term must outweigh a common one, or a query mixing an id with
/// everyday words ranks the everyday words.
#[tokio::test]
async fn a_rare_term_outranks_shared_common_ones() {
    let index = index("idf").await;
    let mut chunks: Vec<IndexedChunk> = (0..8)
        .map(|i| {
            let mut c = chunk_with_text(
                &format!("common{i}.md"),
                "结账 服务 编排 记录",
                vec![0.0, 1.0],
            );
            c.heading_path = format!("common{i}.md");
            c
        })
        .collect();
    chunks.push(chunk_with_text("rare.md", "ORD-ZZZ9 结账", vec![0.0, 1.0]));
    index.upsert(&chunks).await.unwrap();

    // Every common note shares three terms with the query; `rare.md` shares
    // one of those plus the id nothing else carries. Read off the arm
    // itself — fusion with a dense arm that scores all nine alike would say
    // nothing about the lexical ranking.
    let hits = index
        .lexical_arm("结账 服务 编排 ORD-ZZZ9", 5)
        .await
        .unwrap();
    assert_eq!(hits[0].chunk.path, "rare.md", "{hits:?}");
}

/// Measured on the real vault: one note held 9 of the dense arm's top 15,
/// so fusion never saw the notes ranked behind it.
#[tokio::test]
async fn one_note_cannot_monopolize_an_arm_before_fusion() {
    let index = index("hog").await;
    let mut chunks: Vec<IndexedChunk> = (0..6)
        .map(|i| {
            let mut c = chunk("hog.md", i, vec![1.0, 0.0]);
            c.text = "结账 服务 编排".into();
            c
        })
        .collect();
    chunks.push(chunk_with_text(
        "rival.md",
        "结账 服务 地图",
        vec![0.99, 0.01],
    ));
    index.upsert(&chunks).await.unwrap();

    // Dense-only, so this isolates the arm cap from anything lexical.
    let hits = index.search(&[1.0, 0.0], "", 5, -1.0).await.unwrap();
    let paths: Vec<&str> = hits.iter().map(|h| h.chunk.path.as_str()).collect();
    assert_eq!(
        paths.iter().filter(|p| **p == "hog.md").count(),
        MAX_CHUNKS_PER_FILE,
        "{paths:?}"
    );
    assert!(paths.contains(&"rival.md"), "{paths:?}");
}

/// A query with no usable terms leaves the lexical arm empty, and the hits
/// must then still carry a cosine — the scale `min_score` is on.
#[tokio::test]
async fn dense_only_search_keeps_cosine_scores() {
    let index = index("dense_only").await;
    index
        .upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
        .await
        .unwrap();
    let hits = index.search(&[1.0, 0.0], "", 5, 0.0).await.unwrap();
    assert_eq!(hits.len(), 1);
    // Cosine, not an RRF value (which would be ~0.5 here).
    assert!(hits[0].score > 0.9, "score was {}", hits[0].score);
}

/// Changing embedding model changes vector width, and two widths cannot be
/// compared. This must say so, and say what fixes it.
#[tokio::test]
async fn a_different_vector_width_is_rejected_with_a_fix() {
    let index = index("width").await;
    index
        .upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
        .await
        .unwrap();

    let err = index
        .upsert(&[chunk("b.md", 0, vec![1.0, 0.0, 0.0])])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("2-dim") && err.contains("3-dim"), "{err}");
    assert!(err.contains("--rebuild"), "must name the fix: {err}");
}

/// `reset` is what makes a model change possible: after it, an index built
/// for one width accepts another.
#[tokio::test]
async fn reset_allows_a_new_vector_width() {
    let index = index("reset").await;
    index
        .upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
        .await
        .unwrap();
    assert_eq!(index.count().await.unwrap(), 1);

    index.reset().await.unwrap();
    assert_eq!(index.count().await.unwrap(), 0);

    index
        .upsert(&[chunk("a.md", 0, vec![0.0, 1.0, 0.0])])
        .await
        .unwrap();
    assert_eq!(index.vector_spec().await.unwrap().unwrap().0, 3);
    assert_eq!(index.count().await.unwrap(), 1);
}

/// One index stores one vector width; a mixed batch is a bug upstream and
/// must be rejected loudly rather than half-written.
#[tokio::test]
async fn mixed_vector_widths_are_rejected() {
    let index = index("mixed").await;
    let err = index
        .upsert(&[
            chunk("a.md", 0, vec![1.0, 0.0]),
            chunk("b.md", 0, vec![1.0, 0.0, 0.0]),
        ])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("mixed vector widths"), "{err}");
}

/// Two corpora, one table: neither may see the other's chunks, even when a
/// chunk id collides.
#[tokio::test]
async fn collections_are_isolated() {
    let path = std::env::temp_dir().join("komo_chunk_collections.db");
    crate::persistence::reset_test_db(&path);
    let db = Arc::new(
        turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .unwrap(),
    );
    let wiki = TursoChunkIndex::open(db.clone(), "komo_wiki")
        .await
        .unwrap();
    let sessions = TursoChunkIndex::open(db, "komo_sessions").await.unwrap();

    wiki.upsert(&[chunk_with_text("a.md", "笔记正文", vec![1.0, 0.0])])
        .await
        .unwrap();
    sessions
        .upsert(&[chunk_with_text("a.md", "会话正文", vec![1.0, 0.0])])
        .await
        .unwrap();

    assert_eq!(wiki.count().await.unwrap(), 1);
    assert_eq!(sessions.count().await.unwrap(), 1);
    let hits = wiki.search(&[1.0, 0.0], "", 5, -1.0).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chunk.text, "笔记正文");
    sessions.reset().await.unwrap();
    assert_eq!(wiki.count().await.unwrap(), 1, "reset crossed collections");
}

/// The two corpora may run different embedding models — `[wiki]` is allowed
/// its own — so one table can hold two vector widths at once. Comparing
/// them is an error in the engine, and a wiki search must never reach a
/// session row to find that out.
#[tokio::test]
async fn a_search_ignores_another_collection_of_a_different_width() {
    let path = std::env::temp_dir().join("komo_chunk_widths.db");
    crate::persistence::reset_test_db(&path);
    let db = Arc::new(
        turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .unwrap(),
    );
    let wiki = TursoChunkIndex::open(db.clone(), WIKI).await.unwrap();
    let sessions = TursoChunkIndex::open(db, SESSIONS).await.unwrap();

    wiki.upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
        .await
        .unwrap();
    sessions
        .upsert(&[chunk("t.md", 0, vec![0.0, 1.0, 0.0])])
        .await
        .unwrap();

    let hits = wiki.search(&[1.0, 0.0], "", 5, -1.0).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chunk.path, "a.md");
    let hits = sessions
        .search(&[0.0, 1.0, 0.0], "", 5, -1.0)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chunk.path, "t.md");
}

#[test]
fn a_term_column_is_space_delimited_and_padded() {
    let column = term_column("订单 ORD-1");
    assert!(column.starts_with(' ') && column.ends_with(' '), "{column}");
    assert!(column.contains(" 订单 "), "{column}");
    assert!(column.contains(" ord "), "{column}");
}

#[test]
fn text_with_no_terms_yields_an_empty_column() {
    assert!(term_column("!!! ,,,").is_empty());
}

#[test]
fn idf_falls_as_a_term_gets_common() {
    assert!(idf(1, 1000) > idf(500, 1000));
    assert!(idf(1000, 1000) >= 0.0, "never negative");
}
