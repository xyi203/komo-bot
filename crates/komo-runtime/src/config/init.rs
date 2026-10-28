//! `komo init`（`docs/komo_bot.md` §3）：在数据目录写一份带注释的最小 `config.toml`，再建一个
//! 权限 600 的 `.env` 模板。
//!
//! 只做"第一次能跑起来"需要的最小集合：一个助手、一个模型提供方占位、一个对话模型、
//! 默认模型、关键词记忆检索（没有 embedding 模型时默认的 hybrid 检索会校验失败）。渠道、
//! MCP、压缩、执行限额等都有默认值，需要时看设计文档再加。
//!
//! 已有的 `config.toml` **不覆盖**（也没有 `--force`）：它是操作者手写的东西，重新生成
//! 一份等于把它丢掉。已有的 `.env` 同样不碰——里面是凭证。

use std::io::Write;
use std::path::{Path, PathBuf};

use super::file::Sources;

/// 模板里模型凭证的变量名（`.env` 里填它）。
pub const API_KEY_VARIABLE: &str = "KOMO_LLM_API_KEY";

/// 一次 `init` 做了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    pub home: PathBuf,
    pub config: PathBuf,
    pub env: PathBuf,
    /// `.env` 是这次新建的；`false` = 它已经在，原样没动。
    pub env_created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InitError {
    #[error("{} 已经存在，不覆盖；想重新生成就先把它改名或删掉", .0.display())]
    ConfigExists(PathBuf),
    #[error("{}：{message}", path.display())]
    Io { path: PathBuf, message: String },
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> InitError + '_ {
    move |error| InitError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

/// 在 `home` 里生成 `config.toml` 与 `.env` 模板。`config.toml` 已存在就报
/// [`InitError::ConfigExists`]，**什么都不写**。
pub fn init_home(home: &Path) -> Result<InitReport, InitError> {
    let sources = Sources::under(home);
    if sources.config.exists() {
        return Err(InitError::ConfigExists(sources.config));
    }
    std::fs::create_dir_all(home).map_err(io_error(home))?;

    // `create_new`：检查与创建之间有人抢先写了也不会被覆盖。
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sources.config)
    {
        Ok(mut file) => file
            .write_all(config_template().as_bytes())
            .map_err(io_error(&sources.config))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(InitError::ConfigExists(sources.config));
        }
        Err(error) => return Err(io_error(&sources.config)(error)),
    }

    let env_created = create_env(&sources.env)?;
    Ok(InitReport {
        home: home.to_path_buf(),
        config: sources.config,
        env: sources.env,
        env_created,
    })
}

/// 建权限 600 的 `.env` 模板；已存在就不碰（返回 `false`）。
fn create_env(path: &Path) -> Result<bool, InitError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // 创建时就是 600，不存在"先 644 再改"的窗口。
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(env_template().as_bytes())
                .map_err(io_error(path))?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(io_error(path)(error)),
    }
}

/// 最小 `config.toml`。`default_agent` 与 `[agents.assistant]` 必须在文件最前面：写在任何
/// `[表]` 之后的顶层键会落进那张表里（TOML 的规则），变成"不认识的键"。
pub fn config_template() -> String {
    format!(
        r#"# komo 配置（`komo init` 生成）。保存后 Gateway 自动热重载；改完先 `komo config check`。
# 完整说明：docs/komo_bot.md §3（配置）、§4（助手）。凭证不写在这里，写在同目录的 .env。

# 没有归属的入口（TUI、CLI、Cron）走哪个助手。这两项要留在文件最前面。
default_agent = "assistant"

[agents.assistant]
instructions = "你是我的个人助手，回答用中文，简洁直接。"

# 模型服务：换成你实际用的。api_backend 可选 responses / chat_completions / anthropic_messages。
[model_providers.default]
base_url = "https://api.openai.com/v1"
api_backend = "responses"
env_key = "{API_KEY_VARIABLE}"          # 凭证在 .env 里的变量名
# timeout_secs = 120  # 等待响应头或正文多久没收到数据算超时；连续 3 次零字节超时停止重试

# 对话模型：model 换成服务提供的模型名；effort 不写就用模型默认。
[model.chat]
type = "completion"
model_provider = "default"
model = "gpt-5"

[models]
default = "chat"

# 记忆检索：没有配 embedding 模型时只能用关键词检索（默认的 hybrid 要求有 embedding）。
[memory.retrieval]
mode = "keyword"
"#
    )
}

/// `.env` 模板：只有模型凭证一行，值留空（空值按"没配"处理，`komo config check` 会指出来）。
pub fn env_template() -> String {
    format!(
        "# komo 的凭证，只放在这里（权限 600，别提交到仓库）。\n\
         # config.toml 里 [model_providers.default] 的 env_key 指向下面这个变量，填上你的 API key：\n\
         {API_KEY_VARIABLE}=\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigError, ConfigHolder, LoadOptions};
    use komo_kernel::protocol::config::IssueSeverity;

    fn filled_env(key: &str) -> String {
        env_template().replace(
            &format!("{API_KEY_VARIABLE}=\n"),
            &format!("{API_KEY_VARIABLE}={key}\n"),
        )
    }

    /// 生成的文件填上 key 之后，走和 Gateway 启动一样的加载 + 校验（`ConfigHolder::load`）：
    /// 装得上、没有错误、也没有"不认识的键"（那是顶层键落进了某张表里的征兆）。
    #[test]
    fn the_generated_files_load_and_validate_once_the_key_is_filled_in() {
        let home = tempfile::tempdir().unwrap();
        let report = init_home(home.path()).unwrap();
        assert!(report.env_created);
        std::fs::write(&report.env, filled_env("sk-test")).unwrap();

        let holder = ConfigHolder::load(&LoadOptions::at(home.path())).expect("装得上");
        let snapshot = holder.current();
        assert_eq!(snapshot.agent.default_agent, "assistant");
        assert!(snapshot.agent.get("assistant").is_some());
        assert_eq!(snapshot.model.model, "gpt-5");
        assert_eq!(snapshot.model.api_key_env, API_KEY_VARIABLE);
        assert!(
            holder
                .warnings()
                .iter()
                .all(|issue| issue.severity != IssueSeverity::Error),
            "{:?}",
            holder.warnings()
        );
        assert!(
            holder
                .warnings()
                .iter()
                .all(|issue| !issue.message.contains("不认识这个键")),
            "{:?}",
            holder.warnings()
        );
    }

    /// 没填 key 就加载：校验把缺的变量名说出来，操作者照着填。
    #[test]
    fn the_untouched_template_asks_for_the_api_key_by_name() {
        let home = tempfile::tempdir().unwrap();
        init_home(home.path()).unwrap();
        let Err(error) = ConfigHolder::load(&LoadOptions::at(home.path())) else {
            panic!("没有 key 不该装得上");
        };
        let ConfigError::Invalid { issues } = &error else {
            panic!("{error:?}");
        };
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains(API_KEY_VARIABLE)),
            "{issues:?}"
        );
    }

    /// 已有 config.toml：拒绝，原文件一个字节都不动，也不顺手建 `.env`。
    #[test]
    fn an_existing_config_is_never_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        std::fs::write(&config, "# 我自己写的\n").unwrap();

        let error = init_home(home.path()).unwrap_err();
        assert_eq!(error, InitError::ConfigExists(config.clone()));
        assert!(error.to_string().contains("不覆盖"));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "# 我自己写的\n");
        assert!(!home.path().join(".env").exists());
    }

    /// 已有 `.env`（里面是凭证）原样不动，报告里说它不是这次建的。
    #[test]
    fn an_existing_env_file_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let env = home.path().join(".env");
        std::fs::write(&env, "FEISHU_APP_ID=cli_x\n").unwrap();

        let report = init_home(home.path()).unwrap();
        assert!(!report.env_created);
        assert_eq!(
            std::fs::read_to_string(&env).unwrap(),
            "FEISHU_APP_ID=cli_x\n"
        );
        assert!(home.path().join("config.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_new_env_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let report = init_home(home.path()).unwrap();
        let mode = std::fs::metadata(&report.env).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// 数据目录还不存在也行（第一次装机）。
    #[test]
    fn the_data_directory_is_created_when_missing() {
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("nested").join(".komo");
        let report = init_home(&home).unwrap();
        assert!(report.config.exists() && report.env.exists());
    }

    /// 顶层键在任何表之前：`default_agent` 与 `[agents.assistant]` 是文件里最前面的两项。
    #[test]
    fn the_agent_declaration_comes_before_every_other_table() {
        let text = config_template();
        let first_table = text
            .lines()
            .position(|line| line.starts_with('['))
            .expect("有表");
        let default_agent = text
            .lines()
            .position(|line| line.starts_with("default_agent"))
            .expect("有 default_agent");
        let agents = text
            .lines()
            .position(|line| line.starts_with("[agents.assistant]"))
            .expect("有 agents.assistant");
        assert!(default_agent < first_table);
        assert_eq!(first_table, agents);
    }
}
