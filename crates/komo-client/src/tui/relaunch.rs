//! 终端重启之后怎么把这个界面拉回来：OSC 88（Terminal Resume Protocol）。
//!
//! 终端崩了、机器重启、终端升级重开——TUI 进程跟着没了，Run 还在 Gateway 里，回来靠的
//! 是 `komo resume <会话>` 这一行。OSC 88 让程序**自己声明**这一行：有会话就 `arm`，终端
//! 把它按窗格存下，冷启动恢复时替人敲一遍；人自己退出（`Ctrl-C`）就 `clear`，故意退掉的
//! 不该被拉回来。被杀掉的进程来不及 `clear`，声明留着——那正是该被拉回来的时候。
//!
//! 单向、发了就算：不认 OSC 88 的终端整段忽略，不需要先探测支持。规范见
//! <https://github.com/Otty-sh/osc-88>，参考实现是 Otty。

use komo_kernel::types::ids::SessionId;

use crate::tui::TuiMode;

/// 撤回声明。
pub const CLEAR: &str = "\x1b]88;clear\x1b\\";

/// 声明「重启后跑 `komo resume <会话>`」（home 会话是 `komo home`）。
///
/// `cmd` 是终端拿来核对的那一半：它要在窗格的进程树里看到一个同名的活进程才肯存下，
/// 所以给这个可执行文件自己的路径。`args` 会经过 shell，会话 Id 里混进别的字符（`komo
/// resume` 的参数是人敲的）就不声明——返回 `None`。补读历史由 `komo resume` 自己画，
/// 所以带 `self_repaint=1`：终端不再回放旧屏幕，两边一起画会把格子搅乱。
pub fn arm(session: &SessionId, mode: TuiMode, cwd: &str) -> Option<String> {
    let args = match mode {
        TuiMode::Home => "home".to_string(),
        TuiMode::New | TuiMode::Resume => {
            let id = session.as_str();
            let plain = !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !plain {
                return None;
            }
            format!("resume {id}")
        }
    };
    let cmd = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "komo".into());
    let mut sequence = format!(
        "\x1b]88;arm;cmd={};args={};self_repaint=1",
        base64(cmd.as_bytes()),
        base64(args.as_bytes())
    );
    if !cwd.is_empty() {
        sequence.push_str(&format!(";cwd={}", base64(cwd.as_bytes())));
    }
    sequence.push_str("\x1b\\");
    Some(sequence)
}

/// 标准字母表、带 `=` 填充的 base64——OSC 88 的值只有这一种编码，为它不值得多一个依赖。
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads_like_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"nvim"), "bnZpbQ==");
        assert_eq!(base64("κ 目录".as_bytes()), "zrog55uu5b2V");
    }

    #[test]
    fn arm_declares_komo_resume_for_the_session() {
        let session = SessionId::from_raw("01a1-sess_9");
        let sequence = arm(&session, TuiMode::Resume, "/tmp/w").expect("普通的会话 Id");
        assert!(sequence.starts_with("\x1b]88;arm;cmd="));
        assert!(sequence.ends_with("\x1b\\"));
        assert!(sequence.contains(&format!(";args={};", base64(b"resume 01a1-sess_9"))));
        assert!(sequence.contains(";self_repaint=1;"));
        assert!(sequence.contains(&format!(";cwd={}", base64(b"/tmp/w"))));
    }

    #[test]
    fn arm_relaunches_home_as_komo_home() {
        let session = SessionId::from_raw("home-session");
        let sequence = arm(&session, TuiMode::Home, "").expect("home 总能声明");
        assert!(sequence.contains(&format!(";args={};", base64(b"home"))));
        assert!(!sequence.contains(";cwd="));
    }

    #[test]
    fn arm_refuses_ids_the_shell_would_interpret() {
        for raw in ["", "a b", "x;rm -rf ~", "$(id)", "a\x1b]0;t"] {
            assert_eq!(arm(&SessionId::from_raw(raw), TuiMode::Resume, "/"), None);
        }
    }
}
