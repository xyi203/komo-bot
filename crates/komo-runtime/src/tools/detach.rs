//! 长命令自动转后台（`docs/monitor.md` §8）。
//!
//! `bash` 在对话会话里跑过 `detach_after_secs` 还没完，就把还活着的子进程（管道、进程组、
//! 已收到的输出）交给接手的一方——生产里是 Gateway 的 `GatewayMonitors`——自己带着"已转
//! 后台 #id"收尾这次调用。接缝放在 runtime 而不是 kernel：交出去的是一个活的
//! [`LiveChild`]，kernel 没有 tokio，也不该有。
//!
//! 同一个文件里还有对话会话里"别在 bash 里 sleep 等"的判断：等状态是 `monitor` 的事。

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::types::refs::AttemptRef;

use super::process::LiveChild;

/// 交给接手方的一次转后台。
#[derive(Debug)]
pub struct DetachedCall {
    pub attempt: AttemptRef,
    pub command: String,
    pub cwd: PathBuf,
    pub live: LiveChild,
}

/// 接手之后的句柄：和 `monitor` 同一个 id 空间，`monitor_stop #短号` 停得掉它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detached {
    pub id: String,
    pub short_id: String,
}

#[async_trait]
pub trait Detacher: Send + Sync {
    /// 接手一个活的子进程，之后它的结束消息交回 `call.attempt.session`。
    /// 失败时接手方**自己终止进程组**，调用方不再碰它。
    async fn adopt(&self, call: DetachedCall) -> Result<Detached, String>;
}

/// `BashTool` 与 Gateway 之间的空槽：工具先造好，接手方等整份状态有了才接上。
#[derive(Default)]
pub struct DetachSlot(OnceLock<Arc<dyn Detacher>>);

impl std::fmt::Debug for DetachSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetachSlot")
            .field("wired", &self.0.get().is_some())
            .finish()
    }
}

impl DetachSlot {
    pub fn set(&self, detacher: Arc<dyn Detacher>) {
        if self.0.set(detacher).is_err() {
            tracing::warn!("转后台的接手方已经接过一次，忽略这次重复设置");
        }
    }

    pub fn get(&self) -> Option<&Arc<dyn Detacher>> {
        self.0.get()
    }
}

/// 一次 `sleep` 到这么久，对话会话里就拒绝。
pub const LONG_SLEEP: Duration = Duration::from_secs(10);

/// 命令里有没有一个**字面量**、不短于 [`LONG_SLEEP`] 的 `sleep`；有就返回它的原文
/// （`sleep 20`、`sleep 1m`）。`sleep $N` 之类不是字面量，不管。
pub fn long_sleep(command: &str) -> Option<String> {
    let spaced: String = command
        .chars()
        .map(|c| {
            if matches!(c, ';' | '|' | '&' | '(' | ')' | '{' | '}' | '\n' | '`') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let tokens: Vec<&str> = spaced.split_whitespace().collect();
    for (at, token) in tokens.iter().enumerate() {
        if *token != "sleep" {
            continue;
        }
        let mut shown = vec!["sleep"];
        for arg in &tokens[at + 1..] {
            let Some(span) = parse_span(arg.trim_matches(['"', '\''])) else {
                break;
            };
            shown.push(arg);
            if span >= LONG_SLEEP {
                return Some(shown.join(" "));
            }
        }
    }
    None
}

/// `10`、`2.5`、`30s`、`1m`、`2h`、`1d`。
fn parse_span(raw: &str) -> Option<Duration> {
    let (digits, unit) = match raw.chars().last()? {
        's' => (&raw[..raw.len() - 1], 1.0),
        'm' => (&raw[..raw.len() - 1], 60.0),
        'h' => (&raw[..raw.len() - 1], 3600.0),
        'd' => (&raw[..raw.len() - 1], 86_400.0),
        _ => (raw, 1.0),
    };
    let number: f64 = digits.parse().ok()?;
    (number.is_finite() && number >= 0.0).then(|| Duration::from_secs_f64(number * unit))
}

/// 最后 `n` 行。
pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_sleep_of_ten_seconds_or_more_is_found_in_any_position() {
        for (command, shown) in [
            ("sleep 20", "sleep 20"),
            ("sleep 1m", "sleep 1m"),
            ("sleep 0.5h", "sleep 0.5h"),
            (
                "for i in $(seq 1 30); do glab ci status; sleep 20; done",
                "sleep 20",
            ),
            ("a && sleep 10", "sleep 10"),
            ("(sleep 600)", "sleep 600"),
            ("sleep 5 15", "sleep 5 15"),
        ] {
            assert_eq!(long_sleep(command).as_deref(), Some(shown), "{command}");
        }
    }

    #[test]
    fn short_or_non_literal_sleeps_are_left_alone() {
        for command in [
            "sleep 2 && curl localhost:8080",
            "sleep 9.9",
            "sleep $N",
            "sleep \"$INTERVAL\"",
            "echo sleepy 30",
            "make sleep_test",
        ] {
            assert_eq!(long_sleep(command), None, "{command}");
        }
    }
}
