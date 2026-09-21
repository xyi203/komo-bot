//! Run 的能力面：这次运行允许调用哪些工具（§4 末）。
//!
//! 它是"给模型看的 schema"与"执行时查找的表"的同一个来源。两边各拿一份就会出现
//! "schema 里没有这个名字、执行器却查得到"，而模型只要拼出那个名字就越过了能力边界
//! ——所以执行器**一次都不回退**到"手里正好装着哪些工具"。

use serde::{Deserialize, Serialize};

/// 一次 Run 允许调用的工具集合。
///
/// 空集是合法的（这次运行一个工具都不给）；**不要**把它解释成"全部"——那正是这类边界
/// 最容易出的错，而错法是把"没配"读成"随便用"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentSurface {
    /// 允许的工具名。**顺序就是交给模型的 schema 顺序**。
    tools: Vec<String>,
}

impl AgentSurface {
    /// 从工具名建一份能力面。重复的名字只留第一次出现的那个位置。
    pub fn new(tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut names: Vec<String> = Vec::new();
        for tool in tools {
            let tool = tool.into();
            if !names.contains(&tool) {
                names.push(tool);
            }
        }
        Self { tools: names }
    }

    /// 这个名字在不在这次运行的能力面里。
    pub fn allows(&self, tool: &str) -> bool {
        self.tools.iter().any(|name| name == tool)
    }

    pub fn names(&self) -> &[String] {
        &self.tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_keeps_the_order_and_drops_repeats() {
        let surface = AgentSurface::new(["read", "rg", "read"]);
        assert_eq!(surface.names(), ["read", "rg"]);
        assert!(surface.allows("rg"));
        assert!(!surface.allows("shell"));
        // 名字是精确匹配：多一个字符、变个大小写都不是同一个工具。
        assert!(!surface.allows("r"));
        assert!(!surface.allows("Read"));
    }

    #[test]
    fn an_empty_surface_allows_nothing() {
        let surface = AgentSurface::new(Vec::<String>::new());
        assert!(surface.names().is_empty());
        assert!(!surface.allows("read"));
    }
}
