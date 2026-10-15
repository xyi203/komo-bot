//! 转圈的那一格（§13.4）。
//!
//! **"在跑"这件事用动的东西说，不用字。** 一个 `ok` / `▶` 只在它变的那一刻有信息，之后
//! 就是一块静止的文字，看久了分不清是还在跑还是卡住了；一个转着的点不用读就知道"它还
//! 活着"。跑完了反过来——那时要的是一眼扫过去的结论，所以用一个定住的符号。
//!
//! 帧从**时钟**算，不从帧计数器算：状态机不读时钟（钟由驱动在 `Tick` 里递给它），于是
//! 同一个时刻画出来的永远是同一帧，测试给一个固定时刻就能断言它转到哪了。
//!
//! **两种"在跑"长得不一样**（照 Claude Code）：状态行那一格是模型这一摊在动，用一颗来回
//! 变形的星；工具行那一格是某一次调用在跑，用一个一明一暗的圆点。两者都在动时一眼分得
//! 出哪个是模型、哪个是工具。
//!
//! 每帧**正好一列**——换帧不会让后面的字左右跳一格。

use time::OffsetDateTime;

/// 状态行转圈的八帧：一颗星长大再缩回去。
pub const FRAMES: &[&str] = &["✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳"];

/// 一帧停多久（毫秒）。八帧一圈约一秒——比读一行字慢，比看着心慌快。
pub const FRAME_MS: i128 = 120;

/// 工具行在跑的那一格。
pub const PULSE: &str = "●";

/// 圆点亮一次、暗一次各多久（毫秒）。比星慢：它说的是"还在跑"，不是"在想"。
pub const PULSE_MS: i128 = 480;

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

/// 这一刻圆点是亮的还是暗的。没有钟就是亮的。
pub fn lit(now: Option<OffsetDateTime>) -> bool {
    now.is_none_or(|now| (now.unix_timestamp_nanos() / (PULSE_MS * 1_000_000)).rem_euclid(2) == 0)
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
        assert_eq!(crate::tui::markdown::display_width(PULSE), 1);
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

    /// 圆点按自己的节拍一明一暗，与星不同步。
    #[test]
    fn the_pulse_alternates_on_its_own_beat() {
        let t0 = datetime!(2026-09-22 08:00:00 UTC);
        let half = t0 + time::Duration::milliseconds(PULSE_MS as i64);
        assert_ne!(lit(Some(t0)), lit(Some(half)));
        assert_eq!(
            lit(Some(t0)),
            lit(Some(half + time::Duration::milliseconds(PULSE_MS as i64)))
        );
        assert!(lit(None));
    }

    /// 没有钟就不转——那是要冻进回滚区的行。
    #[test]
    fn without_a_clock_it_stands_still() {
        assert_eq!(frame(None), STILL);
    }
}
