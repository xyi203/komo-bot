//! 旧协议里 `RunId` / `Seq` 与新存储记录 ID 的互换（D5 契约，`docs/storage-durable-d5.md` §1 / §2）。
//!
//! 协议在 D5 冻结（`komo_kernel::protocol` 不改，客户端不动），所以 gateway 从 entry / submission
//! 合成旧事件词汇时要用旧的 ID 形状：
//!
//! | 旧 | 新 |
//! |---|---|
//! | `RunId` | 这条 run **首个输入 submission 的十进制 ID** |
//! | `Seq`（SSE `id:`、`Event.seq`、`EventPage.next`、`SubmitRunResponse.seq`） | `(记录 ID << 12) \| k` |
//!
//! `Seq` 里的"记录 ID"是 entry 或 submission 的全局单调 ID（同一个数字命名空间，不会撞），`k` 是同一
//! 条记录产出的第几帧（一条 assistant entry 产出 `MessageAssistant` 再加每个调用的计划 / 开始两帧，
//! 见 §5）。记录 ID 全局单调，所以 `Seq` 严格按记录次序递增，同一记录的帧按 `k` 递增；游标
//! 比较只需比整数。

use super::ids::{EntityId, SubmissionId};
use crate::types::{RunId, Seq};

/// `k` 占低 12 位：同一条记录最多产出 4096 帧。
pub const FRAME_BITS: u32 = 12;
/// `k` 的上限（含）。
pub const MAX_FRAME: u32 = (1 << FRAME_BITS) - 1;

/// 这条 run 的旧 `RunId`：首个输入 submission 的十进制 ID。
pub fn run_id(first_input: SubmissionId) -> RunId {
    RunId::from_raw(first_input.get().to_string())
}

/// `RunId` 对应的 submission ID。只认十进制规范写法（没有符号、前导零、空白），UUID 形状的旧
/// `RunId` 与任何别的字符串都返回 `None`。
pub fn submission_of(run: &RunId) -> Option<SubmissionId> {
    let text = run.as_str();
    let value: u64 = text.parse().ok()?;
    (value.to_string() == text).then(|| SubmissionId::new(value))
}

/// 记录 ID 的第 `k` 帧的 `Seq`。`k` 超过 [`MAX_FRAME`] 时按 `MAX_FRAME` 算（一条记录产出
/// 四千多帧不是正常数据；宁可这些帧同号也不让 gateway panic），记录 ID 大到放不下时同样饱和。
pub fn seq(record: impl EntityId, k: u32) -> Seq {
    let k = u64::from(k.min(MAX_FRAME));
    Seq(record.raw().saturating_mul(1 << FRAME_BITS) | k)
}

/// `Seq` 里的记录 ID（entry 或 submission，调用方自己知道是哪种：`EntryId::new(record_of(seq))`）。
pub fn record_of(seq: Seq) -> u64 {
    seq.0 >> FRAME_BITS
}

/// `Seq` 里的帧序号 `k`。
pub fn frame_of(seq: Seq) -> u32 {
    (seq.0 & u64::from(MAX_FRAME)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::{EntryId, SubmissionId};

    #[test]
    fn run_ids_are_the_decimal_first_input_submission() {
        let run = run_id(SubmissionId::new(42));
        assert_eq!(run.as_str(), "42");
        assert_eq!(submission_of(&run), Some(SubmissionId::new(42)));
    }

    #[test]
    fn only_canonical_decimal_run_ids_decode() {
        for text in [
            "",
            "+1",
            "-1",
            "01",
            " 1",
            "1 ",
            "1.0",
            "abc",
            "0192f0a0-0000-7000-8000-000000000000",
        ] {
            assert_eq!(submission_of(&RunId::from_raw(text)), None, "{text:?}");
        }
        assert_eq!(
            submission_of(&RunId::from_raw("0")),
            Some(SubmissionId::new(0))
        );
    }

    #[test]
    fn seq_packs_the_record_id_above_twelve_frame_bits() {
        let value = seq(EntryId::new(5), 3);
        assert_eq!(value, Seq((5 << 12) | 3));
        assert_eq!(record_of(value), 5);
        assert_eq!(frame_of(value), 3);
    }

    #[test]
    fn seqs_order_by_record_then_frame() {
        let frames = [
            seq(SubmissionId::new(4), 0),
            seq(SubmissionId::new(4), 1),
            seq(EntryId::new(5), 0),
            seq(EntryId::new(5), 2),
            seq(EntryId::new(6), 0),
        ];
        assert!(frames.windows(2).all(|pair| pair[0] < pair[1]));
        // 游标 = 某条记录的第 0 帧：同记录的后续帧都在它之后，前一条记录的都在它之前。
        let cursor = seq(EntryId::new(5), 0);
        assert!(seq(EntryId::new(4), super::MAX_FRAME) < cursor);
        assert_eq!(record_of(cursor), 5);
    }

    #[test]
    fn an_oversized_frame_number_saturates_instead_of_spilling_into_the_id() {
        let value = seq(EntryId::new(7), 100_000);
        assert_eq!(record_of(value), 7);
        assert_eq!(frame_of(value), MAX_FRAME);
    }
}
