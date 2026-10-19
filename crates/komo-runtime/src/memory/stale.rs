//! 注入之前核对一遍记忆里提到的路径（§9.4 之后补的一步）。
//!
//! 记忆是某一刻的观察，机器上的东西会挪走、删掉：一条「技能在 ~/.komo/skills/ha-control」
//! 在那个目录删掉之后还原样注入，模型就会照着一条过期的线索去做。这里只核对**能确定在
//! 本机**的路径（`~/…` 与家目录下的绝对路径，§9.6「路径核对」），确实不在就在正文后面注
//! 一句，不删、不改库里的条目——判断"这条该不该忘"是操作者的事（`komo memory forget`）。
//!
//! `/api/cart` 是 HTTP 路由，`/app/config` 可能在容器里，`/etc/nginx/...` 可能在远端那台
//! 机器：宿主磁盘上找不到它们什么也证明不了，所以一律算"不知道"，不注、不提议 contested。

use std::path::{Path, PathBuf};

use komo_kernel::types::memory::MemoryItem;

/// 每条正文后面补上"其中哪些路径已经不在了"。只改这一次注入用的副本。
pub fn mark_missing_paths(items: Vec<MemoryItem>, home: Option<&Path>) -> Vec<MemoryItem> {
    items
        .into_iter()
        .map(|mut item| {
            let missing = missing_paths(&item.content, home);
            if !missing.is_empty() {
                item.content = format!(
                    "{}（注：{} 现在已不存在，这条可能过期）",
                    item.content,
                    missing.join("、")
                );
            }
            item
        })
        .collect()
}

/// 正文里提到、能确定在本机、而且此刻确实不在的路径。注入时标注与 sweep 时判 contested
/// 共用它。判不了归属、或者探测本身出错的，都不在结果里——"不知道"不是"不存在"。
pub fn missing_paths(content: &str, home: Option<&Path>) -> Vec<String> {
    paths_in(content)
        .into_iter()
        .filter(|raw| local(raw, home).is_some_and(|path| absent(&path)))
        .collect()
}

/// 能确定在本机的那个路径；判不了就 `None`。
///
/// 只认两种：`~/…`，和落在家目录下的绝对路径。没有家目录时两种都判不了。
fn local(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let home = home?;
    match raw.strip_prefix("~/") {
        Some(rest) => Some(home.join(rest)),
        None => {
            let path = Path::new(raw);
            path.starts_with(home).then(|| path.to_path_buf())
        }
    }
}

/// 只有 `NotFound` 才算不存在；没权限、I/O 出错都是"不知道"。
fn absent(path: &Path) -> bool {
    matches!(std::fs::metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

/// 正文里像绝对路径的片段。前一个字符是路径字符或 `:` `}` `)` `]` `>` 的不算（`a/b`、
/// `https://…`、`{HASS_URL}/api/states`），后面紧跟 `<` `{` 的是占位写法，也不算。
fn paths_in(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let starts = match chars[index] {
            '/' => chars.get(index + 1).is_some_and(|c| *c != '/'),
            '~' => chars.get(index + 1) == Some(&'/'),
            _ => false,
        };
        let boundary = index == 0
            || !(path_char(chars[index - 1])
                || matches!(chars[index - 1], ':' | '}' | ')' | ']' | '>'));
        if !(starts && boundary) {
            index += 1;
            continue;
        }
        let end = (index..chars.len())
            .find(|&at| !path_char(chars[at]))
            .unwrap_or(chars.len());
        if matches!(chars.get(end), Some('<' | '{')) {
            index = end.max(index + 1);
            continue;
        }
        let raw: String = chars[index..end].iter().collect();
        let raw = raw.trim_end_matches(['.', ',']).to_string();
        // 至少两级：`/new`、`/v1` 这类是命令或 URL 片段，不是机器上的路径。
        let segments = raw
            .trim_start_matches('~')
            .split('/')
            .filter(|part| !part.is_empty())
            .count();
        if segments >= 2 && !out.contains(&raw) {
            out.push(raw);
        }
        index = end.max(index + 1);
    }
    out
}

fn path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~' | '+' | '@')
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::MemoryId;
    use komo_kernel::types::memory::{
        Confirmation, ExtractionMetadata, MemoryKind, MemoryScope, MemoryState, Provenance,
    };
    use time::OffsetDateTime;

    fn item(content: &str) -> MemoryItem {
        MemoryItem {
            id: MemoryId::from_raw("m-1"),
            revision: 1,
            content: content.into(),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Personal,
            provenance: Provenance::ToolObservation,
            confirmation: Confirmation::Unconfirmed,
            state: MemoryState::Active,
            evidence: vec![],
            observed_at: OffsetDateTime::UNIX_EPOCH,
            valid_until: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            extraction: ExtractionMetadata::new("memory-model", None, "v1"),
            usage: Default::default(),
            supersedes: None,
            superseded_by: None,
        }
    }

    #[test]
    fn absolute_paths_are_picked_out_but_urls_and_commands_are_not() {
        assert_eq!(
            paths_in("技能在 ~/.komo/skills/ha-control，可执行文件在 /home/u/.local/bin/。"),
            vec!["~/.komo/skills/ha-control", "/home/u/.local/bin/"]
        );
        assert!(paths_in("走 https://api.deepseek.com/v1 或者 /new 命令，a/b/c 不算").is_empty());
        assert!(
            paths_in("GET {HASS_URL}/api/states 与 POST /api/services/<domain>/<service>")
                .is_empty()
        );
    }

    #[test]
    fn a_missing_path_is_noted_and_an_existing_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".komo/toolbox")).unwrap();
        let marked = mark_missing_paths(
            vec![
                item("能力在 ~/.komo/toolbox"),
                item("技能在 ~/.komo/skills/ha-control"),
            ],
            Some(dir.path()),
        );
        assert_eq!(marked[0].content, "能力在 ~/.komo/toolbox");
        assert_eq!(
            marked[1].content,
            "技能在 ~/.komo/skills/ha-control（注：~/.komo/skills/ha-control 现在已不存在，这条可能过期）"
        );
    }

    #[test]
    fn routes_and_paths_outside_home_are_unknown_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let content = "购物车走 /api/cart，评论批量接口 /api/internal/v1/comment/batch，\
                       容器里是 /app/config/settings.yaml，远端的 /etc/komo-nowhere/nginx.conf";
        assert_eq!(paths_in(content).len(), 4, "仍然认得出这些片段");
        assert!(
            missing_paths(content, Some(dir.path())).is_empty(),
            "不在家目录下的路径判不了归属，不能说它不存在"
        );
    }

    #[test]
    fn an_absolute_path_under_home_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("code/kept")).unwrap();
        let kept = dir.path().join("code/kept").display().to_string();
        let gone = dir.path().join("code/gone").display().to_string();
        assert_eq!(
            missing_paths(&format!("在 {kept} 与 {gone} 下"), Some(dir.path())),
            vec![gone]
        );
    }

    #[test]
    fn without_a_home_nothing_can_be_judged() {
        assert!(missing_paths("技能在 ~/.komo/skills/gone", None).is_empty());
    }
}
