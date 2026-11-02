use std::path::Path;
use std::process::Command;

use komo_kernel::protocol::http::{DiffStatus, FileChange};
use komo_kernel::types::ids::{RunId, SessionId};

use super::ChangeRecorder;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(dir.path().join("dirty.txt"), "x\n").unwrap();
    std::fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init"]);
    dir
}

fn ids() -> (SessionId, RunId) {
    (SessionId::from_raw("s1"), RunId::from_raw("7"))
}

#[tokio::test]
async fn a_run_in_a_repo_lists_shell_and_tool_changes_but_not_earlier_dirty_work() {
    let work = repo();
    let root = tempfile::tempdir().unwrap();
    let recorder = ChangeRecorder::new(root.path().join("changes"));
    let (session, run) = ids();
    // 这一轮开始之前就没提交的改动不算这一轮的
    std::fs::write(work.path().join("dirty.txt"), "x\ny\n").unwrap();

    assert!(recorder.begin(&session, &run, work.path()).await);
    assert!(
        !recorder.begin(&session, &run, work.path()).await,
        "同一轮只开一次"
    );
    recorder
        .capture(&session, &run, &work.path().join("a.txt"))
        .await;
    std::fs::write(work.path().join("a.txt"), "one\n2\nthree\nfour\n").unwrap();
    // shell 新建的、没被忽略的文件
    std::fs::write(work.path().join("new.md"), "hi\n").unwrap();
    // 被忽略的目录里，经文件工具写的
    std::fs::create_dir(work.path().join("build")).unwrap();
    recorder
        .capture(&session, &run, &work.path().join("build/out.txt"))
        .await;
    std::fs::write(work.path().join("build/out.txt"), "made\n").unwrap();
    recorder.finish(&run).await;

    let changes = recorder.run(&run).await.expect("记下了");
    let listed: Vec<_> = changes
        .files
        .iter()
        .map(|f| (f.display.as_str(), f.change, f.added, f.deleted))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("a.txt", FileChange::Modified, Some(2), Some(1)),
            ("build/out.txt", FileChange::Added, Some(1), Some(0)),
            ("new.md", FileChange::Added, Some(1), Some(0)),
        ]
    );
    assert_eq!(changes.total, 3);

    let diff = recorder.diff(&run, 0).await.expect("有对比");
    assert_eq!(diff.status, DiffStatus::Text);
    let patch = diff.patch.unwrap();
    assert!(patch.contains("-two\n+2\n"), "{patch}");
    let ignored = recorder.diff(&run, 1).await.expect("副本也有对比");
    assert!(ignored.patch.unwrap().contains("+made"));

    let mut status = Command::new("git");
    status
        .current_dir(work.path())
        .args(["status", "--porcelain"]);
    let out = String::from_utf8(status.output().unwrap().stdout).unwrap();
    assert!(out.contains(" M dirty.txt"), "仓库的 index 没被动过：{out}");
}

#[tokio::test]
async fn outside_a_repo_only_file_tool_edits_are_listed() {
    let work = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let recorder = ChangeRecorder::new(root.path().join("changes"));
    let (session, run) = ids();
    std::fs::write(work.path().join("keep.txt"), "a\n").unwrap();
    std::fs::write(work.path().join("shell.txt"), "a\n").unwrap();

    recorder.begin(&session, &run, work.path()).await;
    recorder
        .capture(&session, &run, &work.path().join("keep.txt"))
        .await;
    std::fs::write(work.path().join("keep.txt"), "a\nb\n").unwrap();
    std::fs::write(work.path().join("shell.txt"), "changed\n").unwrap();
    // 写了又写回原样：不列
    recorder
        .capture(&session, &run, &work.path().join("same.txt"))
        .await;
    recorder.finish(&run).await;

    let changes = recorder.run(&run).await.expect("记下了");
    assert_eq!(changes.files.len(), 1);
    assert_eq!(changes.files[0].display, "keep.txt");
    assert_eq!(changes.files[0].added, Some(1));
}

#[tokio::test]
async fn asking_for_an_open_run_waits_until_it_finishes() {
    let work = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let recorder = std::sync::Arc::new(ChangeRecorder::new(root.path().join("changes")));
    let (session, run) = ids();
    recorder.begin(&session, &run, work.path()).await;
    recorder
        .capture(&session, &run, &work.path().join("n.txt"))
        .await;
    std::fs::write(work.path().join("n.txt"), "n\n").unwrap();

    let waiting = {
        let recorder = std::sync::Arc::clone(&recorder);
        let run = run.clone();
        tokio::spawn(async move { recorder.run(&run).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    recorder.finish(&run).await;
    let changes = waiting.await.unwrap().expect("收尾之后答出来");
    assert_eq!(changes.files[0].change, FileChange::Added);
    assert_eq!(recorder.session(&session).await.len(), 1);
}

#[tokio::test]
async fn a_run_that_changed_nothing_has_no_record() {
    let work = repo();
    let root = tempfile::tempdir().unwrap();
    let recorder = ChangeRecorder::new(root.path().join("changes"));
    let (session, run) = ids();
    recorder.begin(&session, &run, work.path()).await;
    recorder.finish(&run).await;
    assert!(recorder.run(&run).await.is_none());
    assert!(recorder.session(&session).await.is_empty());
    assert!(
        recorder.run(&RunId::from_raw("8")).await.is_none(),
        "没记过"
    );
}
