//! 记录器用到的 git 命令。快照走私有 index 加会话自己的临时对象目录，仓库的对象库只读挂成
//! alternate：仓库的 index、对象、工作树和 ref 都不动，用户此前没提交的改动也不会算进这一轮。
//!
//! 每条命令都清掉会改变 git 行为的环境变量（`GIT_DIR` 之类）、不读环境里注入的配置、不弹
//! 凭据提示、不抢可选的锁，并且有时限、有输出上限。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// 单条命令的时限。
const TIMEOUT: Duration = Duration::from_secs(30);
/// 单条命令保留的输出字节数；超了这一轮就不记。
const OUTPUT_MAX: usize = 8 * 1024 * 1024;

/// 会改变 git 找仓库、找对象、读配置方式的环境变量：Gateway 自己的环境里有就是意外。
const SCRUB: [&str; 9] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_EXTERNAL_DIFF",
];

/// 一个工作目录所在的仓库。
#[derive(Debug, Clone)]
pub struct Repo {
    /// 工作树根。
    pub top: PathBuf,
    /// 这个工作树自己的 git 目录（index 在这里；linked worktree 与主仓库不同）。
    pub git_dir: PathBuf,
    /// 对象库所在的公共 git 目录。
    pub common_dir: PathBuf,
}

/// 快照用的私有存储：会话自己的临时目录。
#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    fn objects(&self) -> PathBuf {
        self.dir.join("objects")
    }
}

#[derive(Debug)]
pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("起不来 git：{0}")]
    Spawn(std::io::Error),
    #[error("git {0} 超时")]
    Timeout(String),
    #[error("git {0} 的输出超过 {OUTPUT_MAX} 字节")]
    TooLarge(String),
    #[error("git {what} 退出码 {code}")]
    Failed { what: String, code: i32 },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

fn command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir);
    for name in SCRUB {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

/// 跑一条命令，返回退出码与 stdout。`ok` 里的退出码之外都算失败。
async fn run(mut command: Command, args: &[&str], ok: &[i32]) -> Result<Output, GitError> {
    let what = args.first().copied().unwrap_or("").to_owned();
    command.args(args);
    let mut child = command.spawn().map_err(GitError::Spawn)?;
    let mut stdout = child.stdout.take().expect("stdout 是管道");
    let work = async {
        let mut buffer = Vec::new();
        let read = (&mut stdout)
            .take(OUTPUT_MAX as u64 + 1)
            .read_to_end(&mut buffer)
            .await?;
        if read > OUTPUT_MAX {
            return Err(GitError::TooLarge(what.clone()));
        }
        let status = child.wait().await?;
        Ok((status, buffer))
    };
    let (status, stdout) = tokio::time::timeout(TIMEOUT, work)
        .await
        .map_err(|_| GitError::Timeout(what.clone()))??;
    let code = status.code().unwrap_or(-1);
    if !ok.contains(&code) {
        return Err(GitError::Failed { what, code });
    }
    Ok(Output { code, stdout })
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end().to_owned()
}

/// `cwd` 所在的仓库。不在仓库里、没有 git（macOS 上只有开发者工具桩程序也算）都是 `None`。
pub async fn locate(cwd: &Path) -> Option<Repo> {
    let output = run(
        command(cwd),
        &[
            "rev-parse",
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-common-dir",
        ],
        &[0],
    )
    .await
    .ok()?;
    let out = text(&output.stdout);
    let mut lines = out.lines();
    let top = PathBuf::from(lines.next()?);
    let git_dir = PathBuf::from(lines.next()?);
    // 相对路径是相对 `cwd` 说的
    let common_dir = cwd.join(lines.next()?);
    Some(Repo {
        top,
        git_dir,
        common_dir,
    })
}

/// 给工作树拍一次快照，返回树 ID。index 以仓库的为种子（没改过的文件不重新算哈希），新对象
/// 只落进 `store`。读不出来的文件跳过——git 以退出码 1 报告，照样认。
pub async fn snapshot(repo: &Repo, store: &Store, name: &str) -> Result<String, GitError> {
    tokio::fs::create_dir_all(store.objects()).await?;
    let index = store.dir.join(format!("index-{name}"));
    match tokio::fs::copy(repo.git_dir.join("index"), &index).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let _ = tokio::fs::remove_file(&index).await;
        }
        Err(error) => return Err(error.into()),
    }
    let scoped = || {
        let mut command = command(&repo.top);
        command
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_OBJECT_DIRECTORY", store.objects())
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                repo.common_dir.join("objects"),
            );
        command
    };
    run(scoped(), &["add", "--all", "--ignore-errors"], &[0, 1]).await?;
    let tree = run(scoped(), &["write-tree"], &[0]).await?;
    let _ = tokio::fs::remove_file(&index).await;
    Ok(text(&tree.stdout))
}

fn in_store(repo: &Repo, store: &Store) -> Command {
    let mut command = command(&repo.top);
    command.env("GIT_OBJECT_DIRECTORY", store.objects()).env(
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        repo.common_dir.join("objects"),
    );
    command
}

/// 两棵树之间改了的路径（仓库根下的相对路径）：状态与增删行数（二进制为 `None`）。
pub async fn tree_changes(
    repo: &Repo,
    store: &Store,
    from: &str,
    to: &str,
) -> Result<Vec<TreeChange>, GitError> {
    let status = run(
        in_store(repo, store),
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-status",
            from,
            to,
        ],
        &[0],
    )
    .await?;
    let numstat = run(
        in_store(repo, store),
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--numstat",
            from,
            to,
        ],
        &[0],
    )
    .await?;
    let mut changes: Vec<TreeChange> = parse_name_status(&status.stdout);
    for (path, counts) in parse_numstat(&numstat.stdout) {
        if let Some(change) = changes.iter_mut().find(|c| c.path == path) {
            change.counts = counts;
        }
    }
    Ok(changes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeChange {
    pub path: String,
    /// `A` / `M` / `D` / `T`…（`--name-status` 的那个字母）。
    pub status: char,
    pub counts: Option<(u64, u64)>,
}

fn parse_name_status(bytes: &[u8]) -> Vec<TreeChange> {
    let mut fields = bytes
        .split(|b| *b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned());
    let mut out = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let Some(letter) = status.chars().next() else {
            break;
        };
        out.push(TreeChange {
            path,
            status: letter,
            counts: None,
        });
    }
    out
}

/// `--numstat -z`（没开改名检测）：每条是 `增\t删\t路径\0`；二进制两个数都是 `-`。
fn parse_numstat(bytes: &[u8]) -> Vec<(String, Option<(u64, u64)>)> {
    bytes
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .filter_map(|field| {
            let line = String::from_utf8_lossy(field);
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?;
            let deleted = parts.next()?;
            let path = parts.next()?.to_owned();
            let counts = added.parse().ok().zip(deleted.parse().ok());
            Some((path, counts))
        })
        .collect()
}

/// 两棵树之间一个路径的 unified diff。
pub async fn tree_patch(
    repo: &Repo,
    store: &Store,
    from: &str,
    to: &str,
    path: &str,
) -> Result<Vec<u8>, GitError> {
    let output = run(
        in_store(repo, store),
        &[
            "diff-tree",
            "-p",
            "-U3",
            "--no-renames",
            "--no-ext-diff",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            from,
            to,
            "--",
            path,
        ],
        &[0],
    )
    .await?;
    Ok(output.stdout)
}

/// `path`（仓库里的）有没有被忽略：被忽略的文件快照里没有，要靠整文件副本。
pub async fn ignored(repo: &Repo, path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return false;
    };
    run(
        command(&repo.top),
        &["check-ignore", "-q", "--no-index", "--", path],
        &[0, 1],
    )
    .await
    .map(|output| output.code == 0)
    .unwrap_or(false)
}

/// 两份副本（`None` = 那一侧没有这个文件）之间的增删行数；二进制为 `None`。`dir` 只是
/// git 的工作目录，`--no-index` 不看仓库。
pub async fn file_counts(
    dir: &Path,
    before: Option<&Path>,
    after: Option<&Path>,
) -> Result<Option<(u64, u64)>, GitError> {
    let (a, b) = sides(before, after);
    let output = run(
        command(dir),
        &["diff", "--no-index", "--numstat", "-z", "--", &a, &b],
        &[0, 1],
    )
    .await?;
    // `--no-index` 两侧都有时也走改名检测，路径会多出来；数字在前两列
    let line = String::from_utf8_lossy(&output.stdout);
    let mut parts = line.splitn(3, '\t');
    let added = parts.next().unwrap_or("");
    let deleted = parts.next().unwrap_or("");
    Ok(added.parse().ok().zip(deleted.parse().ok()))
}

/// 两份副本之间的 unified diff。文件头里是副本的路径，读的人只看 `@@` 之后的部分。
pub async fn file_patch(
    dir: &Path,
    before: Option<&Path>,
    after: Option<&Path>,
) -> Result<Vec<u8>, GitError> {
    let (a, b) = sides(before, after);
    let output = run(
        command(dir),
        &[
            "diff",
            "--no-index",
            "-U3",
            "--no-color",
            "--no-ext-diff",
            "--",
            &a,
            &b,
        ],
        &[0, 1],
    )
    .await?;
    Ok(output.stdout)
}

fn sides(before: Option<&Path>, after: Option<&Path>) -> (String, String) {
    let side = |p: Option<&Path>| {
        p.map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/dev/null".to_owned())
    };
    (side(before), side(after))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_reads_counts_and_marks_binary() {
        let raw = b"3\t1\tsrc/a.rs\0-\t-\tlogo.png\0";
        assert_eq!(
            parse_numstat(raw),
            vec![
                ("src/a.rs".to_owned(), Some((3, 1))),
                ("logo.png".to_owned(), None)
            ]
        );
    }

    #[test]
    fn name_status_pairs_letters_with_paths() {
        let raw = b"A\0new file.txt\0M\0src/a.rs\0";
        let changes = parse_name_status(raw);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "new file.txt");
        assert_eq!(changes[0].status, 'A');
        assert_eq!(changes[1].status, 'M');
    }
}
