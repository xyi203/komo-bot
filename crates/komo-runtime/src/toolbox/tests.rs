//! §14 阶段 6 的验收，toolbox 这一侧。
//!
//! 真解释器：本机的 `python3`。没有它就跳过——`python_runtime` 的测试已经是这个约定。

use std::path::PathBuf;
use std::time::Duration;

use komo_kernel::types::plan::EnvVersion;
use time::OffsetDateTime;

use super::*;
use crate::python_runtime::{PythonEnvConfig, PythonRuntime};

const NOW: OffsetDateTime = time::macros::datetime!(2026-09-17 08:00:00 UTC);

fn have_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

fn host(dir: &std::path::Path) -> PythonRuntime {
    let mut config = PythonEnvConfig::new(dir.join("env"), dir.to_path_buf());
    config.interpreter = Some(PathBuf::from("python3"));
    config.timeout = Duration::from_secs(60);
    PythonRuntime::new(config, EnvVersion("py-test".into()))
}

fn toolbox(dir: &std::path::Path) -> Toolbox {
    let toolbox = Toolbox::new(dir.join("toolbox"));
    toolbox.ensure_layout().unwrap();
    toolbox
}

const GOOD: &str = r#""""一个会算加法的模块。"""

__all__ = ["add"]


def add(a, b):
    return a + b
"#;

const GOOD_TEST: &str = r#"import unittest

from toolbox.adder import add


class AddTest(unittest.TestCase):
    def test_adds(self):
        self.assertEqual(add(1, 2), 3)
"#;

const BROKEN: &str = r#"""" 一个算错的模块。"""

__all__ = ["add"]


def add(a, b):
    return a - b
"#;

// ---------------------------------------------------------------- 布局与版本

#[test]
fn the_layout_is_created_once_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    assert!(toolbox.layout().package_init().exists());
    assert!(toolbox.layout().readme().exists());
    assert!(toolbox.layout().staging_dir().is_dir());

    std::fs::write(toolbox.layout().readme(), "我改过的说明").unwrap();
    toolbox.ensure_layout().unwrap();
    assert_eq!(
        std::fs::read_to_string(toolbox.layout().readme()).unwrap(),
        "我改过的说明"
    );
}

/// 候选改一个字节，版本就变，上一次的测试结果**立刻对不上号**——这是"校验候选哈希与
/// 已测版本一致"的地基（§5.4）。
#[test]
fn editing_a_candidate_invalidates_the_test_result_it_had() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    toolbox
        .save_candidate("adder", GOOD, Some(GOOD_TEST), NOW)
        .unwrap();

    let version = toolbox.candidate("adder").unwrap().unwrap().version;
    // 假装跑过测试。
    let mut candidate = toolbox.candidate("adder").unwrap().unwrap();
    candidate.tests = Some(TestReport {
        version: version.clone(),
        passed: true,
        ran: 1,
        failures: 0,
        errors: 0,
        skipped: 0,
        output: "ok".into(),
        at: NOW,
    });
    write_json(&toolbox.layout().candidate_meta("adder"), &candidate).unwrap();
    assert!(toolbox.candidate("adder").unwrap().unwrap().tests.is_some());

    // 有人 edit 了候选。
    std::fs::write(
        toolbox.layout().candidate_code("adder"),
        format!("{GOOD}\n# 又加了一行\n"),
    )
    .unwrap();
    let after = toolbox.candidate("adder").unwrap().unwrap();
    assert_ne!(after.version, version);
    assert!(after.tests.is_none(), "那份结果不属于这一版了");

    let error = toolbox.enable("adder", None, NOW).unwrap_err();
    assert!(matches!(error, ToolboxError::Untested { .. }), "{error:?}");
}

// ---------------------------------------------------------------- 候选测试

#[tokio::test]
async fn a_candidate_that_passes_its_tests_can_be_enabled_and_one_that_fails_cannot() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    let host = host(dir.path());

    // 先是一个算错的版本。
    toolbox
        .save_candidate("adder", BROKEN, Some(GOOD_TEST), NOW)
        .unwrap();
    let report = toolbox
        .run_candidate_tests("adder", &host, NOW)
        .await
        .unwrap();
    assert!(!report.passed, "{report:?}");
    assert_eq!(report.failures, 1, "{report:?}");
    let error = toolbox.enable("adder", None, NOW).unwrap_err();
    assert!(matches!(error, ToolboxError::Untested { .. }), "{error:?}");
    assert!(
        !toolbox.layout().enabled_code("adder").exists(),
        "没通过测试的版本一秒钟都不该当过当前版本"
    );

    // 改对了再来。
    toolbox
        .save_candidate("adder", GOOD, Some(GOOD_TEST), NOW)
        .unwrap();
    let report = toolbox
        .run_candidate_tests("adder", &host, NOW)
        .await
        .unwrap();
    assert!(report.passed, "{report:?}");
    assert_eq!(report.ran, 1);

    let enabled = toolbox.enable("adder", None, NOW).unwrap();
    assert_eq!(enabled.version, report.version);
    assert_eq!(
        std::fs::read_to_string(toolbox.layout().enabled_code("adder")).unwrap(),
        GOOD
    );
    // 快照留下来了（§5.4「相关代码保存快照」）。
    assert!(
        toolbox
            .layout()
            .snapshot_code("adder", &enabled.version)
            .exists()
    );
    // 候选装完就清掉，不然下一次 enable 会以为还有新东西要装。
    assert!(!toolbox.layout().candidate_code("adder").exists());
}

/// 一个 import 不起来的测试文件跑了 0 个用例，而 `unittest` 对 0 个用例答"成功"。
/// 那不叫测过。
#[tokio::test]
async fn a_test_file_that_cannot_even_be_imported_is_not_a_pass() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    let host = host(dir.path());
    toolbox
        .save_candidate("adder", GOOD, Some("import nonexistent_module_xyz\n"), NOW)
        .unwrap();
    let report = toolbox
        .run_candidate_tests("adder", &host, NOW)
        .await
        .unwrap();
    assert!(!report.passed, "{report:?}");
}

#[tokio::test]
async fn a_candidate_without_any_test_says_so_instead_of_passing_vacuously() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    let host = host(dir.path());
    toolbox.save_candidate("adder", GOOD, None, NOW).unwrap();
    let error = toolbox
        .run_candidate_tests("adder", &host, NOW)
        .await
        .unwrap_err();
    assert!(matches!(error, ToolboxError::TestRun(_)), "{error:?}");
}

// ---------------------------------------------------------------- 启用与解析

#[test]
fn a_candidate_is_never_callable_only_the_enabled_version_is() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    toolbox
        .save_candidate("adder", GOOD, Some(GOOD_TEST), NOW)
        .unwrap();

    let error = toolbox.resolve_call("toolbox.adder", "add").unwrap_err();
    assert!(
        matches!(error, ToolboxError::NotEnabled { .. }),
        "{error:?}"
    );

    enable_now(&toolbox, "adder", GOOD, GOOD_TEST);
    let resolved = toolbox.resolve_call("toolbox.adder", "add").unwrap();
    assert_eq!(resolved.module, "adder");
    assert_eq!(resolved.version, ModuleVersion::of(GOOD, None));
    assert!(resolved.verifier.is_none());
}

#[test]
fn only_what_the_module_exports_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    enable_now(&toolbox, "adder", GOOD, GOOD_TEST);

    let error = toolbox.resolve_call("adder", "subtract").unwrap_err();
    assert!(
        matches!(error, ToolboxError::NotExported { .. }),
        "{error:?}"
    );

    // 没有 `__all__` 的模块一个函数都够不到（§5.2）。
    std::fs::write(
        toolbox.layout().enabled_code("adder"),
        "def add(a, b):\n    return a + b\n",
    )
    .unwrap();
    let error = toolbox.resolve_call("adder", "add").unwrap_err();
    assert!(matches!(error, ToolboxError::NoExports { .. }), "{error:?}");
}

/// 「校验候选哈希与已测版本一致」（§5.4）：审批时看到的是哪一版，装的就得是哪一版。
#[test]
fn a_candidate_changed_during_the_approval_window_refuses_to_be_installed() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    toolbox
        .save_candidate("adder", GOOD, Some(GOOD_TEST), NOW)
        .unwrap();
    pass_tests(&toolbox, "adder");
    let approved = toolbox.candidate("adder").unwrap().unwrap().version;

    // 审批还在等着的时候有人又改了候选。
    toolbox
        .save_candidate(
            "adder",
            &format!("{GOOD}\n# 偷偷加的\n"),
            Some(GOOD_TEST),
            NOW,
        )
        .unwrap();

    let error = toolbox.enable("adder", Some(&approved), NOW).unwrap_err();
    assert!(
        matches!(error, ToolboxError::VersionMismatch { .. }),
        "{error:?}"
    );
}

#[test]
fn disabling_keeps_the_snapshot_so_old_calls_stay_answerable() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    enable_now(&toolbox, "adder", GOOD, GOOD_TEST);
    let version = toolbox.enabled("adder").unwrap().unwrap().version;

    let disabled = toolbox.disable("adder").unwrap();
    assert_eq!(disabled.version, version);
    assert!(toolbox.enabled("adder").unwrap().is_none());
    assert!(toolbox.layout().snapshot_code("adder", &version).exists());
    assert!(matches!(
        toolbox.resolve_call("adder", "add").unwrap_err(),
        ToolboxError::NoSuchModule { .. }
    ));
}

// ---------------------------------------------------------------- 审批要展示的东西

/// §14 阶段 6：「toolbox 启用的审批在微信里能看到版本差异与测试结果」——这里是它的
/// 前一半：那两样**存在**且说的是这一版。渲染那一半在 gateway 的集成测试里。
#[test]
fn the_enable_preview_carries_the_version_diff_and_the_test_result() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    enable_now(&toolbox, "adder", GOOD, GOOD_TEST);
    let first = toolbox.enabled("adder").unwrap().unwrap().version;

    let next = GOOD.replace("return a + b", "return a + b + 0");
    toolbox
        .save_candidate("adder", &next, Some(GOOD_TEST), NOW)
        .unwrap();
    pass_tests(&toolbox, "adder");

    let preview = toolbox.enable_preview("adder").unwrap();
    assert_eq!(preview.previous, Some(first.clone()));
    assert_ne!(preview.version, first);
    assert!(
        preview.changes.contains("-     return a + b\n"),
        "{}",
        preview.changes
    );
    assert!(
        preview.changes.contains("+     return a + b + 0"),
        "{}",
        preview.changes
    );
    assert!(
        preview.changes.contains("  def add"),
        "上下文也在：{}",
        preview.changes
    );
    assert!(
        preview.evidence().contains("候选测试通过"),
        "{}",
        preview.evidence()
    );
    assert!(preview.headline().contains(first.as_str()));
    assert!(preview.changes_block().contains(preview.version.as_str()));
}

#[test]
fn a_new_module_shows_its_whole_body_as_the_change() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    toolbox
        .save_candidate("adder", GOOD, Some(GOOD_TEST), NOW)
        .unwrap();
    let preview = toolbox.enable_preview("adder").unwrap();
    assert!(preview.previous.is_none());
    assert!(preview.changes.contains("+ def add"), "{}", preview.changes);
    // 没跑过测试就**明说没跑过**：空白读起来像"没有问题"。
    assert!(
        preview.evidence().contains("还没有跑过测试"),
        "{}",
        preview.evidence()
    );
}

// ---------------------------------------------------------------- 清单

#[test]
fn the_list_shows_enabled_modules_and_candidate_only_ones_alike() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    enable_now(&toolbox, "adder", GOOD, GOOD_TEST);
    toolbox
        .save_candidate("scratch", "__all__ = []\n", None, NOW)
        .unwrap();

    let listed = toolbox.list().unwrap();
    let names: Vec<&str> = listed.iter().map(|m| m.module.as_str()).collect();
    assert_eq!(names, vec!["adder", "scratch"]);
    assert!(listed[0].enabled.is_some());
    assert_eq!(listed[0].exports, vec!["add"]);
    assert!(listed[1].enabled.is_none());
    assert!(listed[1].candidate.is_some());
    assert!(listed[1].exports.is_empty(), "没启用就没有导出可言");
}

#[test]
fn a_module_nobody_ever_wrote_is_a_clear_no_such_module() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    assert!(matches!(
        toolbox.inspect("ghost").unwrap_err(),
        ToolboxError::NoSuchModule { .. }
    ));
    assert!(matches!(
        toolbox.inspect("../etc/passwd").unwrap_err(),
        ToolboxError::BadName(_)
    ));
}

// ---------------------------------------------------------------- 内置 memos

/// §14 的 Memory 覆盖里那四条（写入返回 ID / 链接、查询回原文、改完给新内容、写入未知
/// 时先核对）在 **Python 侧**的证据：内置模块自带的那份测试真的跑得过。
#[tokio::test]
async fn the_builtin_memos_module_passes_its_own_tests() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    let host = host(dir.path());

    builtin::install(&toolbox, NOW).unwrap();
    // 内置模块是直接启用的，所以把它的正文与测试当成一个候选再跑一遍。
    toolbox
        .save_candidate("memos", builtin::MEMOS, Some(builtin::MEMOS_TEST), NOW)
        .unwrap();
    let report = toolbox
        .run_candidate_tests("memos", &host, NOW)
        .await
        .unwrap();
    assert!(report.passed, "{}", report.output);
    assert!(report.ran >= 8, "{report:?}");
}

#[test]
fn the_builtin_memos_module_is_callable_the_moment_it_is_installed() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = toolbox(dir.path());
    builtin::install(&toolbox, NOW).unwrap();

    let resolved = toolbox.resolve_call("toolbox.memos", "create").unwrap();
    assert_eq!(resolved.version, ModuleVersion::of(builtin::MEMOS, None));
    assert_eq!(resolved.verifier.as_deref(), Some("verify"));

    let info = toolbox.inspect("memos").unwrap();
    assert!(info.enabled.unwrap().builtin);
    assert!(info.doc.unwrap().contains("Memos"));
}

// ---------------------------------------------------------------- 脚手架

/// 直接把一份正文装成当前版本——测的是别的事情时不必绕候选流程一圈。
fn enable_now(toolbox: &Toolbox, module: &str, code: &str, tests: &str) {
    toolbox
        .install_builtin(module, code, Some(tests), NOW)
        .unwrap()
        .expect("装得上");
}

/// 把候选标成"测过且通过"，不真跑 Python。
fn pass_tests(toolbox: &Toolbox, module: &str) {
    let mut candidate = toolbox.candidate(module).unwrap().unwrap();
    candidate.tests = Some(TestReport {
        version: candidate.version.clone(),
        passed: true,
        ran: 1,
        failures: 0,
        errors: 0,
        skipped: 0,
        output: "ok".into(),
        at: NOW,
    });
    write_json(&toolbox.layout().candidate_meta(module), &candidate).unwrap();
}
