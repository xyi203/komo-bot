//! 切点：压缩时哪一段收成摘要、从哪里开始原样保留。

use crate::types::ids::Seq;

/// 粗估 token 数：UTF-8 字节数 / 4，向上取整。
///
/// 对中日韩文字会**低估**（一个汉字 3 字节，实际常常是 1–2 个 token）。压缩的账看的是
/// 比例——摘要比原文短多少、上下文每次涨多少——同一把尺子量两边，偏差大体抵消。
pub fn estimate_tokens(text: &str) -> u64 {
    text.len().div_ceil(4) as u64
}

/// 从末尾往前数，保留至少 `keep_recent_tokens` 的近期内容，返回第一条保留下来的 seq。
///
/// `candidates` 按日志顺序给出 `(seq, token 数, 是不是一轮助手回复的开头)`。切点只能落
/// 在助手回复的开头：一轮的工具调用和它的结果都在这条回复之后，于是调用永远不会和它
/// 的结果被切开。凑够了 token 却不在轮次开头，就继续往前找最近的一个——宁可多留。
///
/// 切点之前什么都没有（整段都要保留，或者根本凑不够）时返回 `None`。
pub fn find_cut(candidates: &[(Seq, u64, bool)], keep_recent_tokens: u64) -> Option<Seq> {
    let mut kept = 0u64;
    for (index, &(seq, tokens, round_start)) in candidates.iter().enumerate().rev() {
        kept = kept.saturating_add(tokens);
        if kept >= keep_recent_tokens && round_start {
            return (index > 0).then_some(seq);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_bytes_over_four_rounded_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("汉字"), 2, "6 字节");
    }

    /// 用户 · 助手(调用) · 结果 · 助手(调用) · 结果 · 助手。
    fn rounds() -> Vec<(Seq, u64, bool)> {
        vec![
            (Seq(1), 100, false),
            (Seq(2), 50, true),
            (Seq(3), 400, false),
            (Seq(4), 50, true),
            (Seq(5), 300, false),
            (Seq(6), 80, true),
        ]
    }

    #[test]
    fn the_cut_keeps_at_least_the_recent_budget() {
        let candidates = rounds();
        for keep in [0, 80, 81, 300, 430, 431, 880] {
            let Some(cut) = find_cut(&candidates, keep) else {
                panic!("keep = {keep} 应当切得出来");
            };
            let kept: u64 = candidates
                .iter()
                .filter(|(seq, ..)| *seq >= cut)
                .map(|(_, tokens, _)| tokens)
                .sum();
            assert!(kept >= keep, "keep = {keep}，只留了 {kept}");
        }
        assert_eq!(find_cut(&candidates, 80), Some(Seq(6)));
        assert_eq!(
            find_cut(&candidates, 81),
            Some(Seq(4)),
            "凑够了不在轮次开头就往前找"
        );
    }

    #[test]
    fn the_cut_never_separates_a_call_from_its_result() {
        let candidates = rounds();
        for keep in 0..1_000 {
            if let Some(cut) = find_cut(&candidates, keep) {
                let at = candidates.iter().find(|(seq, ..)| *seq == cut).unwrap();
                assert!(at.2, "keep = {keep} 切在了 {cut:?}，不是助手回复的开头");
            }
        }
    }

    #[test]
    fn nothing_before_the_cut_means_no_cut() {
        let candidates = rounds();
        assert_eq!(find_cut(&candidates, 900), None, "要留的比全部还多");
        assert_eq!(find_cut(&[], 0), None);
        let starts_with_assistant = [(Seq(1), 10, true), (Seq(2), 10, false)];
        assert_eq!(find_cut(&starts_with_assistant, 15), None);
        let no_round_start = [(Seq(1), 10, false), (Seq(2), 10, false)];
        assert_eq!(find_cut(&no_round_start, 0), None);
    }
}
