//! 受管理的 Python 解释器（§5.1）。
//!
//! 每次调用起一个新进程，不依赖跨调用的内存变量；stdout / stderr 流式写进
//! [`OutputWriter`]，**结构化结果走另一条路**（一个结果文件），所以脚本打印的 JSON
//! 永远伪造不了执行结果。取消时终止进程组并等待回收。
//!
//! [`PythonHost::env_version`] = **解释器版本 + 依赖清单哈希**。它进
//! [`ExecutionPlan::versions`](komo_kernel::types::plan::ExecutionPlan)，所以依赖一
//! 升级，绑定旧环境的授权就覆盖不到新计划了（§5.4）。环境本身的变更（装依赖、切
//! 版本）是 `Operation::PythonEnvChange`，走 Policy，**不在这个模块里做**。
//!
//! toolbox 的保存、候选测试与启用是后面的阶段（§5.4），这里只有"跑"。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, PythonHost};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::plan::EnvVersion;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{CancelToken, PyError, PythonJob, PythonResult};

use crate::tools::process::{ChildSpec, ProcessError, run_child};

/// 解释器驱动。脚本输出与控制协议分开就靠它。
const DRIVER: &str = include_str!("driver.py");

/// 一次 Python 调用默认跑多久。
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// 默认最多往输出存储里写多少字节。
pub const DEFAULT_OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

const INHERITED: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TZ", "TMPDIR"];

/// 受管理环境的位置。
#[derive(Debug, Clone)]
pub struct PythonEnvConfig {
    /// 虚拟环境目录（`~/.komo/python-envs/<版本>`）。
    pub env_root: PathBuf,
    /// 解释器；`None` = `env_root/bin/python3`。
    pub interpreter: Option<PathBuf>,
    /// `toolbox` 包的**父目录**——它进 PYTHONPATH，于是 `import toolbox.ha` 成立。
    pub toolbox_parent: Option<PathBuf>,
    /// 依赖锁定清单。它的哈希是 `env_version` 的另一半（§5.1「依赖按锁定清单安装」）。
    pub requirements: Option<PathBuf>,
    /// 子进程的工作目录。
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub output_limit: u64,
}

impl PythonEnvConfig {
    pub fn new(env_root: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            env_root: env_root.into(),
            interpreter: None,
            toolbox_parent: None,
            requirements: None,
            cwd: cwd.into(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            output_limit: DEFAULT_OUTPUT_LIMIT,
        }
    }

    pub fn interpreter_path(&self) -> PathBuf {
        self.interpreter
            .clone()
            .unwrap_or_else(|| self.env_root.join("bin/python3"))
    }
}

/// [`PythonHost`] 的生产实现。
pub struct PythonRuntime {
    config: PythonEnvConfig,
    env_version: EnvVersion,
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
        }
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
    async fn run(
        &self,
        job: PythonJob,
        sink: &mut dyn OutputWriter,
        cancel: CancelToken,
    ) -> Result<PythonResult, PyError> {
        // 结果文件放在一个只属于这次调用的目录里。不用 `tempfile`——它在这个 crate
        // 里只是 dev-dependency（§13.4 的依赖清单）。
        let workspace = ResultDir::create()?;
        let result_path = workspace.path().join("result.json");

        let body = serde_json::to_vec(&job)
            .map_err(|error| PyError::Protocol(format!("作业无法序列化：{error}")))?;

        let mut env = base_env(&self.config);
        env.insert("KOMO_RESULT_PATH".into(), result_path.display().to_string());

        let spec = ChildSpec {
            program: self.config.interpreter_path().display().to_string(),
            // `-B`：不要在被审核过的 toolbox 目录里留下 __pycache__。
            args: vec!["-B".into(), "-c".into(), DRIVER.into()],
            cwd: self.config.cwd.clone(),
            env,
            stdin: Some(body),
            timeout: self.config.timeout,
            output_limit: self.config.output_limit,
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
                after_secs: self.config.timeout.as_secs(),
            });
        }

        let Ok(raw) = std::fs::read_to_string(&result_path) else {
            // 没有结果文件：解释器在写下结论之前就没了。**不编造返回值**——退出码和
            // stderr 是这次仅有的证据。
            return Err(PyError::Protocol(format!(
                "解释器没有写下结果（退出码 {:?}）：{}",
                outcome.exit_code,
                outcome.stderr_tail.trim()
            )));
        };
        let reported: DriverResult = serde_json::from_str(&raw)
            .map_err(|error| PyError::Protocol(format!("结果文件读不动：{error}")))?;

        Ok(PythonResult {
            status: reported.status,
            result: reported.result,
            error: reported.error,
            artifacts: vec![],
            env_version: self.env_version.clone(),
        })
    }

    fn env_version(&self) -> EnvVersion {
        self.env_version.clone()
    }
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
    if let Some(parent) = &config.toolbox_parent {
        env.insert("PYTHONPATH".into(), parent.display().to_string());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::test_support::MemOutputWriter;
    use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
    use komo_kernel::types::refs::AttemptRef;

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
        config.timeout = Duration::from_secs(20);
        PythonRuntime::new(config, EnvVersion("py-test".into()))
    }

    fn have_python() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
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
    async fn call_mode_only_reaches_functions_the_module_exports() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let toolbox = dir.path().join("toolbox");
        std::fs::create_dir_all(&toolbox).unwrap();
        std::fs::write(toolbox.join("__init__.py"), "").unwrap();
        std::fs::write(
            toolbox.join("ha.py"),
            "__all__ = ['turn_off']\n\ndef turn_off(entity_id):\n    return {'off': entity_id}\n\ndef _secret():\n    return 'nope'\n\ndef undeclared():\n    return 'nope'\n",
        )
        .unwrap();

        let mut config = PythonEnvConfig::new(dir.path().join("env"), dir.path().to_path_buf());
        config.interpreter = Some(PathBuf::from("python3"));
        config.toolbox_parent = Some(dir.path().to_path_buf());
        config.timeout = Duration::from_secs(20);
        let host = PythonRuntime::new(config, EnvVersion("py-test".into()));

        let mut sink = writer();
        let exported = host
            .run(
                PythonJob::Call {
                    module: "toolbox.ha".into(),
                    function: "turn_off".into(),
                    args: serde_json::json!({ "entity_id": "light.living_room" }),
                },
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            exported.result,
            serde_json::json!({ "off": "light.living_room" })
        );

        for hidden in ["_secret", "undeclared"] {
            let mut sink = writer();
            let refused = host
                .run(
                    PythonJob::Call {
                        module: "toolbox.ha".into(),
                        function: hidden.into(),
                        args: serde_json::json!({}),
                    },
                    &mut sink,
                    CancelToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(refused.status, ToolResultStatus::Failed, "{hidden}");
            assert!(
                refused.error.unwrap().contains("导出"),
                "{hidden} 不该被 call 模式够到"
            );
        }
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
