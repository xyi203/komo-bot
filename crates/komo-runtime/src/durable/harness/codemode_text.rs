//! codemode 结果的正文帮手（`docs/codemode.md` §5）：嵌套调用的摘要、失败首部、多段输出拼接。
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

pub(crate) fn failure_prefix(error: &str, calls: &[NestedCall], _names: &[String]) -> String {
    // JS 异常格式为首行 `ErrorName: message`，随后是 `    at ...` 栈帧；取第一条非空行作为错因。
    let reason = error
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(error);
    let mut text = format!("脚本出错：{}\n", truncate(reason, 1024));
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
    use super::{failure_prefix, join_outputs};

    #[test]
    fn failure_prefix_extracts_the_first_line_of_a_js_stack_trace() {
        let err =
            "ToolError: 没有叫 raed 的工具，最接近的是 tools.read\n    at <eval> (codemode.ts:2:7)";
        let prefix = failure_prefix(err, &[], &[]);
        assert!(
            prefix.starts_with("脚本出错：ToolError: 没有叫 raed 的工具，最接近的是 tools.read\n"),
            "{prefix}"
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
