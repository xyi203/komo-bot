//! `!`：在 Gateway 里直接跑一条 shell 命令，不经模型（[`SubmitRunRequest::shell`]）。
//!
//! 这里只有两件纯逻辑，都是"给人的话"：
//!
//! - [`refuse`]：发出去之前认出**注定跑不好**的命令。那边没有 TTY、stdin 是空的，编辑器 /
//!   分页器 / REPL 要么立刻退出、要么一直挂到超时；`sudo` 输不了密码，数据目录保护
//!   （bwrap 的 no_new_privs）下也提不了权。白等两分钟再看一个退出码，不如现在说清楚；
//! - [`explain`]：跑完之后按结果补一句**为什么**——超时、截断、撞上只读、命令没找到，
//!   只扔一个退出码人得自己猜。
//!
//! [`SubmitRunRequest::shell`]: komo_kernel::protocol::http::SubmitRunRequest::shell

/// 这段输入是不是一条 `!` 命令；是就交回 `!` 后面那段。
pub fn command_of(text: &str) -> Option<&str> {
    text.strip_prefix('!').map(str::trim)
}

/// 发出去之前就知道跑不好的：交回一句说明；能跑就是 `None`。
pub fn refuse(command: &str) -> Option<String> {
    if command.is_empty() {
        return Some(
            "`!` 后面跟一条命令，例如 `!ls -la`：在 Gateway 里直接跑，不经过模型，结果记进会话"
                .into(),
        );
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    // 跳过开头的 `FOO=bar` 赋值，找到真正的那个程序。
    let start = words
        .iter()
        .position(|word| !is_assignment(word))
        .unwrap_or(words.len());
    let program = words.get(start)?;
    let program = program.rsplit('/').next().unwrap_or(program);
    let args = &words[start + 1..];

    let reason = match program {
        "sudo" | "su" | "doas" | "pkexec" => {
            "这里没有 TTY，输不了密码；而且 shell 跑在数据目录保护（bwrap，no_new_privs）里，\
             提不了权。需要 root 的事请另开终端"
        }
        "vim" | "vi" | "nvim" | "nano" | "emacs" | "micro" | "hx" | "helix" => {
            "编辑器要终端，这里没有 TTY。看内容用 `!cat` / `!head`，改文件交给模型或另开终端"
        }
        "less" | "more" | "most" | "man" => {
            "分页器要终端，这里没有 TTY。用 `!cat`、`!head -n 50`，或 `!man xx | col -b | head -80`"
        }
        // `top -b` 是批处理模式，能跑；挡的是光秃秃的全屏用法。
        "top"
            if args
                .iter()
                .any(|arg| arg.starts_with('-') && arg.contains('b')) =>
        {
            return None;
        }
        "top" | "htop" | "btop" | "atop" | "glances" | "nmtui" => {
            "全屏程序要终端，这里没有 TTY。看一眼进程用 `!top -b -n 1 | head -20` 或 `!ps aux`"
        }
        "tmux" | "screen" | "zellij" => "终端复用器要终端，这里没有 TTY",
        "watch" => "`watch` 不会自己结束，只会一直跑到超时。直接跑一次里面那条命令就行",
        "ssh" | "mosh" if args.len() <= 1 => {
            "交互登录要终端，这里没有 TTY。执行一条远程命令可以：`!ssh 主机 '命令'`"
        }
        "python" | "python3" | "ipython" | "node" | "irb" | "ghci" | "lua" | "bash" | "sh"
        | "zsh" | "fish" | "mysql" | "psql" | "sqlite3" | "redis-cli"
            if args.is_empty() =>
        {
            "不带参数会进交互式 REPL，这里没有 TTY、stdin 是空的。带上脚本或 `-c '…'`"
        }
        "tail" | "journalctl"
            if args
                .iter()
                .any(|arg| matches!(*arg, "-f" | "-F" | "--follow")) =>
        {
            "`-f` 会一直跟下去，直到超时才停。看最近的用 `-n 100`"
        }
        _ => return None,
    };
    Some(format!("没有执行 `{command}`：{reason}"))
}

fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

/// 跑完之后按结果预览补一句说明；没什么要补的就是 `None`。
///
/// 预览是 `shell` 工具写的那一段：`exit=N stdout=…B stderr=…B[（输出已截断）]` 或
/// `timeout=Ns（…）…`，后面跟输出摘录。
pub fn explain(preview: &str) -> Vec<String> {
    let mut hints = Vec::new();
    let head = preview.lines().next().unwrap_or_default();
    if head.starts_with("timeout=") {
        hints.push(
            "命令跑满时限被终止了（默认 120 秒），上面是到点前的输出。长任务用 `nohup … &` \
             放后台，或者交给模型去轮询"
                .to_string(),
        );
    }
    if head.contains("（输出已截断）") {
        hints.push(
            "输出超过上限被截断了；完整输出存在 Gateway 里，接着问模型就能读回来".to_string(),
        );
    }
    if head.starts_with("exit=127 ") {
        hints.push(
            "exit=127：命令没找到。shell 用的是 Gateway 进程的 PATH，检查拼写或写全路径"
                .to_string(),
        );
    } else if head.starts_with("exit=126 ") {
        hints.push("exit=126：文件找到了但执行不了，多半是缺执行权限（chmod +x）".to_string());
    }
    if preview.contains("Read-only file system") {
        hints.push(
            "撞上只读了：komo 的数据目录（sessions/、state.db、runtime/、.env）在 shell 里是只读的，\
             防手滑。要改 komo 的状态用 `komo` 子命令"
                .to_string(),
        );
    }
    if preview.contains("no new privileges") {
        hints.push(
            "提权被挡住了：shell 跑在数据目录保护（bwrap，no_new_privs）里，sudo / setuid \
             程序用不了"
                .to_string(),
        );
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_leading_bang_is_a_shell_command() {
        assert_eq!(command_of("!ls -la"), Some("ls -la"));
        assert_eq!(command_of("! ls"), Some("ls"));
        assert_eq!(command_of("!"), Some(""));
        assert_eq!(command_of("看看 !ls"), None);
    }

    #[test]
    fn an_empty_command_gets_the_usage() {
        assert!(refuse("").unwrap().contains("!ls -la"));
    }

    #[test]
    fn commands_that_need_a_terminal_are_refused_with_an_alternative() {
        assert!(refuse("vim a.txt").unwrap().contains("cat"));
        assert!(refuse("/usr/bin/less x").unwrap().contains("head"));
        assert!(refuse("top").unwrap().contains("top -b"));
        assert!(refuse("sudo dnf upgrade").unwrap().contains("密码"));
        assert!(refuse("FOO=1 sudo ls").is_some(), "赋值前缀后面的才是程序");
        assert!(refuse("tail -f /var/log/x").unwrap().contains("-n 100"));
        assert!(refuse("python3").is_some());
        assert!(refuse("ssh nas").is_some());
    }

    #[test]
    fn ordinary_and_non_interactive_forms_go_through() {
        for command in [
            "ls -la",
            "python3 -c 'print(1)'",
            "ssh nas 'df -h'",
            "tail -n 20 log",
            "top -b -n 1",
            "git status && git log -3",
        ] {
            assert_eq!(refuse(command), None, "{command}");
        }
        assert_eq!(refuse("echo vim"), None, "参数里的 vim 不算");
    }

    #[test]
    fn the_outcome_is_explained_not_just_numbered() {
        assert!(explain("timeout=120s（进程组已终止）stdout=0B stderr=0B\n")[0].contains("120"));
        assert!(explain("exit=0 stdout=9B stderr=0B（输出已截断）\nx")[0].contains("截断"));
        assert!(
            explain("exit=127 stdout=0B stderr=20B\nfoo: command not found")[0].contains("PATH")
        );
        assert!(
            explain("exit=1 stdout=0B stderr=40B\nrm: cannot remove 'x': Read-only file system")[0]
                .contains("只读")
        );
        assert!(explain("exit=0 stdout=3B stderr=0B\nhi\n").is_empty());
    }
}
