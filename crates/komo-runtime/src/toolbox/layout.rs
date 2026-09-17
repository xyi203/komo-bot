//! toolbox 的文件布局与版本（§5.3、§5.4）。
//!
//! §5.3 的目录树逐字照做：
//!
//! ```text
//! toolbox/
//! ├── __init__.py
//! ├── README.md
//! ├── ha.py            ← 已启用版本，唯一 `import toolbox.ha` 够得到的那一份
//! ├── memos.py
//! ├── tests/
//! └── .staging/        ← 候选
//! ```
//!
//! 文档没有给"版本号存在哪"，而 §5.4 要求"每次调用记录……实际使用的自定义模块版本"
//! 且"相关代码保存快照"，所以另加两处**隐藏**位置，不改变上面那棵树的可见形状：
//!
//! ```text
//! toolbox/.staging/<module>.py        候选代码
//! toolbox/.staging/test_<module>.py   候选测试（可选；缺省沿用 tests/test_<module>.py）
//! toolbox/.staging/<module>.json      候选元数据：版本、保存时刻、候选测试结果
//! toolbox/.versions/<module>/<version>.py        代码快照
//! toolbox/.versions/<module>/test_<version>.py   测试快照
//! toolbox/.versions/<module>/<version>.json      这一版的元数据
//! toolbox/.versions/<module>/enabled.json        当前启用的是哪一版
//! ```
//!
//! **候选天然 import 不到**：`.staging` 不是合法的 Python 标识符，所以
//! `toolbox/.staging/memos.py` 永远不会是 `toolbox.memos`。这是 §7.3 的第一道门；
//! 第二道是 driver 里的导入钩子（见 `python_runtime/driver.py`）。

use std::path::{Path, PathBuf};

use komo_kernel::types::digest::ContentHash;
use serde::{Deserialize, Serialize};

/// 一个已保存模块的版本：**代码哈希 + 依赖锁哈希**（§5.4）。
///
/// 两半都要：同一份代码在换过依赖的环境里不是同一个东西，而
/// [`PlanVersions::env`](komo_kernel::types::plan::PlanVersions) 记的是整个环境，
/// 粒度对不上"这个模块被审核时依赖是什么"。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModuleVersion(pub String);

impl ModuleVersion {
    /// 代码前 12 位 + 锁文件前 8 位；没有锁文件就明写 `nolock`，不假装锁定过。
    pub fn of(code: &str, lock: Option<&[u8]>) -> Self {
        let code = ContentHash::of_str(code);
        let lock = match lock {
            Some(bytes) => ContentHash::of_bytes(bytes).as_str()[..8].to_string(),
            None => "nolock".to_string(),
        };
        ModuleVersion(format!("{}-{lock}", &code.as_str()[..12]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModuleVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `toolbox` 目录的路径代数。只算路径，不碰磁盘。
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Layout { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `toolbox` 包的**父目录**——它进 PYTHONPATH，于是 `import toolbox.ha` 成立。
    pub fn parent(&self) -> PathBuf {
        self.root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn package_init(&self) -> PathBuf {
        self.root.join("__init__.py")
    }

    pub fn readme(&self) -> PathBuf {
        self.root.join("README.md")
    }

    /// 已启用版本的正文。
    pub fn enabled_code(&self, module: &str) -> PathBuf {
        self.root.join(format!("{module}.py"))
    }

    pub fn tests_dir(&self) -> PathBuf {
        self.root.join("tests")
    }

    /// 已启用版本的测试。
    pub fn enabled_test(&self, module: &str) -> PathBuf {
        self.tests_dir().join(format!("test_{module}.py"))
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(".staging")
    }

    pub fn candidate_code(&self, module: &str) -> PathBuf {
        self.staging_dir().join(format!("{module}.py"))
    }

    pub fn candidate_test(&self, module: &str) -> PathBuf {
        self.staging_dir().join(format!("test_{module}.py"))
    }

    pub fn candidate_meta(&self, module: &str) -> PathBuf {
        self.staging_dir().join(format!("{module}.json"))
    }

    pub fn versions_dir(&self) -> PathBuf {
        self.root.join(".versions")
    }

    pub fn module_versions_dir(&self, module: &str) -> PathBuf {
        self.versions_dir().join(module)
    }

    pub fn snapshot_code(&self, module: &str, version: &ModuleVersion) -> PathBuf {
        self.module_versions_dir(module)
            .join(format!("{version}.py"))
    }

    pub fn snapshot_test(&self, module: &str, version: &ModuleVersion) -> PathBuf {
        self.module_versions_dir(module)
            .join(format!("test_{version}.py"))
    }

    pub fn snapshot_meta(&self, module: &str, version: &ModuleVersion) -> PathBuf {
        self.module_versions_dir(module)
            .join(format!("{version}.json"))
    }

    pub fn enabled_pointer(&self, module: &str) -> PathBuf {
        self.module_versions_dir(module).join("enabled.json")
    }

    /// `code` 模式**不许**导入的目录（§7.3）。driver 的导入钩子读它。
    pub fn denied_import_roots(&self) -> Vec<PathBuf> {
        vec![self.staging_dir(), self.versions_dir()]
    }
}

/// 模块名的规范化：`toolbox.memos`、`memos` 都收，答 `memos`。
///
/// 收窄到一个标识符是**安全要求**而不是方便：模块名会拼进路径，一个 `../` 就能让
/// "启用一个模块"写到 toolbox 之外去。
pub fn normalize_module(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    let name = name.strip_prefix("toolbox.").unwrap_or(name);
    if name.is_empty() {
        return Err("模块名不能为空".into());
    }
    let first = name.chars().next().unwrap_or('0');
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!("{raw} 不是合法模块名：要以字母或下划线开头"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("{raw} 不是合法模块名：只允许字母、数字与下划线"));
    }
    Ok(name.to_string())
}

/// 模块在 Python 里的全名。
pub fn dotted(module: &str) -> String {
    format!("toolbox.{module}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_moves_with_the_code_and_with_the_lock_file() {
        let a = ModuleVersion::of("def f(): pass\n", None);
        let b = ModuleVersion::of("def f(): return 1\n", None);
        assert_ne!(a, b, "代码变了就是另一个版本");

        let locked = ModuleVersion::of("def f(): pass\n", Some(b"httpx==0.27.0"));
        assert_ne!(a, locked, "依赖锁定不同也是另一个版本");
        assert!(a.as_str().ends_with("-nolock"), "{a}");
        assert_eq!(
            locked,
            ModuleVersion::of("def f(): pass\n", Some(b"httpx==0.27.0")),
            "同样的输入给同样的版本"
        );
    }

    #[test]
    fn a_module_name_can_never_escape_the_toolbox_directory() {
        assert_eq!(normalize_module("memos").unwrap(), "memos");
        assert_eq!(normalize_module("toolbox.memos").unwrap(), "memos");
        assert_eq!(
            normalize_module(" toolbox.web_search ").unwrap(),
            "web_search"
        );
        for bad in ["../etc/passwd", "a/b", "a.b", "", "1abc", "a-b"] {
            assert!(normalize_module(bad).is_err(), "{bad} 不该被接受");
        }
    }

    #[test]
    fn the_layout_puts_candidates_where_python_can_never_import_them() {
        let layout = Layout::new("/home/u/.komo/toolbox");
        assert_eq!(
            layout.candidate_code("memos"),
            PathBuf::from("/home/u/.komo/toolbox/.staging/memos.py")
        );
        assert_eq!(
            layout.enabled_code("memos"),
            PathBuf::from("/home/u/.komo/toolbox/memos.py")
        );
        assert_eq!(layout.parent(), PathBuf::from("/home/u/.komo"));
        // `.staging` 不是合法标识符：`toolbox..staging.memos` 不是一个模块名。
        assert!(
            layout
                .denied_import_roots()
                .contains(&PathBuf::from("/home/u/.komo/toolbox/.staging"))
        );
    }
}
