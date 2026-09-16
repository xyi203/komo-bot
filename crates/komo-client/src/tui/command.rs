//! 斜杠命令的解析（§11.3 命令表 + TUI 自己的 `/model` `/effort` `/help`）。
//!
//! 解析是**纯函数**：一段文本进去，一个 [`Command`] 或一条 [`CommandError`] 出来。非法
//! 值报错而不是静默取默认——打字的人还在屏幕前，把 `/effort hgih` 悄悄当成没设置，他
//! 要到下一个 Run 的状态行才发现。

use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::ShortId;
use komo_kernel::types::model::Effort;

/// TUI 认得的命令。§11.3 的七个聊天命令里，`/id` 不在这里——它回显的是聊天平台的
/// 身份，TUI 没有平台身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `/new`：追加 `conversation.boundary`，不切 Session。
    New,
    /// `/cancel`：取消该 Session 当前 Run。
    Cancel,
    /// `/status`：当前 Run 状态、待审批数。
    Status,
    /// `/pending`：列出待处理审批及其短 ID。
    Pending,
    /// `/approve [short_id] [run]`
    Approve {
        short_id: Option<ShortId>,
        scope: ApprovalScope,
    },
    /// `/reject [short_id]`
    Reject {
        short_id: Option<ShortId>,
    },
    /// `/model`（列出）/ `/model <id>`（设定）
    Model {
        id: Option<String>,
    },
    /// `/effort`（列出）/ `/effort <level>`（设定）
    Effort {
        level: Option<Effort>,
    },
    Help,
    /// `/quit`
    Quit,
}

/// 解析失败。每一条都要能直接印给人看。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("没有 /{name} 这个命令。/help 看全部")]
    Unknown { name: String },
    #[error("/{name} 不带参数，收到「{extra}」")]
    UnexpectedArgument { name: String, extra: String },
    #[error("「{raw}」不是短 ID（4 位 base32，如 7K2M）")]
    BadShortId { raw: String },
    #[error("/approve 的范围只有 `run`，收到「{raw}」")]
    BadScope { raw: String },
    #[error("「{raw}」不是可选的 effort；可选值：{options}")]
    BadEffort { raw: String, options: String },
    #[error("「{raw}」不在模型清单里；可选值：{options}")]
    BadModel { raw: String, options: String },
}

/// TUI 能提供的 effort 档位。
///
// TODO(decide: §13.3 明说「low / medium / high / max 等值不是所有模型共享的固定集合」，
// 而 §13.1 里没有一个接口能报出当前模型支持哪几档。在有那个接口之前，TUI 用这五个
// 常见档位做白名单——**宁可拒绝一个模型其实支持的档位，也不放过一个它不支持的**：
// 后者要到请求发出才失败，而 §13.3 要求「不支持的 effort 在请求前拒绝」。
pub const EFFORT_LEVELS: [&str; 5] = [
    Effort::NONE,
    Effort::LOW,
    Effort::MEDIUM,
    Effort::HIGH,
    Effort::MAX,
];

/// 命令名与一行说明，`/help` 与命令面板共用一张表。
pub const COMMANDS: [(&str, &str); 10] = [
    ("/new", "在当前会话划一条回放边界（不切会话）"),
    ("/cancel", "取消本会话正在跑的 Run"),
    ("/status", "当前 Run 状态与待审批数"),
    ("/pending", "列出待处理审批及其短 ID"),
    ("/approve", "[短ID] [run] 批准；带 run = 本次 Run 范围"),
    ("/reject", "[短ID] 拒绝"),
    ("/model", "[模型] 列出或设定下一个 Run 的模型"),
    ("/effort", "[档位] 列出或设定下一个 Run 的推理强度"),
    ("/help", "这张表"),
    ("/quit", "退出 TUI（不取消后台运行）"),
];

/// 这段输入是不是一条命令。
pub fn is_command(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

/// 命令面板：按已经打出来的前缀过滤命令表。
pub fn palette(prefix: &str) -> Vec<(&'static str, &'static str)> {
    let prefix = prefix.trim();
    if !prefix.starts_with('/') || prefix.contains(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .copied()
        .collect()
}

/// 候选里能补全到的最长公共前缀。
pub fn complete(prefix: &str) -> Option<String> {
    let matches = palette(prefix);
    let first = matches.first()?.0;
    let mut common = first.len();
    for (name, _) in &matches[1..] {
        common = common.min(
            name.bytes()
                .zip(first.bytes())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    let completed = &first[..common];
    (completed.len() > prefix.len()).then(|| completed.to_string())
}

/// 解析一条命令。`models` 是当前可选的模型清单；为空 = 不校验（Gateway 没给清单）。
pub fn parse(text: &str, models: &[String]) -> Result<Command, CommandError> {
    let text = text.trim();
    let body = text.strip_prefix('/').unwrap_or(text);
    let mut parts = body.split_whitespace();
    let name = parts.next().unwrap_or("").to_ascii_lowercase();
    let args: Vec<&str> = parts.collect();

    let no_args = |command: Command| -> Result<Command, CommandError> {
        match args.first() {
            None => Ok(command),
            Some(extra) => Err(CommandError::UnexpectedArgument {
                name: name.clone(),
                extra: (*extra).to_string(),
            }),
        }
    };

    match name.as_str() {
        "new" => no_args(Command::New),
        "cancel" => no_args(Command::Cancel),
        "status" => no_args(Command::Status),
        "pending" => no_args(Command::Pending),
        "help" | "h" | "?" => no_args(Command::Help),
        "quit" | "exit" | "q" => no_args(Command::Quit),
        "approve" => {
            let mut short_id = None;
            let mut scope = ApprovalScope::Once;
            for arg in &args {
                if arg.eq_ignore_ascii_case("run") {
                    scope = ApprovalScope::Run;
                } else if short_id.is_none() {
                    short_id =
                        Some(ShortId::parse(arg).ok_or_else(|| CommandError::BadShortId {
                            raw: (*arg).to_string(),
                        })?);
                } else {
                    return Err(CommandError::BadScope {
                        raw: (*arg).to_string(),
                    });
                }
            }
            Ok(Command::Approve { short_id, scope })
        }
        "reject" | "deny" => {
            let short_id = match args.first() {
                None => None,
                Some(raw) => Some(ShortId::parse(raw).ok_or_else(|| CommandError::BadShortId {
                    raw: (*raw).to_string(),
                })?),
            };
            Ok(Command::Reject { short_id })
        }
        "model" => match args.first() {
            None => Ok(Command::Model { id: None }),
            Some(raw) => {
                if !models.is_empty() && !models.iter().any(|m| m == raw) {
                    return Err(CommandError::BadModel {
                        raw: (*raw).to_string(),
                        options: models.join(" · "),
                    });
                }
                Ok(Command::Model {
                    id: Some((*raw).to_string()),
                })
            }
        },
        "effort" => match args.first() {
            None => Ok(Command::Effort { level: None }),
            Some(raw) => {
                let level = Effort::new(raw);
                if !EFFORT_LEVELS.contains(&level.as_str()) {
                    return Err(CommandError::BadEffort {
                        raw: (*raw).to_string(),
                        options: EFFORT_LEVELS.join(" · "),
                    });
                }
                Ok(Command::Effort { level: Some(level) })
            }
        },
        other => Err(CommandError::Unknown {
            name: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seven_chat_commands_parse() {
        assert_eq!(parse("/new", &[]).unwrap(), Command::New);
        assert_eq!(parse("/cancel", &[]).unwrap(), Command::Cancel);
        assert_eq!(parse("/status", &[]).unwrap(), Command::Status);
        assert_eq!(parse("/pending", &[]).unwrap(), Command::Pending);
        assert_eq!(
            parse("/approve", &[]).unwrap(),
            Command::Approve {
                short_id: None,
                scope: ApprovalScope::Once
            }
        );
        assert_eq!(
            parse("/reject", &[]).unwrap(),
            Command::Reject { short_id: None }
        );
    }

    #[test]
    fn approve_with_a_short_id_and_a_run_scope() {
        assert_eq!(
            parse("/approve 7k2m run", &[]).unwrap(),
            Command::Approve {
                short_id: ShortId::parse("7K2M"),
                scope: ApprovalScope::Run
            }
        );
        // 顺序不重要：范围是个词，不是第二个位置。
        assert_eq!(
            parse("/approve run 7K2M", &[]).unwrap(),
            Command::Approve {
                short_id: ShortId::parse("7K2M"),
                scope: ApprovalScope::Run
            }
        );
    }

    #[test]
    fn a_short_id_that_is_not_one_is_refused_not_ignored() {
        let error = parse("/approve 7K2I", &[]).unwrap_err();
        assert!(matches!(error, CommandError::BadShortId { .. }), "{error}");
        assert!(error.to_string().contains("7K2I"));
    }

    #[test]
    fn an_unknown_command_names_itself() {
        let error = parse("/aprove", &[]).unwrap_err();
        assert_eq!(
            error,
            CommandError::Unknown {
                name: "aprove".into()
            }
        );
    }

    #[test]
    fn a_command_that_takes_nothing_refuses_an_argument() {
        let error = parse("/new now", &[]).unwrap_err();
        assert!(
            matches!(error, CommandError::UnexpectedArgument { .. }),
            "{error}"
        );
    }

    #[test]
    fn effort_lists_its_options_when_given_none() {
        assert_eq!(
            parse("/effort", &[]).unwrap(),
            Command::Effort { level: None }
        );
    }

    #[test]
    fn an_illegal_effort_is_refused_and_the_error_lists_the_options() {
        let error = parse("/effort hgih", &[]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("hgih"), "{message}");
        for level in EFFORT_LEVELS {
            assert!(message.contains(level), "{message} 少了 {level}");
        }
    }

    #[test]
    fn an_effort_is_normalized_before_it_is_checked() {
        assert_eq!(
            parse("/effort  HIGH ", &[]).unwrap(),
            Command::Effort {
                level: Some(Effort::new("high"))
            }
        );
    }

    #[test]
    fn a_model_outside_the_menu_is_refused_and_the_error_lists_the_menu() {
        let menu = vec!["gpt-x".to_string(), "claude-y".to_string()];
        let error = parse("/model gpt-z", &menu).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("gpt-x") && message.contains("claude-y"),
            "{message}"
        );
    }

    #[test]
    fn without_a_menu_any_model_name_is_accepted() {
        assert_eq!(
            parse("/model whatever", &[]).unwrap(),
            Command::Model {
                id: Some("whatever".into())
            }
        );
    }

    #[test]
    fn the_palette_filters_by_prefix_and_completes_the_common_part() {
        let matches = palette("/ne");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "/new");
        assert_eq!(complete("/ne").as_deref(), Some("/new"));
        // `/` 后面什么都没打时是整张表。
        assert_eq!(palette("/").len(), COMMANDS.len());
        // 已经打完一个词就不是在选命令了。
        assert!(palette("/approve 7K2M").is_empty());
    }
}
