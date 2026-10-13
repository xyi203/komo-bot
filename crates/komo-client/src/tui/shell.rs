//! `!`：在 Gateway 里直接跑一条 shell 命令，不经模型（[`SubmitRunRequest::shell`]）。
//!
//! 这里只有一件纯逻辑：[`refuse`] 在发出去之前认出**注定跑不好**的命令。那边没有 TTY、
//! stdin 是空的，编辑器 / 分页器 / REPL 要么立刻退出、要么一直挂到超时；`sudo` 输不了密码，
//! 数据目录保护（bwrap 的 no_new_privs）下也提不了权。白等两分钟再看一个退出码，不如现在
//! 说清楚。
//!
//! 跑完之后不再补说明：终端模式就是终端，结果是什么样就显示什么样（2026-10-13 删掉
//! `explain`）。
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
}
