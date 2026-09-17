//! 从模块正文里**静态**读出它声明了什么（§5.2、§8.6）。
//!
//! 为什么不 import 一下问 Python：`Tool::prepare` 的契约写着「**不能通过导入未知
//! Python 模块、运行命令等方式提前执行未审核代码**」。`prepare` 要回答"这个函数导出了
//! 吗"，而 import 一次就已经把模块顶层的代码跑过了——那正是 §7.3 要挡的东西。
//!
//! 所以这里是一个**很小的**解析器，只认三样写在顶层的字面量：
//!
//! - `__all__ = ["create", "get"]`：导出清单（§5.2「明确导出」）。
//! - `__komo_verify__ = "verify"`：与版本绑定的核对函数名（§8.6）。
//! - `__komo_env__ = ["MEMOS_TOKEN"]`：这个模块要的**凭证引用**——变量名，不是值
//!   （§5.3「地址和凭证引用通过配置传给已授权模块」；§7.2「凭证引用」进计划，
//!   凭证的值不进）。
//! - 模块开头的 docstring：给人看的说明（§5.3「README 与模块说明提供用法」）。
//!
//! 认不出来就答"没有"，**不猜**：一个动态拼出来的 `__all__` 在这里读作"没有声明导出"，
//! 于是 `call` 模式一个函数都够不到——保守的那一侧。真正执行时 driver 还会按运行期的
//! `__all__` 再查一遍，两边都过才调得到。

/// 模块正文里声明的那些东西。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Declarations {
    /// `__all__` 里列出的名字。空 = 没有声明导出。
    pub exports: Vec<String>,
    /// `__komo_verify__` 指的那个核对函数。
    pub verifier: Option<String>,
    /// `__komo_env__` 里的**变量名**。凭证的值一个都不在这里。
    pub env: Vec<String>,
    /// 模块开头的 docstring（第一段）。
    pub doc: Option<String>,
}

impl Declarations {
    pub fn exports(&self, function: &str) -> bool {
        self.exports.iter().any(|name| name == function)
    }
}

/// 解析一份模块正文。
pub fn declarations(code: &str) -> Declarations {
    Declarations {
        exports: string_list(code, "__all__"),
        verifier: string_value(code, "__komo_verify__"),
        env: string_list(code, "__komo_env__"),
        doc: docstring(code),
    }
}

/// `NAME = [...]`（可跨行）里的字符串字面量。
fn string_list(code: &str, name: &str) -> Vec<String> {
    let Some(rest) = assignment(code, name) else {
        return Vec::new();
    };
    let rest = rest.trim_start();
    let close = match rest.chars().next() {
        Some('[') => ']',
        Some('(') => ')',
        _ => return Vec::new(),
    };
    let Some(end) = rest.find(close) else {
        return Vec::new();
    };
    let inside = &rest[1..end];
    // **只认纯字面量清单**。`[name for name in dir() if ...]` 里也有引号，而把里面那个
    // `'_'` 当成一个导出名，就等于让一段没读懂的代码决定导出了什么。把字面量挖掉之后
    // 还剩下别的东西 → 这不是一张字面量清单，答"没有声明导出"。
    if !only_literals(inside) {
        return Vec::new();
    }
    literals(inside)
}

/// 挖掉所有字符串字面量之后，只剩下逗号与空白吗。
fn only_literals(text: &str) -> bool {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            for next in chars.by_ref() {
                if next == c {
                    break;
                }
            }
            continue;
        }
        if !c.is_whitespace() && c != ',' {
            return false;
        }
    }
    true
}

/// `NAME = "..."` 里的那个字符串。
fn string_value(code: &str, name: &str) -> Option<String> {
    let rest = assignment(code, name)?;
    let line = rest.lines().next()?;
    literals(line).into_iter().next()
}

/// 顶层（不缩进）`name = ` 之后的那一段。
fn assignment<'a>(code: &'a str, name: &str) -> Option<&'a str> {
    let mut offset = 0usize;
    for line in code.lines() {
        let start = offset;
        offset += line.len() + 1;
        if line.starts_with(char::is_whitespace) {
            continue; // 缩进的不是模块顶层的声明。
        }
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        // 回到整段正文上继续，`__all__` 可以跨行写。
        let consumed = line.len() - rest.len();
        return Some(&code[start + consumed..]);
    }
    None
}

/// 一段文本里的所有单引号 / 双引号字面量。
fn literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '"' && c != '\'' {
            continue;
        }
        let mut value = String::new();
        for next in chars.by_ref() {
            if next == c {
                out.push(value);
                break;
            }
            value.push(next);
        }
    }
    out
}

/// 模块开头的 docstring。
fn docstring(code: &str) -> Option<String> {
    let trimmed = code.trim_start();
    for quote in ["\"\"\"", "'''"] {
        if let Some(rest) = trimmed.strip_prefix(quote)
            && let Some(end) = rest.find(quote)
        {
            let body = rest[..end].trim();
            if !body.is_empty() {
                return Some(body.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE: &str = r#""""Memos 客户端。

写入返回 ID 与链接。
"""

__all__ = ["create", "get", "verify"]
__komo_verify__ = "verify"
__komo_env__ = ["MEMOS_BASE_URL", "MEMOS_TOKEN"]


def create(text):
    return text


def _private():
    __all__ = ["nope"]
    return 1
"#;

    #[test]
    fn the_exports_and_the_verifier_are_read_without_importing_anything() {
        let read = declarations(MODULE);
        assert_eq!(read.exports, vec!["create", "get", "verify"]);
        assert_eq!(read.verifier.as_deref(), Some("verify"));
        assert_eq!(read.env, vec!["MEMOS_BASE_URL", "MEMOS_TOKEN"]);
        assert!(read.doc.as_deref().unwrap().starts_with("Memos 客户端。"));
        assert!(read.exports("create"));
        assert!(!read.exports("delete"));
    }

    #[test]
    fn an_all_written_across_several_lines_still_reads() {
        let read = declarations("__all__ = [\n    'a',\n    'b',\n]\n");
        assert_eq!(read.exports, vec!["a", "b"]);
    }

    /// 一个动态拼出来的 `__all__` 读作"没有声明导出"——`call` 模式因此一个函数都够
    /// 不到，而不是猜一个清单出来。
    #[test]
    fn a_computed_all_reads_as_no_exports_rather_than_a_guess() {
        let read = declarations("__all__ = [name for name in dir() if not name.startswith('_')]\n");
        assert!(read.exports.is_empty(), "{:?}", read.exports);
        assert!(!read.exports("anything"));
    }

    /// 缩进的赋值是函数体里的局部变量，不是模块的声明。
    #[test]
    fn an_indented_assignment_is_not_a_module_declaration() {
        let read = declarations("def f():\n    __all__ = ['sneaky']\n    __komo_verify__ = 'x'\n");
        assert!(read.exports.is_empty());
        assert!(read.verifier.is_none());
    }

    #[test]
    fn a_module_without_declarations_says_so() {
        let read = declarations("def f():\n    return 1\n");
        assert_eq!(read, Declarations::default());
    }
}
