//! toolbox：保存下来的 Python 能力，以及它们的**候选 → 测试 → 审核 → 启用**（§5.3、§5.4）。
//!
//! ```text
//! 读取模块和使用说明
//!   → write/edit 写入候选版本          Toolbox::save_candidate
//!   → Policy 审核候选代码的执行         （候选执行走 python 的 code 模式，照常过 Policy）
//!   → 运行测试并保存结果                Toolbox::run_candidate_tests
//!   → 展示差异、依赖变化和验证证据      Toolbox::enable_plan → ApprovalPresentation
//!   → Policy 审核启用操作               Operation::ToolboxChange
//!   → 校验候选哈希与已测版本一致        Toolbox::enable 的第一件事
//!   → 原子替换当前版本                  写临时文件 + rename
//! ```
//!
//! 三条边界在这个模块里各只有一处实现：
//!
//! - **只有已启用版本调得到**（§5.2）。`call` 模式解析的是 `toolbox/<module>.py`，
//!   而候选在 `.staging/` 里——那不是一个合法的 Python 包名，所以候选**在语言层面**
//!   就 import 不到（`layout` 的模块文档）。
//! - **未测过的版本启用不了**（§5.4「校验候选哈希与已测版本一致」）。测试结果记在候选
//!   的元数据里并**绑定版本**：改一个字节，版本就变，那份结果立刻对不上号。
//! - **版本变化使旧授权失效**（§5.4、§7.2）。启用写进
//!   [`ModuleVersion`]，`python` 的 `call` 计划把它填进
//!   [`PlanVersions::module`](komo_kernel::types::plan::PlanVersions)，于是
//!   `Grant::covers` 的 `versions_cover` 自动把绑旧版本的授权挡在外面——kernel 里已经
//!   有那条测试，这里不重复实现判断。

pub mod builtin;
pub mod diff;
pub mod layout;
pub mod source;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use komo_kernel::traits::{OutputWriter, PythonHost};
use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
use komo_kernel::types::refs::{AttemptRef, ToolResultStatus};
use komo_kernel::types::tool::{CancelToken, PythonJob};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use layout::{Layout, ModuleVersion, dotted, normalize_module};
pub use source::Declarations;

/// toolbox 操作出错。**每一种都说得出是哪一步**：操作者要按它决定下一步做什么。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolboxError {
    #[error("{0}")]
    BadName(String),
    #[error("toolbox 里没有模块 {module}")]
    NoSuchModule { module: String },
    #[error("{module} 没有候选版本；先写 .staging/{module}.py")]
    NoCandidate { module: String },
    #[error("{module} 的候选是 {candidate}，要启用的是 {wanted}：候选哈希与已测版本不一致")]
    VersionMismatch {
        module: String,
        candidate: ModuleVersion,
        wanted: ModuleVersion,
    },
    #[error("{module} 的候选 {version} 还没有通过测试：{why}")]
    Untested {
        module: String,
        version: ModuleVersion,
        why: String,
    },
    #[error("{module} 没有声明 __all__：call 模式只调用明确导出的函数")]
    NoExports { module: String },
    #[error("{module} 没有导出 {function}")]
    NotExported { module: String, function: String },
    #[error("{module} 现在没有启用中的版本")]
    NotEnabled { module: String },
    #[error("跑测试失败：{0}")]
    TestRun(String),
    #[error("{path}：{message}")]
    Io { path: String, message: String },
}

fn io(path: &Path, error: std::io::Error) -> ToolboxError {
    ToolboxError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

// ---------------------------------------------------------------- 元数据

/// 一次候选测试的结果（§5.4「运行测试并保存结果」）。
///
/// **绑定版本**：`version` 是这份结果测的那一份代码。启用时比对的就是它，所以"改一行
/// 再启用"会被挡下来——那一行没有被任何测试跑过。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestReport {
    pub version: ModuleVersion,
    pub passed: bool,
    /// 跑了几个用例。
    #[serde(default)]
    pub ran: u32,
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub errors: u32,
    #[serde(default)]
    pub skipped: u32,
    /// 测试输出的摘要（已截断）。
    #[serde(default)]
    pub output: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

impl TestReport {
    /// 一行话，够放进审批消息的"已有验证"那一项。
    pub fn headline(&self) -> String {
        let verdict = if self.passed { "通过" } else { "未通过" };
        format!(
            "候选测试{verdict}：{} 个用例，{} 失败，{} 出错，{} 跳过（版本 {}，{}）",
            self.ran, self.failures, self.errors, self.skipped, self.version, self.at
        )
    }

    /// 审批里那一整块"已有验证结果"。
    pub fn evidence(&self) -> String {
        if self.output.trim().is_empty() {
            self.headline()
        } else {
            format!("{}\n\n{}", self.headline(), self.output.trim())
        }
    }
}

/// 一个候选（`.staging/<module>.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub module: String,
    pub version: ModuleVersion,
    #[serde(with = "time::serde::rfc3339")]
    pub saved_at: OffsetDateTime,
    /// 这个候选最近一次测试的结果。`None` = 从未跑过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<TestReport>,
}

impl Candidate {
    /// 这个候选现在够不够格被启用。
    fn tested(&self) -> Result<&TestReport, ToolboxError> {
        match &self.tests {
            Some(report) if report.version != self.version => Err(ToolboxError::Untested {
                module: self.module.clone(),
                version: self.version.clone(),
                why: format!("最近一次测试测的是 {}，代码在那之后改过", report.version),
            }),
            Some(report) if !report.passed => Err(ToolboxError::Untested {
                module: self.module.clone(),
                version: self.version.clone(),
                why: report.headline(),
            }),
            Some(report) => Ok(report),
            None => Err(ToolboxError::Untested {
                module: self.module.clone(),
                version: self.version.clone(),
                why: "还没有跑过候选测试".into(),
            }),
        }
    }
}

/// 一个**已启用**版本（`.versions/<module>/enabled.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enabled {
    pub module: String,
    pub version: ModuleVersion,
    #[serde(with = "time::serde::rfc3339")]
    pub enabled_at: OffsetDateTime,
    /// 启用时那份验证证据。审批之后它就是这一版的档案。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<TestReport>,
    /// 随 komo 装进来的内置模块（§5.5 的 memos）。
    #[serde(default)]
    pub builtin: bool,
}

/// 一个模块在 toolbox 里的全貌。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleInfo {
    pub module: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<Enabled>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<Candidate>,
    /// 已启用版本导出的函数。没有启用版本时为空。
    #[serde(default)]
    pub exports: Vec<String>,
    /// 与版本绑定的核对函数（§8.6）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier: Option<String>,
    /// 这个模块要的**凭证引用**：变量名，不是值（§5.3、§7.2）。
    #[serde(default)]
    pub env: Vec<String>,
    /// 模块开头的 docstring。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

// ---------------------------------------------------------------- toolbox

/// `~/.komo/toolbox` 这一个目录。
///
/// 它**不持有** [`PythonHost`]：跑候选测试是一次真实的 Python 执行，宿主由调用方传进
/// 来，这样"谁在跑这段代码"在调用点上是看得见的。
#[derive(Debug, Clone)]
pub struct Toolbox {
    layout: Layout,
    /// 依赖锁定清单——版本的另一半（§5.1）。
    requirements: Option<PathBuf>,
}

impl Toolbox {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Toolbox {
            layout: Layout::new(root),
            requirements: None,
        }
    }

    pub fn with_requirements(mut self, requirements: Option<PathBuf>) -> Self {
        self.requirements = requirements;
        self
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// 建出 §5.3 那棵树。已经在的一个都不动。
    pub fn ensure_layout(&self) -> Result<(), ToolboxError> {
        for dir in [
            self.layout.root().to_path_buf(),
            self.layout.tests_dir(),
            self.layout.staging_dir(),
            self.layout.versions_dir(),
        ] {
            std::fs::create_dir_all(&dir).map_err(|error| io(&dir, error))?;
        }
        for (path, body) in [
            (self.layout.package_init(), PACKAGE_INIT),
            (self.layout.tests_dir().join("__init__.py"), ""),
            (self.layout.readme(), README),
        ] {
            if !path.exists() {
                std::fs::write(&path, body).map_err(|error| io(&path, error))?;
            }
        }
        Ok(())
    }

    /// 锁文件的正文——版本的另一半。读不到就是"没有锁定"。
    fn lock(&self) -> Option<Vec<u8>> {
        self.requirements
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
    }

    fn version_of(&self, code: &str) -> ModuleVersion {
        ModuleVersion::of(code, self.lock().as_deref())
    }

    // ---- 读 ----

    /// toolbox 里有哪些模块：已启用的、只有候选的，都算。
    pub fn list(&self) -> Result<Vec<ModuleInfo>, ToolboxError> {
        let mut names: Vec<String> = Vec::new();
        for dir in [self.layout.root().to_path_buf(), self.layout.staging_dir()] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let Some(stem) = name.strip_suffix(".py") else {
                    continue;
                };
                if stem == "__init__" || stem.starts_with("test_") {
                    continue;
                }
                if let Ok(module) = normalize_module(stem)
                    && !names.contains(&module)
                {
                    names.push(module);
                }
            }
        }
        names.sort();
        names.into_iter().map(|name| self.inspect(&name)).collect()
    }

    /// 一个模块的全貌。模块不存在（既没启用也没候选）时报 [`ToolboxError::NoSuchModule`]。
    pub fn inspect(&self, module: &str) -> Result<ModuleInfo, ToolboxError> {
        let module = self.name(module)?;
        let enabled = self.enabled(&module)?;
        let candidate = self.candidate(&module)?;
        if enabled.is_none() && candidate.is_none() {
            return Err(ToolboxError::NoSuchModule { module });
        }
        let declarations = match self.enabled_code(&module)? {
            Some(code) => source::declarations(&code),
            None => Declarations::default(),
        };
        Ok(ModuleInfo {
            module,
            enabled,
            candidate,
            exports: declarations.exports,
            verifier: declarations.verifier,
            env: declarations.env,
            doc: declarations.doc,
        })
    }

    /// 已启用版本的正文。
    pub fn enabled_code(&self, module: &str) -> Result<Option<String>, ToolboxError> {
        let module = self.name(module)?;
        read_optional(&self.layout.enabled_code(&module))
    }

    /// 候选的正文。
    pub fn candidate_code(&self, module: &str) -> Result<Option<String>, ToolboxError> {
        let module = self.name(module)?;
        read_optional(&self.layout.candidate_code(&module))
    }

    pub fn enabled(&self, module: &str) -> Result<Option<Enabled>, ToolboxError> {
        let module = self.name(module)?;
        // 指针在，正文不在 = 这个模块被删掉了，不算启用。
        if !self.layout.enabled_code(&module).exists() {
            return Ok(None);
        }
        read_json(&self.layout.enabled_pointer(&module))
    }

    pub fn candidate(&self, module: &str) -> Result<Option<Candidate>, ToolboxError> {
        let module = self.name(module)?;
        let Some(code) = read_optional(&self.layout.candidate_code(&module))? else {
            return Ok(None);
        };
        let version = self.version_of(&code);
        // 元数据以**正文**为准：文件被 write / edit 改过之后版本就变了，那份旧结果
        // 不再属于它（§5.4「校验候选哈希与已测版本一致」）。
        let meta: Option<Candidate> = read_json(&self.layout.candidate_meta(&module))?;
        Ok(Some(match meta {
            Some(mut meta) if meta.version == version => {
                meta.module = module;
                meta
            }
            other => Candidate {
                module,
                version,
                saved_at: other
                    .map(|m| m.saved_at)
                    .unwrap_or(OffsetDateTime::UNIX_EPOCH),
                tests: None,
            },
        }))
    }

    /// `call` 模式要的那三样：启用中的版本、导出清单、核对函数。
    ///
    /// 这是 [`crate::tools::PythonTool::prepare`] 的入口，所以它**一行代码都不执行**
    /// ——声明是静态读出来的（见 [`source`]）。
    pub fn resolve_call(&self, module: &str, function: &str) -> Result<ResolvedCall, ToolboxError> {
        let module = self.name(module)?;
        let Some(code) = read_optional(&self.layout.enabled_code(&module))? else {
            // 有候选而没启用：说清楚是"还没启用"而不是"没有这个模块"——那是两个不同
            // 的下一步。
            return Err(if self.layout.candidate_code(&module).exists() {
                ToolboxError::NotEnabled { module }
            } else {
                ToolboxError::NoSuchModule { module }
            });
        };
        let declarations = source::declarations(&code);
        if declarations.exports.is_empty() {
            return Err(ToolboxError::NoExports { module });
        }
        if !declarations.exports(function) {
            return Err(ToolboxError::NotExported {
                module,
                function: function.to_string(),
            });
        }
        Ok(ResolvedCall {
            version: self.version_of(&code),
            verifier: declarations.verifier,
            env: declarations.env,
            module,
        })
    }

    // ---- 写 ----

    /// 存一个候选版本（§5.4 的第二步）。
    ///
    /// `tests` 是这个候选自带的测试正文；`None` = 沿用已启用版本的 `tests/test_<m>.py`。
    /// 存下来的候选**没有测试结果**：代码一变，上一次的结果就不属于它了。
    pub fn save_candidate(
        &self,
        module: &str,
        code: &str,
        tests: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<Candidate, ToolboxError> {
        let module = self.name(module)?;
        self.ensure_layout()?;
        write_atomic(&self.layout.candidate_code(&module), code.as_bytes())?;
        if let Some(tests) = tests {
            write_atomic(&self.layout.candidate_test(&module), tests.as_bytes())?;
        }
        let candidate = Candidate {
            module: module.clone(),
            version: self.version_of(code),
            saved_at: now,
            tests: None,
        };
        write_json(&self.layout.candidate_meta(&module), &candidate)?;
        Ok(candidate)
    }

    /// 跑候选自带的测试，把结果记进候选的元数据（§5.4）。
    ///
    /// **测试入口是约定**：候选的 `.staging/test_<module>.py`，没有就用已启用版本的
    /// `tests/test_<module>.py`；文件是一个 `unittest` 模块，里面的
    /// `unittest.TestCase` 子类会被 `unittest` 自己发现。跑的时候候选被挂成
    /// `toolbox.<module>`，所以测试里照常写 `from toolbox.<module> import create`。
    ///
    /// 挂载点是一个**临时目录**，不是 `.staging` 本身：`.staging` 在 driver 的导入
    /// 拒绝名单里（§7.3），而候选测试恰恰是要 import 候选的那一次——把它放在拒绝名单
    /// 里的目录上跑，等于让这两条规则互相打架。
    pub async fn run_candidate_tests(
        &self,
        module: &str,
        host: &dyn PythonHost,
        now: OffsetDateTime,
    ) -> Result<TestReport, ToolboxError> {
        let module = self.name(module)?;
        let Some(candidate) = self.candidate(&module)? else {
            return Err(ToolboxError::NoCandidate { module });
        };
        let code = self
            .candidate_code(&module)?
            .ok_or_else(|| ToolboxError::NoCandidate {
                module: module.clone(),
            })?;
        let tests = match read_optional(&self.layout.candidate_test(&module))? {
            Some(body) => body,
            None => read_optional(&self.layout.enabled_test(&module))?.ok_or_else(|| {
                ToolboxError::TestRun(format!(
                    "{module} 没有测试：写 .staging/test_{module}.py 或 tests/test_{module}.py"
                ))
            })?,
        };

        let stage = Mount::create(&module, &code, &tests)?;
        let job = PythonJob::Code {
            code: harness(stage.path(), &module),
        };
        let mut sink = NullWriter::detached();
        let outcome = host
            .run(job, &mut sink, CancelToken::new())
            .await
            .map_err(|error| ToolboxError::TestRun(error.to_string()))?;

        let mut report = match outcome.status {
            ToolResultStatus::Completed => serde_json::from_value::<RawReport>(outcome.result)
                .map(|raw| raw.into_report(candidate.version.clone(), now))
                .unwrap_or_else(|error| TestReport {
                    version: candidate.version.clone(),
                    passed: false,
                    ran: 0,
                    failures: 0,
                    errors: 1,
                    skipped: 0,
                    output: format!("测试结果读不动：{error}"),
                    at: now,
                }),
            _ => TestReport {
                version: candidate.version.clone(),
                passed: false,
                ran: 0,
                failures: 0,
                errors: 1,
                skipped: 0,
                output: outcome
                    .error
                    .unwrap_or_else(|| "测试进程没有给出结果".into()),
                at: now,
            },
        };
        if report.output.trim().is_empty() {
            report.output = sink.tail();
        }

        let recorded = Candidate {
            tests: Some(report.clone()),
            ..candidate
        };
        write_json(&self.layout.candidate_meta(&module), &recorded)?;
        Ok(report)
    }

    /// 启用中的版本与候选之间的那份改动，加上候选的测试结果。
    ///
    /// 审批消息的「改动」与「已有验证」两项就是它（§7.2、§14 阶段 6 的验收列）。
    pub fn enable_preview(&self, module: &str) -> Result<EnablePreview, ToolboxError> {
        let module = self.name(module)?;
        let Some(candidate) = self.candidate(&module)? else {
            return Err(ToolboxError::NoCandidate { module });
        };
        let after = self
            .candidate_code(&module)?
            .ok_or_else(|| ToolboxError::NoCandidate {
                module: module.clone(),
            })?;
        let before = self.enabled_code(&module)?;
        let previous = self.enabled(&module)?.map(|e| e.version);
        let changes = match &before {
            Some(before) => diff::unified(before, &after),
            None => diff::added(&after),
        };
        Ok(EnablePreview {
            module,
            previous,
            version: candidate.version.clone(),
            changes,
            tests: candidate.tests.clone(),
            candidate,
        })
    }

    /// 启用一个候选版本（§5.4 的最后两步）。
    ///
    /// 顺序是定死的：**先校验候选哈希与已测版本一致，再原子替换**。反过来做的话，一个
    /// 没通过测试的版本会先成为"当前版本"，再被拒绝——而在那之间它是可调用的。
    ///
    /// `wanted` 给出时必须与候选当前的版本逐字相同：这是"批准的是哪一版"与"现在要装的
    /// 是哪一版"的对账，审批期间有人改过候选就在这里被挡下来。
    pub fn enable(
        &self,
        module: &str,
        wanted: Option<&ModuleVersion>,
        now: OffsetDateTime,
    ) -> Result<Enabled, ToolboxError> {
        let module = self.name(module)?;
        let Some(candidate) = self.candidate(&module)? else {
            return Err(ToolboxError::NoCandidate { module });
        };
        if let Some(wanted) = wanted
            && wanted != &candidate.version
        {
            return Err(ToolboxError::VersionMismatch {
                module,
                candidate: candidate.version,
                wanted: wanted.clone(),
            });
        }
        let report = candidate.tested()?.clone();

        let code = self
            .candidate_code(&module)?
            .ok_or_else(|| ToolboxError::NoCandidate {
                module: module.clone(),
            })?;
        let tests = match read_optional(&self.layout.candidate_test(&module))? {
            Some(body) => Some(body),
            None => read_optional(&self.layout.enabled_test(&module))?,
        };

        let enabled = Enabled {
            module: module.clone(),
            version: candidate.version.clone(),
            enabled_at: now,
            tests: Some(report),
            builtin: false,
        };
        self.install(&module, &code, tests.as_deref(), &enabled)?;

        // 候选已经成为当前版本，`.staging` 里那一份留着只会让下一次 `enable` 以为还有
        // 新东西要装。
        let _ = std::fs::remove_file(self.layout.candidate_code(&module));
        let _ = std::fs::remove_file(self.layout.candidate_test(&module));
        let _ = std::fs::remove_file(self.layout.candidate_meta(&module));
        Ok(enabled)
    }

    /// 停用一个模块：正文移出 `toolbox/`，**快照与元数据留着**。
    ///
    /// 留着是有用的——停用之后"当初那一版是什么"仍然要答得出来，账本里还有引用它的
    /// 调用。
    pub fn disable(&self, module: &str) -> Result<Enabled, ToolboxError> {
        let module = self.name(module)?;
        let Some(enabled) = self.enabled(&module)? else {
            return Err(ToolboxError::NotEnabled { module });
        };
        for path in [
            self.layout.enabled_code(&module),
            self.layout.enabled_test(&module),
        ] {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(self.layout.enabled_pointer(&module));
        Ok(enabled)
    }

    /// 装一个**内置**模块：随 komo 一起发的那些（§5.5 的 memos）。
    ///
    /// 「首次启动若 toolbox 里没有就写入并标已启用，版本 = 内容哈希」。它不经候选流程，
    /// 因为它不是模型写的——审核发生在 komo 自己的代码评审里。**已经有同名模块就不动**：
    /// 操作者或模型改过的那一份是他们的，不该被一次升级悄悄盖掉。
    pub fn install_builtin(
        &self,
        module: &str,
        code: &str,
        tests: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<Option<Enabled>, ToolboxError> {
        let module = self.name(module)?;
        self.ensure_layout()?;
        if self.layout.enabled_code(&module).exists() {
            return Ok(None);
        }
        let enabled = Enabled {
            module: module.clone(),
            version: self.version_of(code),
            enabled_at: now,
            tests: None,
            builtin: true,
        };
        self.install(&module, code, tests, &enabled)?;
        Ok(Some(enabled))
    }

    /// 真正把一份正文装成"当前版本"：先留快照，再原子替换。
    fn install(
        &self,
        module: &str,
        code: &str,
        tests: Option<&str>,
        enabled: &Enabled,
    ) -> Result<(), ToolboxError> {
        self.ensure_layout()?;
        let versions = self.layout.module_versions_dir(module);
        std::fs::create_dir_all(&versions).map_err(|error| io(&versions, error))?;

        // 快照先落地：替换之后再写快照的话，中间崩一次就没人知道当前这一份是哪一版
        // （§5.4「相关代码保存快照」）。
        write_atomic(
            &self.layout.snapshot_code(module, &enabled.version),
            code.as_bytes(),
        )?;
        if let Some(tests) = tests {
            write_atomic(
                &self.layout.snapshot_test(module, &enabled.version),
                tests.as_bytes(),
            )?;
        }
        write_json(
            &self.layout.snapshot_meta(module, &enabled.version),
            enabled,
        )?;

        write_atomic(&self.layout.enabled_code(module), code.as_bytes())?;
        if let Some(tests) = tests {
            write_atomic(&self.layout.enabled_test(module), tests.as_bytes())?;
        }
        write_json(&self.layout.enabled_pointer(module), enabled)?;
        Ok(())
    }

    fn name(&self, raw: &str) -> Result<String, ToolboxError> {
        normalize_module(raw).map_err(ToolboxError::BadName)
    }
}

/// `call` 模式解析出来的那一份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCall {
    pub module: String,
    /// 已启用版本。计划绑定的就是它（§7.2）。
    pub version: ModuleVersion,
    /// 与版本绑定的核对函数（§8.6）。没有就没有可靠恢复方式。
    pub verifier: Option<String>,
    /// 凭证引用：**变量名**。进计划的是它，凭证的值不进（§7.2）。
    pub env: Vec<String>,
}

/// 启用审批要展示的那两项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnablePreview {
    pub module: String,
    /// 上一版；`None` = 这是一个新模块。
    pub previous: Option<ModuleVersion>,
    pub version: ModuleVersion,
    /// 版本差异（§7.2 的"改动"）。
    pub changes: String,
    /// 候选测试结果（§7.2 的"已有验证结果"）。
    pub tests: Option<TestReport>,
    pub candidate: Candidate,
}

impl EnablePreview {
    /// 「已有验证」那一项。没跑过测试就**明说没跑过**，不留空——空白读起来像"没有问题"。
    pub fn evidence(&self) -> String {
        match &self.tests {
            Some(report) => report.evidence(),
            None => format!("{} 的候选 {} 还没有跑过测试", self.module, self.version),
        }
    }

    /// 「改动」那一项的抬头：从哪一版到哪一版。
    pub fn headline(&self) -> String {
        match &self.previous {
            Some(previous) => format!("{}：{previous} → {}", self.module, self.version),
            None => format!("{}：新模块 → {}", self.module, self.version),
        }
    }

    /// 「改动」整块：抬头 + diff。
    pub fn changes_block(&self) -> String {
        format!("{}\n\n{}", self.headline(), self.changes.trim_end())
    }
}

// ---------------------------------------------------------------- 跑测试

/// 候选测试的挂载点：一个临时的 `toolbox` 包，走完就删。
struct Mount(PathBuf);

impl Mount {
    fn create(module: &str, code: &str, tests: &str) -> Result<Mount, ToolboxError> {
        let root = std::env::temp_dir().join(format!(
            "komo-toolbox-test-{}",
            crate::tools::unique_suffix()
        ));
        let package = root.join("toolbox");
        let test_dir = package.join("tests");
        std::fs::create_dir_all(&test_dir).map_err(|error| io(&test_dir, error))?;
        std::fs::write(package.join("__init__.py"), PACKAGE_INIT)
            .map_err(|error| io(&package, error))?;
        std::fs::write(test_dir.join("__init__.py"), "").map_err(|error| io(&test_dir, error))?;
        std::fs::write(package.join(format!("{module}.py")), code)
            .map_err(|error| io(&package, error))?;
        std::fs::write(test_dir.join(format!("test_{module}.py")), tests)
            .map_err(|error| io(&test_dir, error))?;
        Ok(Mount(root))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 跑 `unittest` 并把结论写进 `result` 的那段代码。
///
/// `sys.path.insert(0, ...)`：挂载点要**排在**真 toolbox 的前面，否则测的是已启用的
/// 那一份，而不是候选。
fn harness(root: &Path, module: &str) -> String {
    format!(
        r#"import io, sys, unittest
sys.path.insert(0, {root})
stream = io.StringIO()
loader = unittest.TestLoader()
try:
    suite = loader.loadTestsFromName("toolbox.tests.test_{module}")
except Exception as error:
    result = {{"loaded": False, "output": "测试模块加载不了：%r" % (error,)}}
else:
    run = unittest.TextTestRunner(stream=stream, verbosity=2).run(suite)
    result = {{
        "loaded": True,
        "passed": run.wasSuccessful(),
        "ran": run.testsRun,
        "failures": len(run.failures),
        "errors": len(run.errors),
        "skipped": len(run.skipped),
        "output": stream.getvalue()[-4000:],
    }}
"#,
        root = python_string(&root.display().to_string()),
    )
}

fn python_string(raw: &str) -> String {
    format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
}

#[derive(Debug, Deserialize)]
struct RawReport {
    #[serde(default)]
    loaded: bool,
    #[serde(default)]
    passed: bool,
    #[serde(default)]
    ran: u32,
    #[serde(default)]
    failures: u32,
    #[serde(default)]
    errors: u32,
    #[serde(default)]
    skipped: u32,
    #[serde(default)]
    output: String,
}

impl RawReport {
    fn into_report(self, version: ModuleVersion, at: OffsetDateTime) -> TestReport {
        TestReport {
            version,
            // 加载不出来的测试模块不算通过——一个 import 失败的测试文件跑了 0 个用例，
            // 而 `wasSuccessful()` 对 0 个用例答 True。
            passed: self.loaded && self.passed && self.ran > 0,
            ran: self.ran,
            failures: self.failures,
            errors: if self.loaded { self.errors } else { 1 },
            skipped: self.skipped,
            output: self.output,
            at,
        }
    }
}

/// 一个**不发布**的流式写入器：留一段尾巴给报告，其余丢掉。
///
/// 两个调用点都不是"一次工具调用"，所以没有 `ToolOutputStore` 那条路可走：候选测试是
/// 操作者发起的一次执行，核对函数是恢复流程里的一步。它们的输出不该长出一份
/// `output.json` 来——那份账是给 `tool.started` 配的，而这两次都没有 started。
pub struct NullWriter {
    attempt: AttemptRef,
    buffer: Vec<u8>,
    written: u64,
}

const CAPTURE_LIMIT: usize = 8 * 1024;

impl std::fmt::Debug for NullWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NullWriter")
            .field("written", &self.written)
            .finish_non_exhaustive()
    }
}

impl NullWriter {
    /// 挂在一次真实调用的身份上（核对函数用这个）。
    pub fn new(ctx: &komo_kernel::types::tool::ToolContext) -> Self {
        NullWriter {
            attempt: AttemptRef {
                session: ctx.session.clone(),
                run: ctx.run.clone(),
                call: ctx.call.clone(),
                attempt: ctx.attempt.clone(),
            },
            buffer: Vec::new(),
            written: 0,
        }
    }

    /// 没有调用身份可言（候选测试用这个）。
    pub fn detached() -> Self {
        NullWriter {
            attempt: AttemptRef {
                session: SessionId::from_raw("toolbox"),
                run: RunId::from_raw("toolbox-tests"),
                call: ToolCallId::from_raw("toolbox-tests"),
                attempt: AttemptId::from_raw("toolbox-tests"),
            },
            buffer: Vec::new(),
            written: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.written += chunk.len() as u64;
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() > CAPTURE_LIMIT {
            let cut = self.buffer.len() - CAPTURE_LIMIT;
            self.buffer.drain(..cut);
        }
    }

    /// 最后写下来的那一段。
    pub fn tail(&self) -> String {
        String::from_utf8_lossy(&self.buffer).trim().to_string()
    }
}

#[async_trait::async_trait]
impl OutputWriter for NullWriter {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), komo_kernel::traits::StoreError> {
        self.push(chunk);
        Ok(())
    }

    async fn write_stderr(&mut self, chunk: &[u8]) -> Result<(), komo_kernel::traits::StoreError> {
        self.push(chunk);
        Ok(())
    }

    fn attempt(&self) -> &AttemptRef {
        &self.attempt
    }

    fn bytes_written(&self) -> u64 {
        self.written
    }
}

// ---------------------------------------------------------------- 文件

fn read_optional(path: &Path) -> Result<Option<String>, ToolboxError> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(Some(body)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(path, error)),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, ToolboxError> {
    let Some(body) = read_optional(path)? else {
        return Ok(None);
    };
    // 读不动的元数据不该让整个 toolbox 报错：正文才是权威，元数据是它的账。
    Ok(serde_json::from_str(&body).ok())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ToolboxError> {
    let body = serde_json::to_vec_pretty(value).map_err(|error| ToolboxError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    write_atomic(path, &body)
}

/// 先写临时文件再 rename：读到的要么是上一版，要么是新版，**不会是半个**
/// （§5.4「原子替换当前版本」）。
fn write_atomic(path: &Path, body: &[u8]) -> Result<(), ToolboxError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    }
    let temporary = path.with_extension(format!(
        "{}.partial",
        path.extension().and_then(|e| e.to_str()).unwrap_or("tmp")
    ));
    std::fs::write(&temporary, body).map_err(|error| io(&temporary, error))?;
    std::fs::rename(&temporary, path).map_err(|error| io(path, error))
}

const PACKAGE_INIT: &str = "\"\"\"komo 的 toolbox：已启用的可复用 Python 能力（§5.3）。\"\"\"\n";

const README: &str = r#"# toolbox

已保存的 Python 能力。`python` 工具的 `mode = "call"` 只调用**这个目录里**模块的
`__all__` 所导出的函数。

- `<module>.py` —— 已启用版本，唯一 `import toolbox.<module>` 够得到的那一份。
- `tests/test_<module>.py` —— 这一版的测试。
- `.staging/` —— 候选。候选不是合法的包名，所以 import 不到；driver 里还有一道导入
  拒绝（§7.3）。

写一个新版本：

1. `write` 到 `.staging/<module>.py`（需要的话再写 `.staging/test_<module>.py`）。
2. `komo toolbox test <module>` 跑候选测试。
3. `komo toolbox enable <module>` ——这一步产生一条审批，消息里带**版本差异**与
   **测试结果**；批准之后候选才成为当前版本。

模块约定：

- `__all__ = [...]` 列出导出函数，**没有它 call 模式一个函数都调不到**。
- `__komo_verify__ = "verify"` 指一个与版本绑定的核对函数（§8.6）。它被调用的形式是
  `verify(function=..., args={...})`，要答
  `{"kind": "already_satisfied" | "not_performed" | "conflict" | "unknown", ...}`。
  核对函数自己也要写进 `__all__`。
- 服务地址与凭证**从环境变量读**，由配置点名传进来；不要把凭证写进代码或打印出来。
"#;
