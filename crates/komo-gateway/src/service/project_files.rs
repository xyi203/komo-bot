//! 项目规范文件（`AGENTS.md` / `CLAUDE.md`）：从一条 Run 的工作目录往上找（`docs/agent.md`
//! §10）。受理时找一次，正文冻结进 `payloads/`（§4.3），恢复时按引用读回、不再找。
//!
//! 规则照 pi（`coding-agent/src/core/resource-loader.ts` 的 `loadProjectContextFiles`）：
//!
//! - **一层最多一份**：按 [`CANDIDATES`] 的顺序，第一个是普通文件（跟随符号链接）且读得
//!   出来的就是这一层的，同层其余的不读——仓库里 `CLAUDE.md` 常常就是指向 `AGENTS.md`
//!   的链接，两份都读是同一份规范进两遍；
//! - **一直走到根**：不停在 git 仓库根或家目录，外层目录的约定对里面同样适用；
//! - **外层在前、工作目录那一层在最后**：越近越具体，冲突时以靠后的为准（提示里也这么说）。
//!
//! 与 pi 不同的两处：没有全局文件（Profile 的 `instructions` 就是那一份），不做 git
//! worktree 去重。读不出来的文件记一条 warn、换下一个候选，不让受理失败；不是 UTF-8 的
//! 字节按替换字符读。

use std::path::Path;

use komo_kernel::traits::{LedgerError, StoreError};
use komo_kernel::types::agent::ProjectFile;
use komo_kernel::types::refs::PayloadRef;
use komo_store::PayloadStore;
use komo_store::db::store_to_ledger;

/// 每一层按这个顺序找，第一个就是这一层的。`AGENTS.override.md` 是"本地覆盖、不进仓库"
/// 的那一份。
const CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// 从 `workspace`（含它自己）往上找到的规范文件，外层在前。
///
/// `workspace` 应当是解析过的真实路径；相对路径一个都不找——往上走到空路径就成了"网关
/// 进程的当前目录"，那不是这条 Run 的目录。
pub(crate) fn discover(workspace: &Path) -> Vec<ProjectFile> {
    if !workspace.is_absolute() {
        return Vec::new();
    }
    let mut found: Vec<ProjectFile> = workspace.ancestors().filter_map(in_dir).collect();
    found.reverse();
    found
}

/// 这一层的那一份（[`CANDIDATES`] 里第一个读得出来的普通文件）。
fn in_dir(dir: &Path) -> Option<ProjectFile> {
    CANDIDATES.iter().find_map(|name| {
        let path = dir.join(name);
        if !path.is_file() {
            return None;
        }
        match std::fs::read(&path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                Some(ProjectFile {
                    path: path.to_string_lossy().into_owned(),
                    content: text.strip_prefix('\u{feff}').unwrap_or(&text).to_string(),
                })
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "项目规范文件读不出来，换下一个候选"
                );
                None
            }
        }
    })
}

/// 冻结：找到的那些存进 `payloads/`，返回引用。一个都没有就是 `None`（快照里不留这一格）。
pub(crate) async fn freeze(
    payloads: &PayloadStore,
    files: &[ProjectFile],
) -> Result<Option<PayloadRef>, StoreError> {
    if files.is_empty() {
        return Ok(None);
    }
    Ok(Some(payloads.put_json(&files, None).await?))
}

/// 按引用读回冻结的那一份。读不出来或不是预期的 JSON = 会话缺内容，由调用方停下来
/// 报告——现场再找一遍就是"恢复出来的 Run 换了一副面孔"。
pub(crate) async fn thaw(
    payloads: &PayloadStore,
    reference: &PayloadRef,
) -> Result<Vec<ProjectFile>, LedgerError> {
    payloads.open_json(reference).await.map_err(store_to_ledger)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// 临时目录在真实路径里（macOS 的 `/var` 是链接），外面那几层的文件不归测试管：
    /// 断言只看临时目录里面的。
    fn under(root: &Path, files: Vec<ProjectFile>) -> Vec<(PathBuf, String)> {
        files
            .into_iter()
            .map(|file| (PathBuf::from(file.path), file.content))
            .filter(|(path, _)| path.starts_with(root))
            .map(|(path, content)| (path.strip_prefix(root).unwrap().to_path_buf(), content))
            .collect()
    }

    #[test]
    fn walks_up_to_the_root_outermost_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = komo_runtime::tools::paths::real_root(dir.path());
        let inner = root.join("repo").join("crate");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(root.join("AGENTS.md"), "外层").unwrap();
        std::fs::write(root.join("repo").join("CLAUDE.md"), "仓库").unwrap();
        std::fs::write(inner.join("AGENTS.md"), "子目录").unwrap();

        assert_eq!(
            under(&root, discover(&inner)),
            vec![
                (PathBuf::from("AGENTS.md"), "外层".to_string()),
                (PathBuf::from("repo/CLAUDE.md"), "仓库".to_string()),
                (PathBuf::from("repo/crate/AGENTS.md"), "子目录".to_string()),
            ],
            "一路走到根，外层在前，工作目录那一层在最后"
        );
    }

    #[test]
    fn one_file_per_directory_in_candidate_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = komo_runtime::tools::paths::real_root(dir.path());
        std::fs::write(root.join("AGENTS.md"), "规范").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "另一份").unwrap();
        std::fs::write(root.join("AGENTS.override.md"), "本地覆盖").unwrap();

        assert_eq!(
            under(&root, discover(&root)),
            vec![(PathBuf::from("AGENTS.override.md"), "本地覆盖".to_string())],
            "同一层只取第一个候选，其余不读"
        );
    }

    #[test]
    fn skips_non_files_and_strips_the_bom() {
        let dir = tempfile::tempdir().unwrap();
        let root = komo_runtime::tools::paths::real_root(dir.path());
        // 叫 `AGENTS.md` 的目录不算：换下一个候选。
        std::fs::create_dir(root.join("AGENTS.md")).unwrap();
        std::fs::write(root.join("CLAUDE.md"), "\u{feff}正文").unwrap();

        assert_eq!(
            under(&root, discover(&root)),
            vec![(PathBuf::from("CLAUDE.md"), "正文".to_string())]
        );
    }

    #[test]
    fn a_relative_workspace_finds_nothing() {
        assert!(discover(Path::new("relative/dir")).is_empty());
    }

    #[tokio::test]
    async fn freezes_into_payloads_and_thaws_back() {
        let dir = tempfile::tempdir().unwrap();
        let payloads = PayloadStore::new(komo_store::SessionPaths::new(
            dir.path(),
            &komo_kernel::types::ids::SessionId::from_raw("s-1"),
        ));
        assert_eq!(
            freeze(&payloads, &[]).await.unwrap(),
            None,
            "没有就不留引用"
        );

        let files = vec![ProjectFile {
            path: "/repo/AGENTS.md".into(),
            content: "先读再改。".into(),
        }];
        let reference = freeze(&payloads, &files).await.unwrap().expect("有引用");
        assert_eq!(thaw(&payloads, &reference).await.unwrap(), files);
    }
}
