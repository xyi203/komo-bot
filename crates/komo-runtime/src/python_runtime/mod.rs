//! 受管理的 Python 解释器（§5.1）。
//!
//! 每次调用起一个新进程，不依赖跨调用的内存变量；stdout / stderr 流式写进
//! [`OutputWriter`]，**结构化结果走另一条路**（一个结果文件），所以脚本打印的 JSON
//! 永远伪造不了执行结果。取消时终止进程组并等待回收。
//!
//! [`PythonHost::env_version`] = **解释器版本 + 依赖清单哈希**。它进
//! [`ExecutionPlan::versions`](komo_kernel::types::plan::ExecutionPlan)，所以依赖一
//! 升级，账本里看得出新计划跑在另一个环境上（§7.2）。环境本身的变更（装依赖、切
//! 版本）是 `Operation::PythonEnvChange`，**不在这个模块里做**。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, PythonHost};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::plan::EnvVersion;
use komo_kernel::types::refs::{ContentRef, ToolResultStatus};
use komo_kernel::types::tool::{CancelToken, PyError, PythonJob, PythonResult};

use crate::tools::process::{ChildRegistration, ChildSpec, ProcessError, run_child};

/// 解释器驱动。脚本输出与控制协议分开就靠它。
const DRIVER: &str = include_str!("driver.py");

/// 默认最多往输出存储里写多少字节。
/// 默认保存完整输出；模型视图的限制由 kernel 投影负责。
pub const DEFAULT_OUTPUT_LIMIT: u64 = u64::MAX;

const INHERITED: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TZ", "TMPDIR"];

/// 受管理环境的位置。
#[derive(Debug, Clone)]
pub struct PythonEnvConfig {
    /// 虚拟环境目录（`~/.komo/python-envs/<版本>`）。
    pub env_root: PathBuf,
    /// 解释器；`None` = `env_root/bin/python3`。
    pub interpreter: Option<PathBuf>,
    /// 依赖锁定清单。它的哈希是 `env_version` 的另一半（§5.1「依赖按锁定清单安装」）。
    pub requirements: Option<PathBuf>,
    /// 子进程的工作目录。
    pub cwd: PathBuf,
    pub output_limit: u64,
}

impl PythonEnvConfig {
    pub fn new(env_root: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            env_root: env_root.into(),
            interpreter: None,
            requirements: None,
            cwd: cwd.into(),
            output_limit: DEFAULT_OUTPUT_LIMIT,
        }
    }

    pub fn interpreter_path(&self) -> PathBuf {
        self.interpreter
            .clone()
            .unwrap_or_else(|| self.env_root.join("bin/python3"))
    }
}

/// 受管理环境还没建时退回哪个系统解释器：`PATH` 里版本最高的 `python3.N`，一个都没有才是
/// 裸的 `python3`。
///
/// 裸 `python3` 在 macOS 上常常是 `/usr/bin/python3`（3.9），哪怕 Homebrew / uv 装着 3.12、3.14——
/// 它们只提供带版本号的名字。实测会话里因此缺 `tomllib`、`fromisoformat` 读不了 komo 自己的时间戳，
/// 白费好几轮。
///
/// 从 3.20 往下找到 3.10；每个版本按 `PATH` 顺序取第一个可执行文件。
pub fn system_interpreter(path_var: Option<&std::ffi::OsStr>) -> PathBuf {
    let dirs: Vec<PathBuf> = path_var
        .map(|value| std::env::split_paths(value).collect())
        .unwrap_or_default();
    for minor in (10..=20).rev() {
        let name = format!("python3.{minor}");
        for dir in &dirs {
            let candidate = dir.join(&name);
            if is_executable(&candidate) {
                return candidate;
            }
        }
    }
    PathBuf::from("python3")
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// [`PythonHost`] 的生产实现。
pub struct PythonRuntime {
    config: PythonEnvConfig,
    env_version: EnvVersion,
    /// 在册登记（§8.7）。
    register: Option<ChildRegistration>,
}

impl std::fmt::Debug for PythonRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonRuntime")
            .field("interpreter", &self.config.interpreter_path())
            .field("env_version", &self.env_version)
            .finish_non_exhaustive()
    }
}

impl PythonRuntime {
    /// 环境版本已知时直接构造（测试、或由配置固定下来的版本）。
    pub fn new(config: PythonEnvConfig, env_version: EnvVersion) -> Self {
        Self {
            config,
            env_version,
            register: None,
        }
    }

    /// 把解释器进程登记在册，恢复扫描才核实得了它（§8.7）。
    pub fn registered(mut self, registration: ChildRegistration) -> Self {
        self.register = Some(registration);
        self
    }

    /// 探一次解释器，算出这个环境的版本。
    ///
    /// 版本在**构造时**定下来，之后不变：`env_version()` 是同步无错的，而且一次 Run
    /// 里的每份计划都要绑定同一个值——中途重算会让同一次执行的前后两个调用绑上不同
    /// 的环境。
    pub async fn probe(config: PythonEnvConfig) -> Result<Self, PyError> {
        let interpreter = config.interpreter_path();
        let output = tokio::process::Command::new(&interpreter)
            .arg("-c")
            .arg("import sys; print('.'.join(map(str, sys.version_info[:3])))")
            .env_clear()
            .envs(base_env(&config))
            .output()
            .await
            .map_err(|error| PyError::Spawn(format!("{}: {error}", interpreter.display())))?;
        if !output.status.success() {
            return Err(PyError::Spawn(format!(
                "{} 退出码 {:?}：{}",
                interpreter.display(),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Self::new(
            config.clone(),
            env_version_of(&version, config.requirements.as_deref()),
        ))
    }

    pub fn config(&self) -> &PythonEnvConfig {
        &self.config
    }
}

/// 解释器版本 + 依赖清单哈希。清单读不到就明写 `no-lock`——不假装锁定过。
pub fn env_version_of(interpreter_version: &str, requirements: Option<&Path>) -> EnvVersion {
    let lock = match requirements.map(std::fs::read) {
        Some(Ok(bytes)) => {
            let hash = ContentHash::of_bytes(&bytes);
            hash.as_str()[..12].to_string()
        }
        Some(Err(_)) | None => "no-lock".to_string(),
    };
    EnvVersion(format!("py-{interpreter_version}-{lock}"))
}

#[async_trait]
impl PythonHost for PythonRuntime {
    /// §4.7：把**产出目录**交给这次调用。
    ///
    /// 目录由调用方从 `ToolContext` 算好（`<会话内容目录>/artifacts/<run>`），这里负责
    /// 建它（父目录不存在一并建）、以 `KOMO_ARTIFACT_DIR` 交给子进程，并在跑完之后登记
    /// **这次新出现**的文件。`artifact://files/<run>/<名字>` 读的就是这个位置——不复制
    /// 第二份，也不把文件挪进 `tool-output`。
    async fn run_with_artifacts(
        &self,
        job: PythonJob,
        artifacts: Option<&Path>,
        timeout: Duration,
        sink: &mut dyn OutputWriter,
        cancel: CancelToken,
    ) -> Result<PythonResult, PyError> {
        self.run_with(job, artifacts, timeout, sink, cancel).await
    }

    fn env_version(&self) -> EnvVersion {
        self.env_version.clone()
    }
}

impl PythonRuntime {
    async fn run_with(
        &self,
        job: PythonJob,
        artifacts: Option<&Path>,
        timeout: Duration,
        sink: &mut dyn OutputWriter,
        cancel: CancelToken,
    ) -> Result<PythonResult, PyError> {
        // 结果文件放在一个只属于这次调用的目录里。不用 `tempfile`——它在这个 crate
        // 里只是 dev-dependency（§13.4 的依赖清单）。
        let workspace = ResultDir::create()?;
        let result_path = workspace.path().join("result.json");

        // 产出目录先建好、再记下**跑之前**已经有什么：同一次 Run 里可能有好几个 python
        // 调用，前面那几次留下的文件不该在这一次的 `artifacts` 里再报一遍。记的是相对
        // 路径，所以脚本把一个旧文件改了名再写回来也算"新出现"。
        let before = match artifacts {
            Some(dir) => {
                std::fs::create_dir_all(dir).map_err(|error| {
                    PyError::Failed(format!("准备产物目录 {} 失败：{error}", dir.display()))
                })?;
                list_files(dir)?
            }
            None => BTreeMap::new(),
        };

        let body = serde_json::to_vec(&job)
            .map_err(|error| PyError::Protocol(format!("作业无法序列化：{error}")))?;

        let mut env = base_env(&self.config);
        env.insert("KOMO_RESULT_PATH".into(), result_path.display().to_string());
        // 产出目录的**名字**留在 runtime 这一层（kernel 只传"产出目录在哪"）。
        if let Some(dir) = artifacts {
            env.insert("KOMO_ARTIFACT_DIR".into(), dir.display().to_string());
        }
        let spec = ChildSpec {
            program: self.config.interpreter_path().display().to_string(),
            // `-B`：不留 __pycache__。
            args: vec!["-B".into(), "-c".into(), DRIVER.into()],
            cwd: self.config.cwd.clone(),
            env,
            stdin: Some(body),
            timeout,
            output_limit: self.config.output_limit,
            register: self.register.clone(),
            label: format!("python · {}", self.config.interpreter_path().display()),
        };

        let outcome = run_child(spec, sink, &cancel)
            .await
            .map_err(|error| match error {
                ProcessError::Spawn(message) => PyError::Spawn(message),
                ProcessError::Io(message) | ProcessError::Sink(message) => PyError::Failed(message),
            })?;

        if outcome.cancelled {
            return Err(PyError::Cancelled);
        }
        if outcome.timed_out {
            return Err(PyError::Timeout {
                after_secs: timeout.as_secs(),
            });
        }

        let Ok(raw) = std::fs::read_to_string(&result_path) else {
            // 没有结果文件：解释器在写下结论之前就没了。**不编造返回值**——退出码和
            // stderr 是这次仅有的证据。
            return Err(PyError::Protocol(format!(
                "解释器没有写下结果（退出码 {:?}）：{}",
                outcome.exit_code,
                outcome.stderr_excerpt.trim()
            )));
        };
        let reported: DriverResult = serde_json::from_str(&raw)
            .map_err(|error| PyError::Protocol(format!("结果文件读不动：{error}")))?;

        // 跑完才登记：失败的结果也可能留下了文件（脚本先写再抛），不替它判断哪一次
        // "算数"——**目录里新出现的东西就是这次产生的**。
        let produced = match artifacts {
            Some(dir) => new_artifacts(dir, &before)?,
            None => Vec::new(),
        };

        Ok(PythonResult {
            status: reported.status,
            result: reported.result,
            error: reported.error,
            artifacts: produced,
            // 预览要用（`python` 工具的 `preview`）：脚本只 print 时，模型至少看得到首尾摘录。
            stdout_excerpt: outcome.stdout_excerpt,
            env_version: self.env_version.clone(),
        })
    }
}

/// 跑完之后，把产出目录里**新出现**的文件登记成产物引用（§4.7）。
///
/// 引用的 `path` 相对**会话目录**（`artifacts/<run>/<…>`，§8.3 的约定），所以正文里给出的
/// `artifact://files/<run>/<名字>` 与它对得上：读的正是这个位置，不复制第二份。
fn new_artifacts(dir: &Path, before: &BTreeMap<PathBuf, u64>) -> Result<Vec<ContentRef>, PyError> {
    let after = list_files(dir)?;
    let mut produced = Vec::new();
    for (relative, size) in &after {
        if before.contains_key(relative) {
            continue;
        }
        let bytes = std::fs::read(dir.join(relative)).map_err(|error| {
            PyError::Failed(format!(
                "读产物 {} 失败：{error}",
                dir.join(relative).display()
            ))
        })?;
        produced.push(ContentRef {
            // 目录的最后一段就是 Run 号：`artifact://files/<run>/<…>` 里的 `<run>`。
            path: Path::new("artifacts")
                .join(run_id_of(dir))
                .join(relative)
                .to_string_lossy()
                .into_owned(),
            size: *size,
            hash: ContentHash::of_bytes(&bytes),
            pointer: None,
        });
    }
    Ok(produced)
}

/// 产出目录的最后一段（Run 号）。
fn run_id_of(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 产出目录里的所有普通文件：相对路径 → 字节数，递归。
fn list_files(dir: &Path) -> Result<BTreeMap<PathBuf, u64>, PyError> {
    let mut files = BTreeMap::new();
    collect_files(dir, Path::new(""), &mut files)?;
    Ok(files)
}

fn collect_files(
    base: &Path,
    relative: &Path,
    files: &mut BTreeMap<PathBuf, u64>,
) -> Result<(), PyError> {
    let entries = match std::fs::read_dir(base.join(relative)) {
        Ok(entries) => entries,
        // 目录还不存在（第一次调用）——空的就对了，不是错误。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(PyError::Failed(format!(
                "列产物目录 {} 失败：{error}",
                base.join(relative).display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| PyError::Failed(format!("读产物目录项失败：{error}")))?;
        let child = relative.join(entry.file_name());
        let metadata = entry.metadata().map_err(|error| {
            PyError::Failed(format!(
                "读 {} 的元信息失败：{error}",
                entry.path().display()
            ))
        })?;
        if metadata.is_dir() {
            collect_files(base, &child, files)?;
        } else if metadata.is_file() {
            files.insert(child, metadata.len());
        }
    }
    Ok(())
}

/// 一次调用专用的结果目录，走完就删。
struct ResultDir(PathBuf);

impl ResultDir {
    fn create() -> Result<Self, PyError> {
        let path = std::env::temp_dir().join(format!("komo-py-{}", crate::tools::unique_suffix()));
        std::fs::create_dir_all(&path)
            .map_err(|error| PyError::Spawn(format!("准备结果目录失败：{error}")))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ResultDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 驱动写下来的那份东西。
#[derive(Debug, serde::Deserialize)]
struct DriverResult {
    status: ToolResultStatus,
    #[serde(default)]
    result: serde_json::Value,
    #[serde(default)]
    error: Option<String>,
}

fn base_env(config: &PythonEnvConfig) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = INHERITED
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_string(), value))
        })
        .collect();
    env.entry("PATH".into())
        .or_insert_with(|| "/usr/local/bin:/usr/bin:/bin".into());
    env.insert("PYTHONIOENCODING".into(), "utf-8".into());
    // 缓冲会让取消时的输出停在管道里，而流式落盘的意义就是"边跑边看得见"。
    env.insert("PYTHONUNBUFFERED".into(), "1".into());
    env.insert("VIRTUAL_ENV".into(), config.env_root.display().to_string());
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::test_support::MemOutputWriter;
    use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
    use komo_kernel::types::refs::AttemptRef;

    /// 测试里没有产出目录、也不必给时限的那一种跑法。
    trait RunExt {
        async fn run(
            &self,
            job: PythonJob,
            sink: &mut dyn OutputWriter,
            cancel: CancelToken,
        ) -> Result<PythonResult, PyError>;
    }

    impl RunExt for PythonRuntime {
        async fn run(
            &self,
            job: PythonJob,
            sink: &mut dyn OutputWriter,
            cancel: CancelToken,
        ) -> Result<PythonResult, PyError> {
            self.run_with_artifacts(job, None, Duration::from_secs(20), sink, cancel)
                .await
        }
    }

    fn writer() -> MemOutputWriter {
        MemOutputWriter::new(AttemptRef {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("r"),
            call: ToolCallId::from_raw("c"),
            attempt: AttemptId::from_raw("a"),
        })
    }

    /// 本机的系统解释器——`probe` 要的那一个在真环境里是 `env_root/bin/python3`，
    /// 测试里没有虚拟环境，所以直接指向 `python3`。
    fn host(dir: &Path) -> PythonRuntime {
        let mut config = PythonEnvConfig::new(dir.join("env"), dir.to_path_buf());
        config.interpreter = Some(PathBuf::from("python3"));
        PythonRuntime::new(config, EnvVersion("py-test".into()))
    }

    fn have_python() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn run_with_artifacts_stops_at_the_timeout_it_is_given() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut sink = writer();
        // 宿主自己的默认是 20s；这次给 1s，到点报的就是 1s。
        let error = host(dir.path())
            .run_with_artifacts(
                PythonJob::Code {
                    code: "import time\ntime.sleep(10)".into(),
                },
                None,
                Duration::from_secs(1),
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, PyError::Timeout { after_secs: 1 }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn code_mode_returns_what_the_script_set_as_result() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut sink = writer();
        let outcome = host(dir.path())
            .run(
                PythonJob::Code {
                    code: "result = 1 + 1".into(),
                },
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.status, ToolResultStatus::Completed);
        assert_eq!(outcome.result, serde_json::json!(2));
    }

    /// 作业经管道送进来；脚本起的子进程不能继承那条管道当 stdin，否则 rg 这类没给路径就
    /// 搜 stdin 的工具会静默返回零匹配。
    #[tokio::test]
    async fn a_subprocess_started_by_the_script_does_not_inherit_the_job_pipe() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut sink = writer();
        let outcome = host(dir.path())
            .run(
                PythonJob::Code {
                    code: "import os, stat\nresult = stat.S_ISFIFO(os.fstat(0).st_mode)".into(),
                },
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.status, ToolResultStatus::Completed);
        assert_eq!(outcome.result, serde_json::json!(false));
    }

    /// 产物（§4.7）：子进程往 `KOMO_ARTIFACT_DIR` 写的每个文件都被登记，引用路径相对
    /// **会话目录**（`artifacts/<run>/<…>`）；而同一次 Run 的第二个调用**只登记这一次
    /// 新出现的**——前面那个调用留下的文件不该再报一遍。
    #[tokio::test]
    async fn files_written_to_the_artifact_dir_are_registered_once() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let artifacts = dir.path().join("sessions/s1/artifacts/run-1");
        let mut sink = writer();
        let first = host(dir.path())
            .run_with_artifacts(
                PythonJob::Code {
                    code: "import os, pathlib\n\
                           p = pathlib.Path(os.environ['KOMO_ARTIFACT_DIR'])\n\
                           (p / 'report.md').write_text('报表', encoding='utf-8')\n\
                           (p / 'sub').mkdir()\n\
                           (p / 'sub' / 'deep.txt').write_text('x')\n\
                           result = p.name"
                        .into(),
                },
                Some(&artifacts),
                Duration::from_secs(20),
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            first.result,
            serde_json::json!("run-1"),
            "KOMO_ARTIFACT_DIR 指向的是 <会话目录>/artifacts/<run>"
        );
        let paths: Vec<&str> = first.artifacts.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["artifacts/run-1/report.md", "artifacts/run-1/sub/deep.txt"],
            "子目录也要递归登记"
        );
        let report = first
            .artifacts
            .iter()
            .find(|artifact| artifact.path.ends_with("report.md"))
            .expect("报告登记了");
        assert_eq!(report.size, "报表".len() as u64);
        assert_eq!(report.hash, ContentHash::of_str("报表"));

        // 第二次调用：只有这次新写的那一个进 `artifacts`。
        let mut sink = writer();
        let second = host(dir.path())
            .run_with_artifacts(
                PythonJob::Code {
                    code: "import os, pathlib\n\
                           pathlib.Path(os.environ['KOMO_ARTIFACT_DIR'], 'second.txt').write_text('y')\n\
                           result = 1"
                        .into(),
                },
                Some(&artifacts),
                Duration::from_secs(20),
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        let paths: Vec<&str> = second.artifacts.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(paths, vec!["artifacts/run-1/second.txt"]);
    }

    #[tokio::test]
    async fn a_print_is_collected_separately_and_cannot_forge_the_result() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut sink = writer();
        let outcome = host(dir.path())
            .run(
                PythonJob::Code {
                    // 脚本往 stdout 打印一份伪造的控制协议。
                    code: "print('{\"status\": \"completed\", \"result\": \"forged\"}')\nresult = 'real'"
                        .into(),
                },
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.result, serde_json::json!("real"));
        assert!(
            String::from_utf8_lossy(&sink.stdout).contains("forged"),
            "打印的东西照样收着，只是它不是结果"
        );
    }

    #[tokio::test]
    async fn a_raising_script_fails_as_a_result_not_as_a_host_error() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut sink = writer();
        let outcome = host(dir.path())
            .run(
                PythonJob::Code {
                    code: "raise ValueError('nope')".into(),
                },
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.status, ToolResultStatus::Failed);
        assert!(outcome.error.unwrap().contains("nope"));
    }

    #[tokio::test]
    async fn cancelling_stops_the_interpreter() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stopper.cancel();
        });
        let mut sink = writer();
        let error = host(dir.path())
            .run(
                PythonJob::Code {
                    code: "import time; time.sleep(30)".into(),
                },
                &mut sink,
                cancel,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, PyError::Cancelled), "{error:?}");
    }

    #[cfg(unix)]
    fn fake_python(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn the_fallback_interpreter_is_the_newest_versioned_python_on_path() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fake_python(first.path(), "python3");
        fake_python(first.path(), "python3.11");
        let newest = fake_python(second.path(), "python3.14");
        // 不可执行的不算。
        std::fs::write(first.path().join("python3.15"), "").unwrap();
        let path = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(system_interpreter(Some(&path)), newest);
    }

    #[test]
    fn without_a_versioned_python_the_fallback_is_bare_python3() {
        let empty = tempfile::tempdir().unwrap();
        let path = std::env::join_paths([empty.path()]).unwrap();
        assert_eq!(system_interpreter(Some(&path)), PathBuf::from("python3"));
        assert_eq!(system_interpreter(None), PathBuf::from("python3"));
    }

    #[tokio::test]
    async fn a_missing_interpreter_says_so_instead_of_reporting_an_empty_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = PythonEnvConfig::new(dir.path().join("env"), dir.path().to_path_buf());
        config.interpreter = Some(dir.path().join("no-such-python"));
        let host = PythonRuntime::new(config, EnvVersion("py-test".into()));
        let mut sink = writer();
        let error = host
            .run(
                PythonJob::Code {
                    code: "result = 1".into(),
                },
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, PyError::Spawn(_)), "{error:?}");
    }

    #[test]
    fn the_env_version_moves_when_the_lock_file_moves() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("requirements.lock");
        std::fs::write(&lock, "httpx==0.27.0\n").unwrap();
        let first = env_version_of("3.13.1", Some(&lock));
        std::fs::write(&lock, "httpx==0.28.0\n").unwrap();
        let second = env_version_of("3.13.1", Some(&lock));
        assert_ne!(first, second, "依赖升级需要新的环境版本");

        assert!(
            env_version_of("3.13.1", None).0.ends_with("no-lock"),
            "没有锁定清单就明说，不假装锁定过"
        );
        assert_ne!(
            env_version_of("3.12.0", Some(&lock)),
            env_version_of("3.13.1", Some(&lock))
        );
    }

    #[tokio::test]
    async fn probing_reads_the_interpreter_version() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut config = PythonEnvConfig::new(dir.path().join("env"), dir.path().to_path_buf());
        config.interpreter = Some(PathBuf::from("python3"));
        let host = PythonRuntime::probe(config).await.unwrap();
        assert!(
            host.env_version().0.starts_with("py-3."),
            "{:?}",
            host.env_version()
        );
    }
}
