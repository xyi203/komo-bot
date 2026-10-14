//! 状态行上"这一段对话花掉的 token"那一格的账（口径照 pi 的页脚）。
//!
//! `message.assistant` 的 `input_tokens` 是**整个提示**、含缓存读写（见
//! [`TokenUsage::input`]）；这里拆开记：`input` 只留没走缓存的那部分，缓存读、写各一个
//! 数，四个数加起来才是模型实际吃进去的。命中率只看**最近一轮**——它说的是"现在缓存还
//! 管不管用"，累计的比例会被前面几十轮摊平，看不出刚才那一下断了。
//!
//! [`TokenUsage::input`]: komo_kernel::types::model::TokenUsage::input

use komo_kernel::events::MessageAssistant;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenTally {
    /// 没走缓存的输入（`↑`）。
    pub input: u64,
    /// 输出（`↓`）。
    pub output: u64,
    /// 从前缀缓存读到的部分（`R`）。
    pub cache_read: u64,
    /// 这次写进前缀缓存的部分（`W`）。
    pub cache_write: u64,
    /// 最近一轮的（缓存读，整个提示）。那一轮没报缓存就是未知，不当成没命中。
    pub last_round: Option<(u64, u64)>,
}

impl TokenTally {
    /// 记上一轮。没报的数是未知，按零加——总数只会偏小，不会凭空多出来。
    pub fn add(&mut self, round: &MessageAssistant) {
        let prompt = round.input_tokens.unwrap_or(0);
        let read = round.cache_read_tokens.unwrap_or(0);
        let write = round.cache_write_tokens.unwrap_or(0);
        self.input += prompt.saturating_sub(read + write);
        self.output += round.output_tokens.unwrap_or(0);
        self.cache_read += read;
        self.cache_write += write;
        self.last_round = match (round.input_tokens, round.cache_read_tokens) {
            (Some(prompt), Some(read)) if prompt > 0 => Some((read, prompt)),
            _ => None,
        };
    }

    /// 最近一轮的缓存命中率（百分比）。这一段对话从没碰过缓存时不给——0% 在那里说的
    /// 是"缓存坏了"，而真相是这家模型压根不报。
    pub fn hit_rate(&self) -> Option<f64> {
        if self.cache_read == 0 && self.cache_write == 0 {
            return None;
        }
        let (read, prompt) = self.last_round?;
        Some(read as f64 / prompt as f64 * 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(input: u64, output: u64, read: Option<u64>, write: Option<u64>) -> MessageAssistant {
        MessageAssistant {
            round: 1,
            text: None,
            text_ref: None,
            tool_calls: vec![],
            provider_blocks: None,
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: read,
            cache_write_tokens: write,
        }
    }

    #[test]
    fn the_up_arrow_counts_only_what_missed_the_cache() {
        let mut tally = TokenTally::default();
        tally.add(&round(1_510, 7, Some(0), Some(1_500)));
        tally.add(&round(1_550, 9, Some(1_500), Some(30)));
        assert_eq!(
            (
                tally.input,
                tally.output,
                tally.cache_read,
                tally.cache_write
            ),
            (30, 16, 1_500, 1_530)
        );
    }

    #[test]
    fn the_hit_rate_is_the_latest_round_not_the_average() {
        let mut tally = TokenTally::default();
        tally.add(&round(1_000, 1, Some(0), Some(1_000)));
        tally.add(&round(1_000, 1, Some(500), Some(0)));
        assert_eq!(tally.hit_rate(), Some(50.0));
    }

    #[test]
    fn a_model_that_never_reports_the_cache_shows_no_hit_rate() {
        let mut tally = TokenTally::default();
        tally.add(&round(120, 30, None, None));
        assert_eq!(tally.input, 120);
        assert_eq!(tally.hit_rate(), None);
    }

    #[test]
    fn a_round_without_a_cache_report_leaves_the_hit_rate_unknown() {
        let mut tally = TokenTally::default();
        tally.add(&round(1_000, 1, Some(900), Some(0)));
        tally.add(&round(1_000, 1, None, None));
        assert_eq!(tally.hit_rate(), None, "没报是未知，不是 0%");
    }
}
