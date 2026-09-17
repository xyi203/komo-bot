//! 随 komo 一起发的 toolbox 模块。
//!
//! 只有一个：`memos`（§5.5）。它是内置的而不是"让模型自己写一个"，理由在 §5.5 里：
//! 用户主动保存的记录的**原文**存在 Memos，它是一条要长期可信的链路，而一个每次会话
//! 都可能被重写的模块给不出这种可信度。
//!
//! 内置模块不走候选流程（`Toolbox::install_builtin`）——它的审核发生在 komo 自己的代码
//! 评审里，版本 = 内容哈希。**已经有同名模块就不装**：操作者或模型改过的那一份是他们
//! 的，一次升级不该悄悄盖掉它。

use time::OffsetDateTime;

use super::{Enabled, Toolbox, ToolboxError};

/// Memos 客户端正文。
pub const MEMOS: &str = include_str!("memos.py");
/// 它自带的测试：对一个本地假 Memos 跑，不碰真实例。
pub const MEMOS_TEST: &str = include_str!("test_memos.py");

/// 这个模块用到的凭证引用。**名字**由模块的 `__komo_env__` 声明、进计划的
/// `ResourceRef`；**值**由 Gateway 在每次 spawn 时从 `.env` 按名解析进那一个子进程
/// （`python_runtime::SecretResolver`）——不进提示词、不进计划、不进 Gateway 自己的
/// 进程环境（§5.3、§7.2）。
pub const MEMOS_ENV: &[&str] = &["MEMOS_BASE_URL", "MEMOS_TOKEN"];

/// 首次启动时把内置模块装进 toolbox。答"这一次装了哪些"。
pub fn install(toolbox: &Toolbox, now: OffsetDateTime) -> Result<Vec<Enabled>, ToolboxError> {
    let mut installed = Vec::new();
    if let Some(enabled) = toolbox.install_builtin("memos", MEMOS, Some(MEMOS_TEST), now)? {
        installed.push(enabled);
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolbox::source;

    /// 内置模块必须满足 `call` 模式的两条要求，否则装进去也调不到。
    #[test]
    fn the_builtin_memos_module_declares_what_call_mode_needs() {
        let read = source::declarations(MEMOS);
        assert_eq!(
            read.exports,
            vec!["create", "get", "search", "update", "delete", "verify"]
        );
        assert_eq!(read.verifier.as_deref(), Some("verify"));
        // 核对函数自己也要是导出的——driver 只调 `__all__` 里的名字。
        assert!(read.exports("verify"));
        assert!(read.doc.is_some(), "模块说明是模型先读的那一层（§5.3）");
    }

    /// 凭证只从被点名的变量来，**代码里不能有任何写死的值**（§5.3）。
    #[test]
    fn the_builtin_module_carries_no_credentials_of_its_own() {
        for name in MEMOS_ENV {
            assert!(
                MEMOS.contains(name),
                "{name} 该由 __komo_env__ 点名并从中读取"
            );
        }
        assert!(
            !MEMOS.contains("Bearer ey") && !MEMOS.contains("token = \""),
            "模块正文里不能写死凭证"
        );
    }

    #[test]
    fn installing_twice_leaves_the_second_one_alone() {
        let dir = tempfile::tempdir().unwrap();
        let toolbox = Toolbox::new(dir.path().join("toolbox"));
        let now = OffsetDateTime::UNIX_EPOCH;

        let first = install(&toolbox, now).unwrap();
        assert_eq!(first.len(), 1);
        assert!(first[0].builtin);

        // 操作者改过这一份。
        let path = toolbox.layout().enabled_code("memos");
        std::fs::write(&path, "__all__ = ['create']\n").unwrap();

        let second = install(&toolbox, now).unwrap();
        assert!(second.is_empty(), "已经有了就不动它");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "__all__ = ['create']\n"
        );
    }
}
