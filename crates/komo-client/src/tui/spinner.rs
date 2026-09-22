//! 转圈的那一格（§13.4）。
//!
//! **"在跑"这件事用动的东西说，不用字。** 一个 `ok` / `▶` 只在它变的那一刻有信息，之后
//! 就是一块静止的文字，看久了分不清是还在跑还是卡住了；一个转着的点不用读就知道"它还
//! 活着"。跑完了反过来——那时要的是一眼扫过去的结论，所以用一个定住的符号。
//!
//! 帧从**时钟**算，不从帧计数器算：状态机不读时钟（钟由驱动在 `Tick` 里递给它），于是
//! 同一个时刻画出来的永远是同一帧，测试给一个固定时刻就能断言它转到哪了。
//!
//! 八帧都是盲文点阵，**每帧正好一列**——换帧不会让后面的字左右跳一格。

use time::OffsetDateTime;

/// 转圈的八帧。
pub const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// 一帧停多久（毫秒）。八帧一圈约一秒——比读一行字慢，比看着心慌快。
pub const FRAME_MS: i128 = 120;

/// 没有钟的时候用它：交给终端回滚区的那些行**冻在纸上**，不该带着某一帧随机的样子留在
/// 那里（见 [`crate::tui::transcript`]）。
pub const STILL: &str = "▸";

/// 这一刻该显示哪一帧。
pub fn frame(now: Option<OffsetDateTime>) -> &'static str {
    let Some(now) = now else {
        return STILL;
    };
    let index =
        (now.unix_timestamp_nanos() / (FRAME_MS * 1_000_000)).rem_euclid(FRAMES.len() as i128);
    FRAMES[index as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// 每一帧都只占一列：否则转一圈，它后面那行字就跟着左右晃。
    #[test]
    fn every_frame_is_one_column_wide() {
        for frame in FRAMES {
            assert_eq!(
                crate::tui::markdown::display_width(frame),
                1,
                "{frame} 不是一列宽"
            );
        }
        assert_eq!(crate::tui::markdown::display_width(STILL), 1);
    }

    /// 帧只由时刻决定：同一刻画两次是同一帧，隔一帧的时间画就换一帧。
    #[test]
    fn the_frame_follows_the_clock_and_nothing_else() {
        let t0 = datetime!(2026-09-22 08:00:00 UTC);
        assert_eq!(frame(Some(t0)), frame(Some(t0)));
        let later = t0 + time::Duration::milliseconds(FRAME_MS as i64);
        assert_ne!(frame(Some(t0)), frame(Some(later)), "过了一帧就该换一帧");
        let full_turn = t0 + time::Duration::milliseconds(FRAME_MS as i64 * FRAMES.len() as i64);
        assert_eq!(frame(Some(t0)), frame(Some(full_turn)), "转满一圈回到原处");
    }

    /// 没有钟就不转——那是要冻进回滚区的行。
    #[test]
    fn without_a_clock_it_stands_still() {
        assert_eq!(frame(None), STILL);
    }
}
