//! 关键词与向量召回的 P95 实测（§14「不预先宣称性能达标」那一行）。
//!
//! 全部 `#[ignore]`：它们要造一万条合成记忆，跑一遍是几十秒，不该挂在 `cargo test` 的
//! 主路上。跑法：
//!
//! ```text
//! cargo test -p komo-store --lib -- --ignored --nocapture repos::memory::bench
//! ```
//!
//! 它测的是**这台机器上这个实现**的数字，不是一句结论。§14 的待验证表要的就是数字：
//! 「在目标 NAS 与 Mac 上测量至少 1 千与 1 万条代表性记忆的索引时间、检索 P95 和内存，
//! 再决定是否需要近似向量索引」——所以这里印出索引时间、关键词 P95、向量 P95 与库文件
//! 大小，让那张表填得上，而不是在这里替它下判断。
//!
//! 合成数据刻意做成**中英混排**：关键词臂对 CJK 切 bigram、对 ASCII 切词，一条全英文的
//! 语料量不出中文查询要扫多少行。

use std::time::Instant;

use komo_kernel::protocol::http::IndexState;
use komo_kernel::traits::MemoryRepo;
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{
    Confirmation, ExtractionMetadata, MemoryItem, MemoryKind, MemoryScope, MemoryState,
    MemoryUsage, Provenance, RecallQuery, RetrievalMode,
};
use komo_kernel::types::model::{DistanceRule, EmbeddingSpace, Vector};
use time::OffsetDateTime;

use super::TursoMemoryRepo;
use crate::db::Db;

/// 向量维度。768 是常见的多语言 embedding 维度（bge-m3 是 1024，e5-base 是 768）。
const DIMENSIONS: u32 = 768;

const SUBJECTS: [&str; 10] = [
    "客厅空调",
    "书房台灯",
    "komo 仓库",
    "周报",
    "地铁通勤",
    "咖啡机",
    "NAS 备份",
    "会议纪要",
    "跑步计划",
    "阳台绿植",
];
const PREDICATES: [&str; 10] = [
    "设成 26 度",
    "每周五晚上处理",
    "用 cargo test --workspace 跑",
    "放在 /srv/data 下",
    "需要先 review 再合并",
    "换成深色主题",
    "改用 rustls 而不是 openssl",
    "保留最近 20 条记录",
    "早上七点提醒一次",
    "两周浇一次水",
];

fn synthetic(index: usize, now: OffsetDateTime) -> MemoryItem {
    let subject = SUBJECTS[index % SUBJECTS.len()];
    let predicate = PREDICATES[(index / SUBJECTS.len()) % PREDICATES.len()];
    MemoryItem {
        id: MemoryId::from_raw(format!("mem-{index:06}")),
        revision: 1,
        content: format!("{subject}{predicate}（第 {index} 条 note-{index:06}）"),
        kind: MemoryKind::Fact,
        scope: if index.is_multiple_of(3) {
            MemoryScope::Personal
        } else {
            MemoryScope::Project {
                project_id: format!("proj-{}", index % 7),
            }
        },
        provenance: Provenance::UserStatement,
        confirmation: Confirmation::Unconfirmed,
        state: MemoryState::Active,
        evidence: vec![],
        observed_at: now,
        valid_until: None,
        created_at: now,
        updated_at: now,
        extraction: ExtractionMetadata::new("bench", None, "bench/v1"),
        usage: MemoryUsage::default(),
        supersedes: None,
    }
}

/// 确定性的"伪 embedding"：正文哈希摊到 768 维。**不是真语义**，但向量臂的代价只取决
/// 于维度和条数，与向量里装的是什么无关。
fn fake_vector(text: &str) -> Vector {
    let digest = komo_kernel::types::digest::sha256(text.as_bytes());
    let mut values = Vec::with_capacity(DIMENSIONS as usize);
    for i in 0..DIMENSIONS as usize {
        values.push((digest[i % 32] as f32 - 127.5) / 127.5);
    }
    let norm: f32 = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    for value in &mut values {
        *value /= norm;
    }
    Vector(values)
}

fn space() -> EmbeddingSpace {
    EmbeddingSpace {
        provider: "bench".into(),
        endpoint: "memory://bench".into(),
        model: "synthetic".into(),
        revision: Some("1".into()),
        dimensions: DIMENSIONS,
        preprocessing: "bench".into(),
        document_prefix: String::new(),
        query_prefix: String::new(),
        normalized: true,
        distance: DistanceRule::Cosine,
        effort: None,
    }
}

fn percentile(mut samples: Vec<u128>, p: f64) -> u128 {
    samples.sort_unstable();
    if samples.is_empty() {
        return 0;
    }
    let index = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[index]
}

fn query(text: &str, mode: RetrievalMode, probe: Option<Vector>) -> RecallQuery {
    RecallQuery {
        text: text.into(),
        mode,
        scopes: vec![],
        candidate_limit: 40,
        top_k: 8,
        max_tokens: 1500,
        now: OffsetDateTime::now_utc(),
        query_vector: probe,
        include_states: vec![],
    }
}

/// 一次完整的测量：建库、写入、索引、查 100 次。
async fn measure(count: usize) {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("state.db");
    let db = Db::connect(&path).await.expect("打开库");
    let repo = TursoMemoryRepo::new(db);
    let now = OffsetDateTime::now_utc();

    // ---- 写入（正文 + 关键词索引，`put` 一步做完）
    let write_started = Instant::now();
    for index in 0..count {
        repo.put(synthetic(index, now), None).await.expect("写得下");
    }
    let write_ms = write_started.elapsed().as_millis();

    // ---- 向量索引
    repo.put_generation("bench-gen", Some(&space()), IndexState::Building, true)
        .await
        .expect("登记代次");
    let index_started = Instant::now();
    for index in 0..count {
        let item = synthetic(index, now);
        repo.put_vector(&item.id, 1, "bench-gen", fake_vector(&item.content))
            .await
            .expect("写向量");
    }
    let index_ms = index_started.elapsed().as_millis();

    // ---- 查询
    let probe = fake_vector("客厅空调设成 26 度");
    let mut keyword = Vec::new();
    let mut vector = Vec::new();
    let mut hybrid = Vec::new();
    for round in 0..100usize {
        let text = format!(
            "{}{}",
            SUBJECTS[round % SUBJECTS.len()],
            PREDICATES[round % PREDICATES.len()]
        );

        let started = Instant::now();
        repo.recall(&query(&text, RetrievalMode::Keyword, None))
            .await
            .expect("关键词");
        keyword.push(started.elapsed().as_micros());

        let started = Instant::now();
        repo.recall(&query(&text, RetrievalMode::Vector, Some(probe.clone())))
            .await
            .expect("向量");
        vector.push(started.elapsed().as_micros());

        let started = Instant::now();
        repo.recall(&query(&text, RetrievalMode::Hybrid, Some(probe.clone())))
            .await
            .expect("混合");
        hybrid.push(started.elapsed().as_micros());
    }

    let db_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let log_bytes = std::fs::metadata(path.with_extension("db-log"))
        .map(|m| m.len())
        .unwrap_or(0);

    println!("\n===== {count} 条记忆（{DIMENSIONS} 维向量）=====");
    println!(
        "写入正文 + 关键词索引  {write_ms} ms（{:.2} ms/条）",
        write_ms as f64 / count as f64
    );
    println!(
        "写入向量               {index_ms} ms（{:.2} ms/条）",
        index_ms as f64 / count as f64
    );
    println!(
        "关键词 P50 / P95       {:.1} / {:.1} ms",
        percentile(keyword.clone(), 0.50) as f64 / 1000.0,
        percentile(keyword, 0.95) as f64 / 1000.0
    );
    println!(
        "向量   P50 / P95       {:.1} / {:.1} ms",
        percentile(vector.clone(), 0.50) as f64 / 1000.0,
        percentile(vector, 0.95) as f64 / 1000.0
    );
    println!(
        "混合   P50 / P95       {:.1} / {:.1} ms",
        percentile(hybrid.clone(), 0.50) as f64 / 1000.0,
        percentile(hybrid, 0.95) as f64 / 1000.0
    );
    println!(
        "state.db / -log        {:.1} MiB / {:.1} MiB",
        db_bytes as f64 / 1024.0 / 1024.0,
        log_bytes as f64 / 1024.0 / 1024.0
    );
}

#[tokio::test]
#[ignore = "基准：要造 1 千条合成记忆，跑一遍十几秒"]
async fn p95_at_one_thousand_memories() {
    measure(1_000).await;
}

#[tokio::test]
#[ignore = "基准：要造 1 万条合成记忆，跑一遍几分钟"]
async fn p95_at_ten_thousand_memories() {
    measure(10_000).await;
}
