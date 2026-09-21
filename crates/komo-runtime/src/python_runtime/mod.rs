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
//! toolbox 的保存、候选测试与启用在 [`crate::toolbox`]，这里只有"跑"——外加一件
//! **执行边界**的事（§7.3）：`denied_imports` 里的目录（候选与历史快照）经
//! `KOMO_DENIED_IMPORTS` 交给 driver 的导入钩子，于是"通过导入未知模块提前执行未审核
//! 代码"这条路是关着的。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, PythonHost};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::plan::EnvVersion;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{CancelToken, PyError, PythonJob, PythonResult};

use crate::tools::process::{ChildRegistration, ChildSpec, ProcessError, run_child};

/// 解释器驱动。脚本输出与控制协议分开就靠它。
const DRIVER: &str = include_str!("driver.py");

/// 一次 Python 调用默认跑多久。
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// 默认最多往输出存储里写多少字节。
pub const DEFAULT_OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

const INHERITED: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TZ", "TMPDIR"];

/// 凭证引用的**解析口**（§5.3「HA、Memos、搜索服务地址和凭证引用通过配置传给已授权
/// 模块」）。
///
/// 为什么不是"从 Gateway 的进程环境里取"：单元文件里不放 `.env` 的内容（launchd 没有
/// `EnvironmentFile` 的等价物，只能把值抄进 plist），而且一旦进了进程环境，`.env` 的
/// 热重载就失效了——旧值会一直活到重启为止——并且这台机器上每一个子进程、每一条
/// `/proc/<pid>/environ` 都看得见它。所以值由 Gateway **按名、在每次 spawn 时**解析，
/// 用完就随子进程一起消失。
///
/// 它同时回答"名字"那一半，因为两个问题的答主本来就是同一个：**哪些名字**由模块自己的
/// `__komo_env__` 声明（`ExecutionPlan.resources` 里那串 `credential_env` 就是从它长出
/// 来的，同一处解析，两边不可能对不上），**值**由 `.env` 给。分成两个 port 只会让
/// "计划里写着要 X、子进程里拿到的却是 Y" 变成一个可能发生的事。
///
/// **值不进 `Debug`、不进日志、不进任何事件或计划。**
pub trait SecretResolver: Send + Sync {
    /// 这个 toolbox 模块声明了哪些凭证引用——**只有名字**。
    fn names_for(&self, module: &str) -> Vec<String>;

    /// 按名解析一个值。解析不到答 `None`：那个变量就**不设置**，让模块自己说
    /// "未配置"——一个空串会让模块以为配过了。
    fn resolve(&self, name: &str) -> Option<String>;
}

/// 受管理环境的位置。
#[derive(Clone)]
pub struct PythonEnvConfig {
    /// 虚拟环境目录（`~/.komo/python-envs/<版本>`）。
    pub env_root: PathBuf,
    /// 解释器；`None` = `env_root/bin/python3`。
    pub interpreter: Option<PathBuf>,
    /// `toolbox` 包的**父目录**——它进 PYTHONPATH，于是 `import toolbox.ha` 成立。
    pub toolbox_parent: Option<PathBuf>,
    /// 依赖锁定清单。它的哈希是 `env_version` 的另一半（§5.1「依赖按锁定清单安装」）。
    pub requirements: Option<PathBuf>,
    /// `code` 模式**不许 import** 的目录（§7.3）：toolbox 的 `.staging` 与
    /// `.versions`。模型不能通过 import 未审核的候选把它提前跑起来。
    pub denied_imports: Vec<PathBuf>,
    /// 凭证引用的解析口。`None` = 这台 Gateway 不给任何模块传凭证。
    pub secrets: Option<Arc<dyn SecretResolver>>,
    /// 子进程的工作目录。
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub output_limit: u64,
}

/// 手写的 `Debug`：`secrets` 只说有没有装上。一个 `#[derive(Debug)]` 迟早会把某个
/// 实现者的内部状态（连着它握着的那份 `.env`）印进日志。
impl std::fmt::Debug for PythonEnvConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonEnvConfig")
            .field("env_root", &self.env_root)
            .field("interpreter", &self.interpreter)
            .field("toolbox_parent", &self.toolbox_parent)
            .field("requirements", &self.requirements)
            .field("denied_imports", &self.denied_imports)
            .field("secrets", &self.secrets.as_ref().map(|_| "<装上了>"))
            .field("cwd", &self.cwd)
            .field("timeout", &self.timeout)
            .field("output_limit", &self.output_limit)
            .finish()
    }
}

impl PythonEnvConfig {
    pub fn new(env_root: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            env_root: env_root.into(),
            interpreter: None,
            toolbox_parent: None,
            requirements: None,
            denied_imports: Vec::new(),
            secrets: None,
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
        // 凭证在**这一刻**解析，跟着这一个子进程走（§5.3）。
        let (credentials, missing) = credential_env(&self.config, &job);
        if !missing.is_empty() {
            // 名字，不是值。
            tracing::warn!(
                variables = missing.join("、"),
                "模块声明的凭证引用没有配置：这些变量不会传给子进程"
            );
        }
        env.extend(credentials);

        let spec = ChildSpec {
            program: self.config.interpreter_path().display().to_string(),
            // `-B`：不要在被审核过的 toolbox 目录里留下 __pycache__。
            args: vec!["-B".into(), "-c".into(), DRIVER.into()],
            cwd: self.config.cwd.clone(),
            env,
            stdin: Some(body),
            timeout: self.config.timeout,
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
            // 预览要用（`python` 工具的 `preview`）：脚本只 print 时，模型至少看得到尾巴。
            stdout_tail: outcome.stdout_tail,
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
    // §7.3：driver 读它，拒绝任何解析到这些目录下的 import。名单为空时钩子不装。
    if !config.denied_imports.is_empty() {
        let joined: Vec<String> = config
            .denied_imports
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        env.insert("KOMO_DENIED_IMPORTS".into(), joined.join(SEPARATOR));
    }
    env
}

/// 这一次调用要带上的凭证（§5.3）。**每次 spawn 现解析**，所以 `.env` 改完、
/// `komo config reload` 之后，下一次调用拿到的就是新值。
///
/// 两条边界：
///
/// - **只有 `call` 模式有凭证。** 任意 `code` 一个都拿不到——「任意 code 模式不会因为
///   import 了已审核模块而自动获得同样授权」（§5.2），那么它也不该拿到那个模块的钥匙。
/// - **基础变量不许被模块声明覆盖**：一个声明了 `PATH` 的模块不能借此换掉解释器要走的
///   那条路径。
///
/// 答 `(要设置的变量, 声明了却解析不到的名字)`。第二个由调用方 `warn!` 出来——**名字**，
/// 不是值。
fn credential_env(
    config: &PythonEnvConfig,
    job: &PythonJob,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut env = BTreeMap::new();
    let mut missing = Vec::new();
    let (Some(secrets), PythonJob::Call { module, .. }) = (&config.secrets, job) else {
        return (env, missing);
    };
    for name in secrets.names_for(module) {
        if INHERITED.contains(&name.as_str()) {
            continue;
        }
        match secrets.resolve(&name) {
            Some(value) => {
                env.insert(name, value);
            }
            // 不设置，也不放一个空串进去：模块因此会说"未配置"，而不是拿着空令牌去
            // 敲远端然后收到一个 401。
            None => missing.push(name),
        }
    }
    (env, missing)
}

/// `KOMO_DENIED_IMPORTS` 的分隔符。不用 `:`——路径里有冒号的系统上那会切错。
const SEPARATOR: &str = "\n";

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

    /// §7.3：`code` 模式 **import 不到未启用的候选**。
    ///
    /// 两道门各挡一半：`.staging` 不是合法包名（所以 `import toolbox..staging.ha` 根本
    /// 不成立），而把候选目录塞进 `sys.path` 再 `import ha` 这一手由 driver 的钩子挡。
    /// 这里测的是第二道——第一道由布局保证，测不出"失败"来。
    #[tokio::test]
    async fn code_mode_cannot_import_a_candidate_that_was_never_enabled() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("toolbox/.staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(
            staging.join("sneaky.py"),
            "import pathlib
pathlib.Path('/tmp/komo-should-not-exist').write_text('x')
",
        )
        .unwrap();

        let mut config = PythonEnvConfig::new(dir.path().join("env"), dir.path().to_path_buf());
        config.interpreter = Some(PathBuf::from("python3"));
        config.denied_imports = vec![staging.clone()];
        config.timeout = Duration::from_secs(20);
        let host = PythonRuntime::new(config, EnvVersion("py-test".into()));

        let mut sink = writer();
        let outcome = host
            .run(
                PythonJob::Code {
                    code: format!(
                        "import sys
sys.path.insert(0, {:?})
import sneaky
result = 'imported'",
                        staging.display().to_string()
                    ),
                },
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.status, ToolResultStatus::Failed, "{outcome:?}");
        let error = outcome.error.unwrap_or_default();
        assert!(error.contains("未经审核"), "{error}");
    }

    /// 名单为空时钩子不装，普通 import 一切照旧——一道只在需要时存在的门。
    #[tokio::test]
    async fn an_empty_deny_list_leaves_ordinary_imports_alone() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut sink = writer();
        let outcome = host(dir.path())
            .run(
                PythonJob::Code {
                    code: "import json
result = json.dumps({'ok': True})"
                        .into(),
                },
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.status, ToolResultStatus::Completed, "{outcome:?}");
    }

    /// 已启用的模块照常 import 得到：禁区只挡候选与快照，不挡当前版本。
    #[tokio::test]
    async fn the_enabled_version_is_still_importable_while_the_candidate_is_not() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let toolbox = dir.path().join("toolbox");
        let staging = toolbox.join(".staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(toolbox.join("__init__.py"), "").unwrap();
        std::fs::write(
            toolbox.join("ha.py"),
            "__all__ = ['ping']

def ping():
    return 'pong'
",
        )
        .unwrap();
        std::fs::write(
            staging.join("ha.py"),
            "raise SystemExit(1)
",
        )
        .unwrap();

        let mut config = PythonEnvConfig::new(dir.path().join("env"), dir.path().to_path_buf());
        config.interpreter = Some(PathBuf::from("python3"));
        config.toolbox_parent = Some(dir.path().to_path_buf());
        config.denied_imports = vec![staging];
        config.timeout = Duration::from_secs(20);
        let host = PythonRuntime::new(config, EnvVersion("py-test".into()));

        let mut sink = writer();
        let outcome = host
            .run(
                PythonJob::Call {
                    module: "toolbox.ha".into(),
                    function: "ping".into(),
                    args: serde_json::json!({}),
                },
                &mut sink,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.result, serde_json::json!("pong"), "{outcome:?}");
    }

    /// 一份只认得几个名字的解析口。**测试里也不碰进程环境**——这一整波改动的要点
    /// 就是"凭证不走进程环境"。
    #[derive(Debug, Default)]
    struct FakeSecrets {
        declared: BTreeMap<String, Vec<String>>,
        values: BTreeMap<String, String>,
        /// 问过哪些名字，按顺序——用来证明"每次 spawn 现解析"。
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl FakeSecrets {
        fn new(module: &str, declared: &[&str], values: &[(&str, &str)]) -> Arc<FakeSecrets> {
            Arc::new(FakeSecrets {
                declared: BTreeMap::from([(
                    module.to_string(),
                    declared.iter().map(|n| (*n).to_string()).collect(),
                )]),
                values: values
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
                asked: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().expect("问过的名字").clone()
        }
    }

    impl SecretResolver for FakeSecrets {
        fn names_for(&self, module: &str) -> Vec<String> {
            self.declared.get(module).cloned().unwrap_or_default()
        }

        fn resolve(&self, name: &str) -> Option<String> {
            self.asked
                .lock()
                .expect("问过的名字")
                .push(name.to_string());
            self.values.get(name).cloned()
        }
    }

    /// 一个 `toolbox.vault` 模块，把问到的三个变量原样交回来。
    fn vault(dir: &Path) -> PythonRuntime {
        let toolbox = dir.join("toolbox");
        std::fs::create_dir_all(&toolbox).unwrap();
        std::fs::write(toolbox.join("__init__.py"), "").unwrap();
        std::fs::write(
            toolbox.join("vault.py"),
            "import os\n\n__all__ = ['peek']\n\n\ndef peek(names):\n    return {name: os.environ.get(name) for name in names}\n",
        )
        .unwrap();
        let mut config = PythonEnvConfig::new(dir.join("env"), dir.to_path_buf());
        config.interpreter = Some(PathBuf::from("python3"));
        config.toolbox_parent = Some(dir.to_path_buf());
        config.timeout = Duration::from_secs(20);
        PythonRuntime::new(config, EnvVersion("py-test".into()))
    }

    async fn peek(host: &PythonRuntime, names: &[&str]) -> serde_json::Value {
        let mut sink = writer();
        host.run(
            PythonJob::Call {
                module: "toolbox.vault".into(),
                function: "peek".into(),
                args: serde_json::json!({ "names": names }),
            },
            &mut sink,
            CancelToken::new(),
        )
        .await
        .unwrap()
        .result
    }

    /// 凭证**不经过 Gateway 的进程环境**：进程里没有这个变量，解析口里有，子进程就
    /// 读得到（§5.3）。
    #[tokio::test]
    async fn a_declared_credential_reaches_the_child_without_ever_touching_the_process_environment()
    {
        if !have_python() {
            return;
        }
        assert!(
            std::env::var("KOMO_TEST_VAULT_TOKEN").is_err(),
            "这个测试的前提就是进程环境里没有它"
        );
        let dir = tempfile::tempdir().unwrap();
        let mut host = vault(dir.path());
        let secrets = FakeSecrets::new(
            "toolbox.vault",
            &["KOMO_TEST_VAULT_TOKEN"],
            &[("KOMO_TEST_VAULT_TOKEN", "from-dot-env")],
        );
        host.config.secrets = Some(Arc::clone(&secrets) as Arc<dyn SecretResolver>);

        let read = peek(&host, &["KOMO_TEST_VAULT_TOKEN"]).await;
        assert_eq!(
            read,
            serde_json::json!({ "KOMO_TEST_VAULT_TOKEN": "from-dot-env" })
        );
        assert!(
            std::env::var("KOMO_TEST_VAULT_TOKEN").is_err(),
            "解析一次不该把它塞进 Gateway 自己的环境"
        );
    }

    /// **每次 spawn 现解析**：`.env` 改完之后下一次调用就是新值，不用重启。
    #[tokio::test]
    async fn the_value_is_resolved_at_every_spawn_so_a_reloaded_env_takes_effect_at_once() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut host = vault(dir.path());

        let first = FakeSecrets::new("toolbox.vault", &["TOK"], &[("TOK", "old")]);
        host.config.secrets = Some(Arc::clone(&first) as Arc<dyn SecretResolver>);
        assert_eq!(
            peek(&host, &["TOK"]).await,
            serde_json::json!({ "TOK": "old" })
        );
        assert_eq!(first.asked(), vec!["TOK"], "问过一次");

        // `.env` 改了、reload 了——生产里换的是 `ConfigHolder` 里那份快照，解析口每次
        // 现读，所以这里换掉它背后的值就等价。
        let next = FakeSecrets::new("toolbox.vault", &["TOK"], &[("TOK", "new")]);
        host.config.secrets = Some(Arc::clone(&next) as Arc<dyn SecretResolver>);
        assert_eq!(
            peek(&host, &["TOK"]).await,
            serde_json::json!({ "TOK": "new" })
        );
        assert_eq!(next.asked(), vec!["TOK"], "第二次是重新问出来的，不是缓存");
    }

    /// 子进程里**只有声明过的名字**：`.env` 里其余的变量一个都不进去。
    #[tokio::test]
    async fn only_the_names_the_module_declared_reach_the_child() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut host = vault(dir.path());
        host.config.secrets = Some(FakeSecrets::new(
            "toolbox.vault",
            &["DECLARED"],
            &[
                ("DECLARED", "yes"),
                ("ANOTHER_SECRET_IN_DOT_ENV", "must-not-leak"),
                // 基础变量：声明了也不许覆盖。
                ("PATH", "/hijacked"),
            ],
        ) as Arc<dyn SecretResolver>);

        let read = peek(&host, &["DECLARED", "ANOTHER_SECRET_IN_DOT_ENV"]).await;
        assert_eq!(
            read,
            serde_json::json!({ "DECLARED": "yes", "ANOTHER_SECRET_IN_DOT_ENV": null })
        );
    }

    /// 声明了 `PATH` 也换不掉解释器要走的那条路。
    #[tokio::test]
    async fn a_module_cannot_declare_its_way_over_a_base_variable() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = PythonEnvConfig::new(dir.path().join("env"), dir.path().to_path_buf());
        config.secrets = Some(FakeSecrets::new(
            "toolbox.vault",
            &["PATH", "TOK"],
            &[("PATH", "/hijacked"), ("TOK", "ok")],
        ) as Arc<dyn SecretResolver>);
        let (env, missing) = credential_env(
            &config,
            &PythonJob::Call {
                module: "toolbox.vault".into(),
                function: "peek".into(),
                args: serde_json::json!({}),
            },
        );
        assert_eq!(env.get("TOK").map(String::as_str), Some("ok"));
        assert!(!env.contains_key("PATH"), "基础变量不许被声明覆盖");
        assert!(missing.is_empty());
    }

    /// 声明了、`.env` 里却没有：**不设置**那个变量，并把名字（不是值）报出来，
    /// 模块自己会说"未配置"。
    #[tokio::test]
    async fn a_declared_but_unconfigured_credential_is_left_unset_and_named() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut config = PythonEnvConfig::new(dir.path().join("env"), dir.path().to_path_buf());
        config.secrets = Some(
            FakeSecrets::new("toolbox.vault", &["MISSING_TOK"], &[]) as Arc<dyn SecretResolver>
        );
        let job = PythonJob::Call {
            module: "toolbox.vault".into(),
            function: "peek".into(),
            args: serde_json::json!({}),
        };
        let (env, missing) = credential_env(&config, &job);
        assert!(env.is_empty(), "一个空串比没有更糟：模块会以为配过了");
        assert_eq!(missing, vec!["MISSING_TOK"]);

        // 子进程那一侧：变量真的不在，模块因此报得出"未配置"。
        let mut host = vault(dir.path());
        host.config.secrets = Some(
            FakeSecrets::new("toolbox.vault", &["MISSING_TOK"], &[]) as Arc<dyn SecretResolver>
        );
        assert_eq!(
            peek(&host, &["MISSING_TOK"]).await,
            serde_json::json!({ "MISSING_TOK": null })
        );
    }

    /// 任意 `code` 一个凭证都拿不到（§5.2：import 了已审核模块也不获得同样授权，
    /// 那么也不该拿到它的钥匙）。
    #[tokio::test]
    async fn arbitrary_code_gets_no_credentials_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = PythonEnvConfig::new(dir.path().join("env"), dir.path().to_path_buf());
        let secrets = FakeSecrets::new("toolbox.vault", &["TOK"], &[("TOK", "value")]);
        config.secrets = Some(Arc::clone(&secrets) as Arc<dyn SecretResolver>);
        let (env, missing) = credential_env(
            &config,
            &PythonJob::Code {
                code: "import os; result = os.environ.get('TOK')".into(),
            },
        );
        assert!(env.is_empty(), "{env:?}");
        assert!(missing.is_empty());
        assert!(secrets.asked().is_empty(), "连问都不该问");
    }

    /// 凭证的值不进 `Debug`。
    #[test]
    fn the_config_debug_never_prints_a_secret() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = PythonEnvConfig::new(dir.path().join("env"), dir.path().to_path_buf());
        config.secrets = Some(FakeSecrets::new(
            "toolbox.vault",
            &["TOK"],
            &[("TOK", "super-secret-value")],
        ) as Arc<dyn SecretResolver>);
        let printed = format!("{config:?}");
        assert!(!printed.contains("super-secret-value"), "{printed}");
        assert!(printed.contains("<装上了>"), "{printed}");
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
