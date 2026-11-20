//! codemode 结果的正文帮手（`docs/codemode.md` §5）：嵌套调用的摘要、失败提示、多段输出拼接。
//! 脚本本身由 `durable::harness::codemode` 跑，这里只管交给模型的那段文字。

use komo_kernel::types::refs::ToolResultStatus;
use serde::Serialize;
use serde_json::Value;

/// 摘要里每次调用的参数最多留这么长。
pub(crate) const ARGS_PREVIEW: usize = 512;
/// 正文只列少量子调用；完整清单保存在 result.calls，由统一投影给出完整结果入口。
const SUMMARY_CALLS: usize = 8;

/// 一次内层调用的摘要（外层 `output.json` 的 `result.calls`）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct NestedCall {
    pub(crate) id: String,
    pub(crate) tool: String,
    pub(crate) args: String,
    /// 收窄过的参数本身（`komo_kernel::protocol::nested::bounded_args`），给界面画子调用那一行；
    /// `args` 是截断的文字，可能解不开。
    pub(crate) arguments: Value,
    /// 结果不明（宿主没等到确定的结果，或前面的致命错误拦下了它）。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) uncertain: bool,
    /// 宿主是否正常返回；不等同于工具结果的 status，保留旧字段含义。
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<ToolResultStatus>,
    /// 已发布子输出的可读入口，来自 journal 里的原尝试（恢复时不换成当前 attempt）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output: Option<String>,
    pub(crate) elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

pub(crate) fn summary(calls: &[NestedCall]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for call in calls {
        match counts.iter_mut().find(|(tool, _)| *tool == call.tool) {
            Some((_, count)) => *count += 1,
            None => counts.push((&call.tool, 1)),
        }
    }
    let failed = calls
        .iter()
        .filter(|call| !call.ok || call.status == Some(ToolResultStatus::Failed))
        .count();
    let unknown = calls
        .iter()
        .filter(|call| call.status == Some(ToolResultStatus::Uncertain))
        .count();
    let listed = counts
        .iter()
        .take(SUMMARY_CALLS)
        .map(|(tool, count)| format!("{tool}×{count}"))
        .collect::<Vec<_>>()
        .join("、");
    let mut text = match failed {
        0 => format!("调用了 {} 次工具：{listed}", calls.len()),
        n => format!("调用了 {} 次工具：{listed}；{n} 次失败", calls.len()),
    };
    if unknown > 0 {
        text.push_str(&format!("；{unknown} 次结果不明"));
    }
    if counts.len() > SUMMARY_CALLS {
        text.push_str(&format!(
            "；另 {} 种工具，完整清单见 result.calls",
            counts.len() - SUMMARY_CALLS
        ));
    }
    text
}

pub(crate) fn failure_prefix(error: &str, calls: &[NestedCall], names: &[String]) -> String {
    let reason = error
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(error);
    let mut text = format!("脚本出错：{}\n", truncate(reason, 1024));
    if let Some(hint) = diagnostic(reason).or_else(|| unknown_tool(reason, names)) {
        text.push_str(&format!("修复提示：{hint}\n"));
    }
    if !calls.is_empty() {
        text.push_str(&format!("（{}）\n", summary(calls)));
        if calls.iter().any(|call| call.output.is_some()) {
            text.push_str(
                "已执行的子调用不会因脚本报错回滚；先核对原始输出，再继续剩余步骤，勿整段重做。\n",
            );
        }
        for call in calls.iter().take(SUMMARY_CALLS) {
            let status = match call.status {
                Some(ToolResultStatus::Completed) => "completed",
                Some(ToolResultStatus::Failed) => "failed",
                Some(ToolResultStatus::Uncertain) => "uncertain",
                None => "未返回工具结果",
            };
            text.push_str(&format!("- {} {}：{status}", call.id, call.tool));
            if let Some(output) = &call.output {
                text.push_str(&format!("；原始输出：{output}"));
            }
            text.push('\n');
        }
        if calls.len() > SUMMARY_CALLS {
            text.push_str(&format!(
                "另 {} 次子调用未展开，完整清单与引用见本次完整输出的 result.calls。\n",
                calls.len() - SUMMARY_CALLS
            ));
        }
    }
    text
}

/// 只匹配已经复现的异常，不根据脚本文字猜错因，也不重写或重跑代码。
fn diagnostic(reason: &str) -> Option<String> {
    // Monty 1.6.0 起有 `datetime.fromtimestamp` 与 `json.load` / `dump`（1.1.0 没有，原先这里教绕法）；
    // `utcfromtimestamp` 仍没有（CPython 3.12 起也弃用了），指向带时区的写法。
    if reason
        == "AttributeError: type object 'datetime.datetime' has no attribute 'utcfromtimestamp'"
    {
        return Some("Monty 没有 utcfromtimestamp：写 datetime.fromtimestamp(ts, timezone.utc)（from datetime import datetime, timezone）；固定 UTC+8 用 timezone(timedelta(hours=8))。".into());
    }
    for module in [
        "json",
        "re",
        "time",
        "datetime",
        "math",
        "collections",
        "itertools",
    ] {
        if reason.starts_with(&format!("NameError: name '{module}' is not defined")) {
            return Some(format!(
                "每次调用都是独立脚本，请在本段先 import {module}。"
            ));
        }
    }
    None
}

/// 脚本里写错工具名（`tools.<名字>` / `atools.<名字>`）：Monty 不支持 `__getattr__`，只给一句裸的
/// AttributeError（`komo-tools` 的 `codemode/prelude.py`）。2026-11 的统计里这类失败都出在 MCP
/// 名字的转写上（`mcp__服务__工具` 少写、多写下划线，或丢了前缀），所以按"去掉 `_` `-` 之后
/// 相同或互相包含"优先、再按编辑距离挑最像的几个名字给回去，模型下一轮就能照着改。
fn unknown_tool(reason: &str, names: &[String]) -> Option<String> {
    let wrong = reason
        .strip_prefix("AttributeError: 'Tools' object has no attribute '")?
        .strip_suffix('\'')?;
    let compact = |name: &str| {
        name.chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let wanted = compact(wrong);
    // 脚本里的写法：工具名里的 `-` 换成 `_`（prelude 的 `setattr`）。
    // 排序键：(不是包含关系, 差了几个字符, 名字)。
    let mut ranked: Vec<(bool, usize, String)> = names
        .iter()
        .map(|name| name.replace('-', "_"))
        .filter_map(|name| {
            let have = compact(&name);
            if have.contains(&wanted) || wanted.contains(&have) {
                return Some((false, have.len().abs_diff(wanted.len()), name));
            }
            let distance = levenshtein(&wanted, &have);
            // 太远的不算"像"：最多改动名字的一半。
            (distance <= wanted.chars().count().div_ceil(2)).then_some((true, distance, name))
        })
        .collect();
    ranked.sort();
    let close: Vec<String> = ranked
        .into_iter()
        .take(3)
        .map(|(_, _, name)| format!("tools.{name}"))
        .collect();
    Some(if close.is_empty() {
        format!("没有叫 {wrong} 的工具；工具名里的 - 写成 _，能调的名字见 TOOLS。")
    } else {
        format!(
            "没有叫 {wrong} 的工具，最接近的是 {}；工具名里的 - 写成 _，能调的名字见 TOOLS。",
            close.join("、")
        )
    })
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = (previous + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(current + 1);
            previous = current;
        }
    }
    row[b.len()]
}

/// 多段 `text()` 拼起来：两段以上时每段前加 `==> text N/M <==`（学 pi 1.1.0）——直接用换行
/// 拼，模型分不清一段多行输出和两段输出的边界。只有一段时原样给，不多一个字。
pub(crate) fn join_outputs(outputs: &[String]) -> String {
    if outputs.len() <= 1 {
        return outputs.join("\n");
    }
    let total = outputs.len();
    outputs
        .iter()
        .enumerate()
        .map(|(index, output)| format!("==> text {}/{total} <==\n{output}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…（截断）", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::join_outputs;

    #[test]
    fn diagnostics_do_not_guess_from_unrelated_names_or_attributes() {
        for error in [
            "AttributeError: type object 'Widget' has no attribute 'utcfromtimestamp'",
            "NameError: name 'json_data' is not defined",
            "KeyError: json",
            "AttributeError: module 'json' has no attribute 'loader'",
        ] {
            assert!(super::diagnostic(error).is_none(), "{error}");
        }
    }

    #[test]
    fn utcfromtimestamp_points_at_fromtimestamp_with_a_timezone() {
        let hint = super::diagnostic(
            "AttributeError: type object 'datetime.datetime' has no attribute 'utcfromtimestamp'",
        )
        .expect("复现过的异常");
        assert!(
            hint.contains("datetime.fromtimestamp(ts, timezone.utc)"),
            "{hint}"
        );
    }

    #[test]
    fn a_mistyped_tool_name_points_at_the_closest_names() {
        let names: Vec<String> = [
            "bash",
            "read",
            "rg",
            "komo",
            "mcp__memos__search_memos",
            "mcp__memos__create_memo",
            "mcp__dev-radius__search_issues",
        ]
        .map(String::from)
        .to_vec();
        let hint = |wrong: &str| {
            super::unknown_tool(
                &format!("AttributeError: 'Tools' object has no attribute '{wrong}'"),
                &names,
            )
            .expect("工具名写错一定有提示")
        };
        // 下划线少写 / 丢了前缀：最像的排第一。
        for wrong in [
            "mcp_memos_search_memos",
            "memos__search_memos",
            "mcp__memos__search",
        ] {
            let text = hint(wrong);
            assert!(
                text.contains("最接近的是 tools.mcp__memos__search_memos"),
                "{wrong}：{text}"
            );
        }
        // 名字里的 - 在脚本里是 _，提示给脚本里的写法。
        assert!(
            hint("mcp__dev_radius__search_issue").contains("tools.mcp__dev_radius__search_issues")
        );
        // 拼错一两个字母。
        assert!(hint("raed").contains("最接近的是 tools.read"));
        // 毫不相干：只指向 TOOLS，不硬猜。
        let far = hint("send_email");
        assert!(!far.contains("最接近"), "{far}");
        assert!(far.contains("TOOLS"), "{far}");
        // 别的对象的 AttributeError 不是工具名写错。
        assert!(
            super::unknown_tool(
                "AttributeError: 'dict' object has no attribute 'read'",
                &names
            )
            .is_none()
        );
    }

    #[test]
    fn a_single_output_is_left_as_is() {
        assert_eq!(join_outputs(&["a\nb".to_string()]), "a\nb");
        assert_eq!(join_outputs(&[]), "");
    }

    #[test]
    fn several_outputs_are_headed_with_their_position() {
        let joined = join_outputs(&["a\nb".to_string(), "c".to_string()]);
        assert_eq!(joined, "==> text 1/2 <==\na\nb\n==> text 2/2 <==\nc");
    }
}
