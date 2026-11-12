//! 一轮改了哪些文件（照 DeepSeek Harness 的 `packages/deliverables/workspace-changes`）。
//!
//! - **快照**：一条 Run 第一次执行工具之前，给工作目录所在的 git 工作树拍一次快照；Run 进
//!   终态时再拍一次，两棵树一比就是这一轮改了什么，shell 改的也在里面（[`git`]）。
//! - **整文件副本**：`write` / `edit` 这类计划里带写目标的调用，每轮每个路径第一次改之前把
//!   整个文件复制一份，收尾时再复制一份。快照管不到的文件（被忽略的、仓库外的、没有仓库或
//!   没有 git 时的全部）靠这两份比；只被 shell 改、又不在快照里的文件看不到。
//! - **只活在进程里**：摘要、快照对象和副本都在内存与 `runtime/changes/<session>/` 里，不进
//!   `durable.db`，Gateway 起来时整个目录清掉。重启之后先前的 Run 没有这份记录——给不出对比
//!   的卡片就不给（DeepSeek 同样的取舍）。它不是事实来源，丢了不影响任何别的东西。
//!
//! 每个会话的 git 工作串行（一把锁），所以同一会话里先后两轮的快照不会交错。

mod git;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::protocol::http::{
    ChangedFile, DiffStatus, FileChange, FileDiff, MAX_CHANGED_FILES, RunChanges,
};
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ContentHash;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::plan::{ExecutionPlan, Replay, TargetAccess};
use komo_kernel::types::resource::TargetRef;
use komo_kernel::types::tool::{
    ToolContext, ToolDefinition, ToolError, ToolExposure, ToolNamespace, ToolOutput,
};
use tokio::sync::Notify;

use git::{Repo, Store};

/// 整文件副本与对比读的上限：更大的文件只知道改了，给不出行数和对比。
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 问一条还没收尾的 Run 时最多等这么久。
const WAIT: Duration = Duration::from_secs(20);

/// 一侧的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Side {
    Missing,
    Copy(PathBuf),
    Oversized,
}

/// 文件第一次被写之前查到的覆盖情况。
#[derive(Debug, Clone)]
enum Capture {
    /// 在快照里，行数与对比都走 git。
    Covered,
    Before(Side),
}

#[derive(Debug, Clone)]
enum Source {
    Tree {
        repo: Repo,
        from: String,
        to: String,
        path: String,
    },
    Copies {
        before: Side,
        after: Side,
    },
}

#[derive(Debug)]
struct Recorded {
    changes: RunChanges,
    sources: Vec<Source>,
}

#[derive(Debug)]
struct RunRecord {
    cwd: PathBuf,
    repo: Option<Repo>,
    base: Option<String>,
    captures: BTreeMap<PathBuf, Capture>,
    result: Option<Recorded>,
}

#[derive(Default)]
struct Inner {
    repos: HashMap<PathBuf, Option<Repo>>,
    runs: Vec<(RunId, RunRecord)>,
}

impl Inner {
    fn run(&mut self, run: &RunId) -> Option<&mut RunRecord> {
        self.runs
            .iter_mut()
            .find(|(id, _)| id == run)
            .map(|(_, record)| record)
    }
}

struct SessionLog {
    store: Store,
    inner: tokio::sync::Mutex<Inner>,
    settled: Notify,
}

/// 记录器：`GatewayState::changes`。
pub struct ChangeRecorder {
    root: PathBuf,
    sessions: Mutex<HashMap<SessionId, Arc<SessionLog>>>,
    owners: Mutex<HashMap<RunId, SessionId>>,
}

impl ChangeRecorder {
    /// `root` = `<home>/runtime/changes`。上一次进程留下的整个清掉：那些记录已经没人认了。
    pub fn new(root: PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&root);
        ChangeRecorder {
            root,
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
        }
    }

    fn log(&self, session: &SessionId) -> Arc<SessionLog> {
        let mut sessions = self.sessions.lock().expect("改动记录表");
        Arc::clone(sessions.entry(session.clone()).or_insert_with(|| {
            Arc::new(SessionLog {
                store: Store {
                    dir: self.root.join(session.as_str()),
                },
                inner: tokio::sync::Mutex::new(Inner::default()),
                settled: Notify::new(),
            })
        }))
    }

    fn existing(&self, run: &RunId) -> Option<Arc<SessionLog>> {
        let session = self.owners.lock().expect("改动记录表").get(run).cloned()?;
        self.sessions
            .lock()
            .expect("改动记录表")
            .get(&session)
            .cloned()
    }

    /// 这条 Run 第一次执行工具：拍基线快照。已经开始过就什么都不做；返回是不是新开的。
    pub async fn begin(&self, session: &SessionId, run: &RunId, cwd: &Path) -> bool {
        let log = self.log(session);
        let mut inner = log.inner.lock().await;
        if inner.run(run).is_some() {
            return false;
        }
        let cwd = canonical(cwd);
        let repo = match inner.repos.get(&cwd) {
            Some(repo) => repo.clone(),
            None => {
                let repo = git::locate(&cwd).await;
                if repo.is_none() {
                    tracing::debug!(cwd = %cwd.display(), "不在 git 仓库里（或没有 git）：只记文件工具的改动");
                }
                inner.repos.insert(cwd.clone(), repo.clone());
                repo
            }
        };
        let base = match &repo {
            Some(repo) => match git::snapshot(repo, &log.store, &format!("{run}-base")).await {
                Ok(tree) => Some(tree),
                Err(error) => {
                    tracing::warn!(%error, %run, "开始时的快照没拍成：这一轮只记文件工具的改动");
                    None
                }
            },
            None => None,
        };
        inner.runs.push((
            run.clone(),
            RunRecord {
                cwd,
                repo,
                base,
                captures: BTreeMap::new(),
                result: None,
            },
        ));
        self.owners
            .lock()
            .expect("改动记录表")
            .insert(run.clone(), session.clone());
        true
    }

    /// 一个写目标第一次要被改：快照覆盖得到就记一笔，否则存一份改前的整文件。
    pub async fn capture(&self, session: &SessionId, run: &RunId, path: &Path) {
        let log = self.log(session);
        let mut inner = log.inner.lock().await;
        let path = canonical(path);
        let Some(record) = inner.run(run) else { return };
        if record.captures.contains_key(&path) {
            return;
        }
        let covered = match (&record.repo, &record.base) {
            (Some(repo), Some(_)) => {
                path.starts_with(&repo.top) && !git::ignored(repo, &path).await
            }
            _ => false,
        };
        let capture = if covered {
            Capture::Covered
        } else {
            Capture::Before(copy(&log.store, &path).await)
        };
        record.captures.insert(path, capture);
    }

    /// Run 进了终态：再拍一次、比出这一轮的改动。已经收过尾的不再收。
    pub async fn finish(&self, run: &RunId) {
        let Some(log) = self.existing(run) else {
            return;
        };
        {
            let mut inner = log.inner.lock().await;
            let Some(record) = inner.run(run) else { return };
            if record.result.is_none() {
                let recorded = summarize(&log.store, run, record).await;
                record.result = Some(recorded);
            }
        }
        log.settled.notify_waiters();
    }

    /// 还开着（开始过、没收尾）的 Run。
    pub async fn open_runs(&self, session: &SessionId) -> Vec<RunId> {
        let log = self.log(session);
        let inner = log.inner.lock().await;
        inner
            .runs
            .iter()
            .filter(|(_, record)| record.result.is_none())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 一个会话里收过尾、改了文件的各条 Run，按先后。
    pub async fn session(&self, session: &SessionId) -> Vec<RunChanges> {
        let log = self.log(session);
        let inner = log.inner.lock().await;
        inner
            .runs
            .iter()
            .filter_map(|(_, record)| record.result.as_ref())
            .filter(|recorded| recorded.changes.total > 0)
            .map(|recorded| recorded.changes.clone())
            .collect()
    }

    /// 一条 Run 的改动。还没收尾就等它（至多 [`WAIT`]）；没记过、或没改文件是 `None`。
    pub async fn run(&self, run: &RunId) -> Option<RunChanges> {
        let log = self.existing(run)?;
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let settled = log.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            {
                let mut inner = log.inner.lock().await;
                let record = inner.run(run)?;
                if let Some(recorded) = &record.result {
                    return (recorded.changes.total > 0).then(|| recorded.changes.clone());
                }
            }
            if tokio::time::timeout_at(deadline, settled).await.is_err() {
                return None;
            }
        }
    }

    /// 一条 Run 里第 `index` 个文件的对比。
    pub async fn diff(&self, run: &RunId, index: usize) -> Option<FileDiff> {
        let log = self.existing(run)?;
        let (file, source) = {
            let mut inner = log.inner.lock().await;
            let recorded = inner.run(run)?.result.as_ref()?;
            (
                recorded.changes.files.get(index)?.clone(),
                recorded.sources.get(index)?.clone(),
            )
        };
        let patch = match &source {
            Source::Tree {
                repo,
                from,
                to,
                path,
            } => git::tree_patch(repo, &log.store, from, to, path).await,
            Source::Copies { before, after } => {
                if [before, after].contains(&&Side::Oversized) {
                    return Some(diff_of(&file, DiffStatus::Oversized, None));
                }
                git::file_patch(&log.store.dir, side_path(before), side_path(after)).await
            }
        };
        let patch = match patch {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                tracing::warn!(%error, %run, index, "对比没算出来");
                return None;
            }
        };
        let status = if patch.trim().is_empty() {
            DiffStatus::Identical
        } else if !patch.contains("\n@@") && patch.contains("Binary files") {
            DiffStatus::Binary
        } else {
            DiffStatus::Text
        };
        let patch = (status == DiffStatus::Text).then_some(patch);
        Some(diff_of(&file, status, patch))
    }
}

fn diff_of(file: &ChangedFile, status: DiffStatus, patch: Option<String>) -> FileDiff {
    FileDiff {
        path: file.path.clone(),
        display: file.display.clone(),
        status,
        patch,
    }
}

fn side_path(side: &Side) -> Option<&Path> {
    match side {
        Side::Copy(path) => Some(path),
        _ => None,
    }
}

/// 文件可能还不存在：解析父目录（macOS 上 `/tmp` 是 `/private/tmp`，git 报的是后者）。
fn canonical(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

/// 把文件此刻的内容存进会话的临时目录，按内容哈希命名（相同内容只存一份）。
async fn copy(store: &Store, path: &Path) -> Side {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Side::Missing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Side::Missing,
        Err(_) => return Side::Oversized,
    };
    if metadata.len() > MAX_FILE_BYTES {
        return Side::Oversized;
    }
    let Ok(bytes) = tokio::fs::read(path).await else {
        return Side::Oversized;
    };
    let target = store
        .dir
        .join("copies")
        .join(ContentHash::of_bytes(&bytes).as_str());
    if tokio::fs::metadata(&target).await.is_err() {
        let written = async {
            tokio::fs::create_dir_all(store.dir.join("copies")).await?;
            tokio::fs::write(&target, &bytes).await
        };
        if let Err(error) = written.await {
            tracing::warn!(%error, path = %path.display(), "整文件副本没存下来");
            return Side::Oversized;
        }
    }
    Side::Copy(target)
}

async fn summarize(store: &Store, run: &RunId, record: &RunRecord) -> Recorded {
    let mut entries: BTreeMap<PathBuf, (ChangedFile, Source)> = BTreeMap::new();
    if let (Some(repo), Some(base)) = (&record.repo, &record.base) {
        let tree = async {
            let end = git::snapshot(repo, store, &format!("{run}-end")).await?;
            let changes = git::tree_changes(repo, store, base, &end).await?;
            Ok::<_, git::GitError>((end, changes))
        };
        match tree.await {
            Ok((end, changes)) => {
                for change in changes {
                    let abs = repo.top.join(&change.path);
                    let file = ChangedFile {
                        path: String::new(),
                        display: String::new(),
                        change: match change.status {
                            'A' => FileChange::Added,
                            'D' => FileChange::Deleted,
                            _ => FileChange::Modified,
                        },
                        added: change.counts.map(|c| c.0),
                        deleted: change.counts.map(|c| c.1),
                        binary: change.counts.is_none(),
                        oversized: false,
                    };
                    let source = Source::Tree {
                        repo: repo.clone(),
                        from: base.clone(),
                        to: end.clone(),
                        path: change.path,
                    };
                    entries.insert(abs, (file, source));
                }
            }
            Err(error) => {
                tracing::warn!(%error, %run, "收尾时的快照没拍成：这一轮只列文件工具的改动");
            }
        }
    }
    for (path, capture) in &record.captures {
        let Capture::Before(before) = capture else {
            continue;
        };
        let after = copy(store, path).await;
        if *before == after {
            continue;
        }
        let oversized = [before, &after].contains(&&Side::Oversized);
        let counts = if oversized {
            Ok(None)
        } else {
            git::file_counts(&store.dir, side_path(before), side_path(&after)).await
        };
        let file = ChangedFile {
            path: String::new(),
            display: String::new(),
            change: match (before, &after) {
                (Side::Missing, _) => FileChange::Added,
                (_, Side::Missing) => FileChange::Deleted,
                _ => FileChange::Modified,
            },
            added: counts.as_ref().ok().copied().flatten().map(|c| c.0),
            deleted: counts.as_ref().ok().copied().flatten().map(|c| c.1),
            binary: matches!(counts, Ok(None)) && !oversized,
            oversized,
        };
        let source = Source::Copies {
            before: before.clone(),
            after,
        };
        entries.insert(path.clone(), (file, source));
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut listed: Vec<(ChangedFile, Source)> = entries
        .into_iter()
        .map(|(abs, (mut file, source))| {
            let (path, display) = labels(&abs, &record.cwd, record.repo.as_ref(), home.as_deref());
            file.path = path;
            file.display = display;
            (file, source)
        })
        .collect();
    listed.sort_by(|a, b| a.0.display.cmp(&b.0.display));
    let total = listed.len();
    listed.truncate(MAX_CHANGED_FILES);
    let (files, sources) = listed.into_iter().unzip();
    Recorded {
        changes: RunChanges {
            run: run.clone(),
            cwd: record.cwd.to_string_lossy().into_owned(),
            files,
            total,
        },
        sources,
    }
}

/// `path`：在工作目录里是相对路径，否则绝对路径。`display`：相对路径；仓库里、工作目录之上
/// 的用 `../`；家目录下的用 `~/`；其余绝对路径。
fn labels(abs: &Path, cwd: &Path, repo: Option<&Repo>, home: Option<&Path>) -> (String, String) {
    let text = |p: &Path| p.to_string_lossy().into_owned();
    if let Ok(rel) = abs.strip_prefix(cwd) {
        return (text(rel), text(rel));
    }
    let display = match repo.filter(|repo| abs.starts_with(&repo.top)) {
        Some(_) => {
            let common = cwd
                .ancestors()
                .find(|dir| abs.starts_with(dir))
                .unwrap_or(Path::new("/"));
            let ups = cwd
                .strip_prefix(common)
                .map(|rest| rest.components().count())
                .unwrap_or(0);
            let rest = abs.strip_prefix(common).unwrap_or(abs);
            format!("{}{}", "../".repeat(ups), text(rest))
        }
        None => match home.and_then(|home| abs.strip_prefix(home).ok()) {
            Some(rest) => format!("~/{}", text(rest)),
            None => text(abs),
        },
    };
    (text(abs), display)
}

// ---------------------------------------------------------------- 工具外面包一层

/// 包在每个工具外面：执行之前先让记录器开这一轮（拍基线），写目标先存一份改前的整文件，
/// 再交给原工具。记录器出什么事都不影响工具本身。
pub struct Recording {
    inner: Arc<dyn Tool>,
    state: Weak<crate::service::state::GatewayState>,
}

impl Recording {
    pub fn wrap(
        inner: Arc<dyn Tool>,
        state: &Arc<crate::service::state::GatewayState>,
    ) -> Arc<dyn Tool> {
        Arc::new(Recording {
            inner,
            state: Arc::downgrade(state),
        })
    }
}

#[async_trait]
impl Tool for Recording {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        self.inner.prepare(args, ctx).await
    }

    async fn execute(
        &self,
        plan: ExecutionPlan,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        // `!` 命令没有输入 Run，不算哪一轮的
        if let (Some(run), Some(state)) = (&ctx.input_run, self.state.upgrade()) {
            if state.changes.begin(&ctx.session, run, &ctx.cwd).await {
                watch(&state, &ctx.session);
            }
            for target in plan
                .targets
                .iter()
                .filter(|t| t.access == TargetAccess::Write)
            {
                let path = match &target.target {
                    TargetRef::LocalPath { path } => Some(path),
                    TargetRef::Resource { resolved, .. } => resolved.as_ref(),
                };
                if let Some(path) = path {
                    state.changes.capture(&ctx.session, run, path).await;
                }
            }
        }
        self.inner.execute(plan, ctx, sink).await
    }

    fn replay(&self) -> Replay {
        self.inner.replay()
    }

    fn parallel_for(&self, args: &serde_json::Value) -> bool {
        self.inner.parallel_for(args)
    }

    fn exposure(&self) -> ToolExposure {
        self.inner.exposure()
    }

    fn namespace(&self) -> Option<ToolNamespace> {
        self.inner.namespace()
    }

    fn outward(&self) -> bool {
        self.inner.outward()
    }
}

// ---------------------------------------------------------------- 收尾

/// 一个会话一个看客（第一次有 Run 开始记录时挂上）：Run 的终态帧来了就收尾。落后丢了帧就把
/// 还开着的、已经结算的 Run 都收一遍。
fn watch(state: &Arc<crate::service::state::GatewayState>, session: &SessionId) {
    use komo_kernel::durable::legacy_ids;
    use komo_kernel::events::EventPayload;
    use komo_kernel::protocol::sse::SseEvent;
    use tokio::sync::broadcast::error::RecvError;

    let marker = RunId::from_raw(format!("changes-watch:{session}"));
    if !state.start_watching(&marker) {
        return;
    }
    let mut events = state.hub.subscribe(session);
    let weak = Arc::downgrade(state);
    let session = session.clone();
    tokio::spawn(async move {
        loop {
            let received = events.recv().await;
            let Some(state) = weak.upgrade() else { break };
            match received {
                Ok(frame) => {
                    let SseEvent::Event(event) = &frame.event else {
                        continue;
                    };
                    let terminal = matches!(
                        event.payload,
                        EventPayload::RunCompleted(_)
                            | EventPayload::RunFailed(_)
                            | EventPayload::RunCancelled(_)
                    );
                    if let (true, Some(run)) = (terminal, &event.run) {
                        state.changes.finish(run).await;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let Ok(core) = state.core() else { continue };
                    for run in state.changes.open_runs(&session).await {
                        let Some(first) = legacy_ids::submission_of(&run) else {
                            continue;
                        };
                        if core
                            .submission_state(first)
                            .await
                            .is_ok_and(|run_state| run_state.is_terminal())
                        {
                            state.changes.finish(&run).await;
                        }
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
        if let Some(state) = weak.upgrade() {
            state.stop_watching(&RunId::from_raw(format!("changes-watch:{session}")));
        }
    });
}
