//! 崩溃注入（spikes/store.md 实验 B 的做法）：子进程提交若干次后 `std::process::abort()`，
//! 父进程重开库校验。
//!
//! 子进程怎么起：仓库里没有"自己 exec 自己"的先例（`komo/tests/codemode_worker.rs` 用的是
//! `CARGO_BIN_EXE_komo`，那是另一个二进制），所以这里按 libtest 的惯例——父测试 exec
//! `current_exe()`，参数 `--exact --ignored` 只跑下面那个 `#[ignore]` 的 [`crash_child`]，
//! 场景与目录经环境变量传入。没有环境变量时 `crash_child` 直接返回，所以手动
//! `--ignored` 全跑也不会误触发。子进程套一层 `sh -c 'ulimit -c 0'`，abort 不落 core。
//!
//! **这些测试证明的是进程级一致性**：`abort()` 之后页缓存仍然有效，所以哪怕 `synchronous=OFF`
//! 也会全绿；"是否 fsync"的判据是 strace（D0 做过），不是这里。测试跑在 tmpfs 上时 fsync
//! 本身也没有意义。要证的是：已确认的提交一条不少、顺序不乱，Progress 的结果是某个提交前缀，
//! 失败的事务回滚干净。

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

use turso::Value;

use super::blob::publish_blob;
use super::line::Line;
use super::write::{CommitKind, Write};

const ENV_SCENARIO: &str = "KOMO_DURABLE_CRASH_SCENARIO";
const ENV_DIR: &str = "KOMO_DURABLE_CRASH_DIR";
const ENV_N: &str = "KOMO_DURABLE_CRASH_N";

const SIGABRT: i32 = 6;

fn entry(id: i64, record: String) -> Write {
    Write::InsertEntry {
        id,
        conversation_id: 1,
        head: Some(id),
        record,
    }
}

fn setup_writes() -> Vec<Write> {
    vec![
        Write::InsertConversation {
            id: 1,
            owner_conversation: None,
            owner_task: None,
            parent_conversation: None,
            parent_at: None,
            session_id: None,
            record: "{}".into(),
        },
        Write::CreateDocument {
            id: 2,
            kind: "komo.live".into(),
            scope_kind: "conversation".into(),
            owner_id: 1,
            key_value: String::new(),
            version: 1,
            value: "{\"k\":-1}".into(),
        },
    ]
}

/// 混合场景的脚本：第 `i` 步（0 起）。每 5 步一个结构性 entry，其余是进度替换。
fn mixed_is_structural(i: usize) -> bool {
    i % 5 == 0
}

/// 子进程入口。只在父测试经环境变量要求时才干活，干完 abort。
#[tokio::test]
#[ignore = "崩溃注入的子进程入口：由 crash_tests 里的父测试 exec，不要单独跑"]
async fn crash_child() {
    let Ok(scenario) = std::env::var(ENV_SCENARIO) else {
        return;
    };
    let dir = std::path::PathBuf::from(std::env::var(ENV_DIR).unwrap());
    let n: usize = std::env::var(ENV_N).unwrap().parse().unwrap();
    let line = Line::open(&dir).await.unwrap();
    line.commit(CommitKind::Structural, setup_writes())
        .await
        .unwrap();

    match scenario.as_str() {
        "structural" => {
            for i in 0..n {
                line.commit_with(CommitKind::Structural, move |mint| {
                    vec![entry(mint.mint(), format!("{{\"i\":{i}}}"))]
                })
                .await
                .unwrap();
            }
        }
        "mixed" => {
            let mut next_entry = 3;
            for i in 0..n {
                if mixed_is_structural(i) {
                    line.commit(
                        CommitKind::Structural,
                        vec![entry(next_entry, format!("{{\"i\":{i}}}"))],
                    )
                    .await
                    .unwrap();
                    next_entry += 1;
                } else {
                    line.commit(
                        CommitKind::Progress,
                        vec![Write::ReplaceDocument {
                            version: 1,
                            id: 2,
                            value: format!("{{\"k\":{i}}}"),
                        }],
                    )
                    .await
                    .unwrap();
                }
            }
        }
        "blob_published" => {
            // 正确的顺序：blob 先发布，再提交引用它的 entry。
            let blob = publish_blob(&dir, "tool-output/9/1/output.json", b"{\"ok\":true}")
                .await
                .unwrap();
            let record = serde_json::json!({ "result": blob.reference }).to_string();
            line.commit(CommitKind::Structural, vec![entry(3, record)])
                .await
                .unwrap();
        }
        "blob_unsynced" => {
            // blob 写了一半（只有 .partial、没同步没改名）就崩：此时还没有任何 entry 引用它。
            let partial = dir.join("tool-output/9/1");
            std::fs::create_dir_all(&partial).unwrap();
            std::fs::write(partial.join("output.json.partial"), b"{\"ok\"").unwrap();
        }
        other => panic!("未知场景 {other}"),
    }
    std::process::abort();
}

fn run_child(scenario: &str, dir: &Path, n: usize) {
    let exe = std::env::current_exe().unwrap();
    let status = Command::new("sh")
        .arg("-c")
        .arg("ulimit -c 0; exec \"$@\"")
        .arg("sh")
        .arg(exe)
        .args([
            "durable::crash_tests::crash_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENV_SCENARIO, scenario)
        .env(ENV_DIR, dir)
        .env(ENV_N, n.to_string())
        .status()
        .unwrap();
    assert_eq!(
        status.signal(),
        Some(SIGABRT),
        "子进程应当被 abort 杀掉，实际 {status:?}"
    );
}

async fn query_ints(line: &Line, sql: &str) -> Vec<Vec<i64>> {
    let conn = line.read_connection().unwrap();
    let mut rows = conn.query(sql, ()).await.unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        let mut cols = Vec::new();
        for i in 0..row.column_count() {
            cols.push(match row.get_value(i).unwrap() {
                Value::Integer(n) => n,
                Value::Null => -1,
                Value::Text(t) => t.parse().unwrap_or_else(|_| panic!("不是整数：{t}")),
                other => panic!("{other:?}"),
            });
        }
        out.push(cols);
    }
    out
}

/// 任何 entry 里引用的 blob 都必须存在（blob 先于引用）。
async fn assert_every_referenced_blob_exists(line: &Line, root: &Path) {
    let conn = line.read_connection().unwrap();
    let mut rows = conn.query("SELECT record FROM entries", ()).await.unwrap();
    while let Some(row) = rows.next().await.unwrap() {
        let Value::Text(record) = row.get_value(0).unwrap() else {
            panic!("record 不是文本")
        };
        let json: serde_json::Value = serde_json::from_str(&record).unwrap();
        if let Some(path) = json.pointer("/result/path").and_then(|p| p.as_str()) {
            assert!(
                root.join(path).exists(),
                "entry 引用了不存在的 blob：{path}"
            );
        }
    }
}

#[tokio::test]
async fn n_structural_commits_before_abort_are_all_present_in_order_after_reopen() {
    const N: usize = 25;
    let dir = tempfile::tempdir().unwrap();
    run_child("structural", dir.path(), N);

    let line = Line::open(dir.path()).await.unwrap();
    let rows = query_ints(&line, "SELECT id, commit_seq FROM entries ORDER BY id").await;
    assert_eq!(rows.len(), N, "恰好 N 条");
    for (k, row) in rows.iter().enumerate() {
        // 取号序 = 提交序：第 k 条 entry 是 ID k+3（1、2 是 conversation 与 document），seq 是 k+2。
        assert_eq!(row, &vec![k as i64 + 3, k as i64 + 2]);
    }
    // seq 接续：setup 一次 + N 次，下一次是 N+2。
    let next = line
        .commit(CommitKind::Structural, vec![entry(1000, "{}".into())])
        .await
        .unwrap();
    assert_eq!(next, N as u64 + 2);
    // 取号器接续：重开后取到的是已有最大 ID（1000）之后的号。
    drop(line);
    tokio::task::yield_now().await;
    let line = Line::open(dir.path()).await.unwrap();
    assert_eq!(line.mint_id(), 1001);
}

#[tokio::test]
async fn progress_commits_mixed_with_structural_leave_a_commit_prefix_after_abort() {
    const N: usize = 48;
    let dir = tempfile::tempdir().unwrap();
    run_child("mixed", dir.path(), N);

    // 脚本第 i 步之后的合法状态：(结构性 entry 数, document 的 k, last_seq)。
    // 提交 1 是 setup（seq 1）；脚本第 i 步是 seq i+2。
    let mut valid = vec![(0usize, -1i64, 1u64)];
    let (mut entries, mut k) = (0usize, -1i64);
    for i in 0..N {
        if mixed_is_structural(i) {
            entries += 1;
        } else {
            k = i as i64;
        }
        valid.push((entries, k, i as u64 + 2));
    }
    let structural_total = (0..N).filter(|&i| mixed_is_structural(i)).count();

    let line = Line::open(dir.path()).await.unwrap();
    let ids: Vec<i64> = query_ints(&line, "SELECT id FROM entries ORDER BY id")
        .await
        .into_iter()
        .map(|r| r[0])
        .collect();
    let conn = line.read_connection().unwrap();
    let mut rows = conn
        .query(
            "SELECT json_extract(value, '$.k'), version FROM documents WHERE id = 2",
            (),
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let Value::Integer(observed_k) = row.get_value(0).unwrap() else {
        panic!("k")
    };
    drop(rows);
    let last_seq = query_ints(
        &line,
        "SELECT value FROM durable_meta WHERE name = 'last_seq'",
    )
    .await[0][0] as u64;

    // 结构性的一条不能丢、顺序不能乱：ID 连续递增。
    assert_eq!(ids, (3..3 + structural_total as i64).collect::<Vec<_>>());
    // 整体状态必须恰好是某个提交前缀。
    assert!(
        valid.contains(&(ids.len(), observed_k, last_seq)),
        "状态 ({}, {observed_k}, {last_seq}) 不是任何提交前缀",
        ids.len()
    );
    // 进程级崩溃不丢写（页缓存还在）：这里实际是完整前缀。断言它，是为了发现"进度提交
    // 在 abort 后凭空消失"这类回归；掉电下才允许丢，那不归这个测试证明。
    assert_eq!(last_seq, N as u64 + 1);
}

#[tokio::test]
async fn a_blob_published_before_the_commit_is_there_after_abort() {
    let dir = tempfile::tempdir().unwrap();
    run_child("blob_published", dir.path(), 0);

    let line = Line::open(dir.path()).await.unwrap();
    assert_eq!(
        query_ints(&line, "SELECT COUNT(*) FROM entries").await[0][0],
        1
    );
    assert_every_referenced_blob_exists(&line, dir.path()).await;
    let bytes = std::fs::read(dir.path().join("tool-output/9/1/output.json")).unwrap();
    assert_eq!(bytes, b"{\"ok\":true}");
}

/// 在 blob 同步完成之前崩溃：没有任何提交指向它，最终文件不存在，只剩可诊断的 `.partial`。
#[tokio::test]
async fn a_crash_before_the_blob_is_published_leaves_no_commit_pointing_at_it() {
    let dir = tempfile::tempdir().unwrap();
    run_child("blob_unsynced", dir.path(), 0);

    let line = Line::open(dir.path()).await.unwrap();
    assert_eq!(
        query_ints(&line, "SELECT COUNT(*) FROM entries").await[0][0],
        0
    );
    assert_every_referenced_blob_exists(&line, dir.path()).await;
    assert!(!dir.path().join("tool-output/9/1/output.json").exists());
    assert!(
        dir.path()
            .join("tool-output/9/1/output.json.partial")
            .exists()
    );
}
