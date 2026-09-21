//! 配置校验（§3、§11.2、§13.3）。
//!
//! **`komo config check` 与热重载共用这一个函数。**两条性质写在结构里：每个问题都带
//! **键路径**（错在哪一行，不是"配置有问题"），以及**一次报全**——遇到第一个错误不停，
//! 因为改配置的人希望一次改完。
//!
//! 校验只看快照：凭证存在与否由 [`ConfigSnapshot::credentials`] 里有没有那一行回答
//! （装填时空值不入表），所以校验函数拿不到、也不需要凭证的值。

use komo_kernel::protocol::config::{
    ChannelConfig, ConfigIssue, ConfigSnapshot, IssueSeverity, KeyPath,
};
use komo_kernel::types::chat::{ChannelPlatform, PeerId};
use komo_kernel::types::memory::RetrievalMode;
use komo_kernel::types::model::{CatalogModel, EmbeddingConfig, ModelConfig};

use super::effort::EffortCapabilities;
use super::file::channel_credentials;

/// 向量维度的可信范围。小于 1 是结构错误，大得离谱的是打字错误——两者都会在第一次
/// 建索引时才炸，而那时人已经走了。
const MAX_DIMENSIONS: u32 = 16_384;

/// 校验一份快照。**`komo config check` 与重载共用它**（§3 第 1 步）。
pub fn validate(snapshot: &ConfigSnapshot) -> Vec<ConfigIssue> {
    validate_with(snapshot, &EffortCapabilities::builtin())
}

/// 同上，但由调用方给出 effort 档位声明（新后端还没进内建表时用）。
pub fn validate_with(snapshot: &ConfigSnapshot, caps: &EffortCapabilities) -> Vec<ConfigIssue> {
    let mut issues = Vec::new();

    check_start_only(snapshot, &mut issues);
    for (alias, model) in &snapshot.model_catalog.entries {
        let key = format!("model.{alias}");
        if alias.trim().is_empty() {
            issues.push(error("model", "模型 alias 不能为空"));
        }
        if model.name().trim().is_empty() {
            issues.push(error(&format!("{key}.name"), "显示名不能为空"));
        }
        match model {
            CatalogModel::Completion {
                config,
                context_window,
                ..
            } => {
                check_model(config, &key, Role::Chat, snapshot, caps, &mut issues);
                if *context_window == Some(0) {
                    issues.push(error(
                        &format!("{key}.context_window"),
                        "context_window 不能是 0",
                    ));
                }
            }
            CatalogModel::Embedding { config, .. } => {
                check_model(
                    &config.model,
                    &key,
                    Role::Embedding,
                    snapshot,
                    caps,
                    &mut issues,
                );
                check_embedding_options(config, &key, &mut issues);
            }
        }
    }

    if snapshot.memory.enabled {
        check_memory_embedding(snapshot, &mut issues);
        check_retrieval(snapshot, &mut issues);
    }

    check_channels(snapshot, &mut issues);
    check_policy(snapshot, &mut issues);
    check_execution(snapshot, &mut issues);
    check_typesafe(snapshot, &mut issues);
    check_agents(snapshot, &mut issues);

    issues.sort_by(|a, b| a.key.cmp(&b.key).then(a.severity.cmp(&b.severity)));
    issues
}

/// 有没有致命问题——有就**不装上**（§3 第 1 步）。
pub fn has_errors(issues: &[ConfigIssue]) -> bool {
    issues
        .iter()
        .any(|issue| issue.severity == IssueSeverity::Error)
}

fn error(key: &str, message: impl Into<String>) -> ConfigIssue {
    ConfigIssue {
        key: KeyPath::new(key),
        severity: IssueSeverity::Error,
        message: message.into(),
    }
}

fn warning(key: &str, message: impl Into<String>) -> ConfigIssue {
    ConfigIssue {
        key: KeyPath::new(key),
        severity: IssueSeverity::Warning,
        message: message.into(),
    }
}

fn check_start_only(snapshot: &ConfigSnapshot, issues: &mut Vec<ConfigIssue>) {
    let listen = &snapshot.start_only.listen;
    if listen.parse::<std::net::SocketAddr>().is_err() {
        issues.push(error(
            "start_only.listen",
            format!("`{listen}` 不是一个 `地址:端口`"),
        ));
    }
    if !snapshot.start_only.data_dir.is_absolute() {
        issues.push(error(
            "start_only.data_dir",
            format!(
                "数据目录要是绝对路径，现在是 `{}`",
                snapshot.start_only.data_dir.display()
            ),
        ));
    }
}

/// 校验时按哪张 effort 声明表判（§13.3：聊天和向量是两组控制）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Chat,
    Embedding,
}

fn check_model(
    model: &ModelConfig,
    key: &str,
    role: Role,
    snapshot: &ConfigSnapshot,
    caps: &EffortCapabilities,
    issues: &mut Vec<ConfigIssue>,
) {
    let provider = model.provider.trim();
    let known = match role {
        Role::Chat => caps.knows_provider(provider),
        Role::Embedding => caps.knows_embedding_provider(provider),
    };
    if provider.is_empty() {
        issues.push(error(&format!("{key}.api_backend"), "api_backend 不能为空"));
    } else if !known {
        let expected = match role {
            Role::Chat => format!(
                "生成协议只有 `{}` 与 `{}`",
                crate::llm::CHAT_COMPLETIONS,
                crate::llm::RESPONSES,
            ),
            Role::Embedding => format!(
                "向量后端只有 `{}` 与 `{}`",
                crate::embedding::OPENAI_COMPATIBLE,
                crate::embedding::OLLAMA
            ),
        };
        issues.push(error(
            &format!("{key}.api_backend"),
            format!("不认识 api_backend `{provider}`：{expected}"),
        ));
    }
    check_base_url(&model.base_url, &format!("{key}.base_url"), issues);

    if model.model.trim().is_empty() {
        issues.push(error(&format!("{key}.model"), "模型名不能为空"));
    }

    if model.api_key_env.trim().is_empty() {
        issues.push(error(
            &format!("{key}.api_key_env"),
            "要写凭证所在的**变量名**（凭证本身放 .env）",
        ));
    } else if !snapshot.credentials.contains_key(&model.api_key_env) {
        issues.push(error(
            &format!("{key}.api_key_env"),
            format!(
                "`{}` 在 .env / 环境里没有值；凭证只放 .env，config.toml 里写变量名",
                model.api_key_env
            ),
        ));
    }

    if model.timeout_secs == 0 {
        issues.push(error(&format!("{key}.timeout_secs"), "超时不能是 0"));
    }

    // provider 都不认识，档位就无从谈起；上面已经报过，不再叠一条 effort 的错。
    if !known {
        return;
    }
    // §13.3：不支持的档位要指出**模型、配置位置及支持值**，不静默映射为另一档。
    let checked = match role {
        Role::Chat => caps.check(model),
        Role::Embedding => caps.check_embedding(model),
    };
    if let Err(problem) = checked {
        issues.push(error(&format!("{key}.effort"), problem.to_string()));
    }
}

fn check_base_url(base_url: &str, key: &str, issues: &mut Vec<ConfigIssue>) {
    let url = base_url.trim();
    if url.is_empty() {
        issues.push(error(key, "base_url 不能为空"));
    } else if !(url.starts_with("http://") || url.starts_with("https://")) {
        issues.push(error(
            key,
            format!("`{url}` 不是一个 http(s) 端点；相同协议可以连接不同端点，但要写全"),
        ));
    }
}

fn check_memory_embedding(snapshot: &ConfigSnapshot, issues: &mut Vec<ConfigIssue>) {
    let Some(_embedding) = &snapshot.memory.embedding else {
        // §13.3：「embedding 必须独立指定；若明确选择 keyword 模式，可以不配置」。
        if snapshot.memory.retrieval.mode != RetrievalMode::Keyword {
            issues.push(error(
                "memory.embedding",
                format!(
                    "检索模式是 {:?} 却没有 memory.embedding alias；要么补上，要么把 \
                     memory.retrieval.mode 明确设成 \"keyword\"",
                    snapshot.memory.retrieval.mode
                ),
            ));
        }
        return;
    };
}

fn check_embedding_options(embedding: &EmbeddingConfig, key: &str, issues: &mut Vec<ConfigIssue>) {
    match embedding.dimensions {
        Some(0) => issues.push(error(&format!("{key}.dimensions"), "维度不能是 0")),
        Some(dimensions) if dimensions > MAX_DIMENSIONS => issues.push(error(
            &format!("{key}.dimensions"),
            format!("维度 {dimensions} 超出可信范围（1..={MAX_DIMENSIONS}）"),
        )),
        // 省略时用模型返回的维度，校验后固定到索引代次（§9.5）——这是允许的。
        _ => {}
    }

    if embedding
        .revision
        .as_ref()
        .is_some_and(|r| r.trim().is_empty())
    {
        issues.push(error(
            &format!("{key}.revision"),
            "写了 revision 就要写一个非空的版本标识；不写就整行去掉",
        ));
    }
}

fn check_retrieval(snapshot: &ConfigSnapshot, issues: &mut Vec<ConfigIssue>) {
    let retrieval = &snapshot.memory.retrieval;
    if retrieval.top_k == 0 {
        issues.push(error("memory.retrieval.top_k", "top_k 不能是 0"));
    }
    if retrieval.candidate_limit == 0 {
        issues.push(error(
            "memory.retrieval.candidate_limit",
            "candidate_limit 不能是 0",
        ));
    }
    if retrieval.top_k > retrieval.candidate_limit {
        issues.push(error(
            "memory.retrieval.top_k",
            format!(
                "top_k（{}）比 candidate_limit（{}）还大：注入不可能多于候选",
                retrieval.top_k, retrieval.candidate_limit
            ),
        ));
    }
    if retrieval.max_tokens == 0 {
        issues.push(error("memory.retrieval.max_tokens", "max_tokens 不能是 0"));
    }
    if retrieval.rerank {
        // 重排换的是"谁进得了 top_k"。短名单和 top_k 一样宽时它只是换个顺序——写了这个
        // 组合的人想要的是重排，拿到的却是一个空转的开关。
        if retrieval.rerank_shortlist <= retrieval.top_k {
            issues.push(error(
                "memory.retrieval.rerank_shortlist",
                format!(
                    "重排开着，但短名单（{}）不比 top_k（{}）宽：这样重排换不出任何本来进不了 \
                     top_k 的条目。把它调大，或者把 rerank 关掉",
                    retrieval.rerank_shortlist, retrieval.top_k
                ),
            ));
        }
        if retrieval.rerank_shortlist > retrieval.candidate_limit {
            issues.push(error(
                "memory.retrieval.rerank_shortlist",
                format!(
                    "重排短名单（{}）比 candidate_limit（{}）还大：每条臂最多也只会给这么多候选",
                    retrieval.rerank_shortlist, retrieval.candidate_limit
                ),
            ));
        }
        if !snapshot.typesafe.enabled {
            issues.push(error(
                "memory.retrieval.rerank",
                "重排开着，但判断后端（[typesafe]）没有 enabled：那一步没有后端可用",
            ));
        }
    }
}

/// `[typesafe]`（§9.4 的可选判断后端）。开着就要有端点、模型与那一条凭证。
fn check_typesafe(snapshot: &ConfigSnapshot, issues: &mut Vec<ConfigIssue>) {
    let typesafe = &snapshot.typesafe;
    if !typesafe.enabled {
        return;
    }
    if typesafe.endpoint.trim().is_empty() {
        issues.push(error("typesafe.endpoint", "endpoint 不能是空的"));
    }
    if typesafe.model.trim().is_empty() {
        issues.push(error("typesafe.model", "model 不能是空的"));
    }
    if typesafe.timeout_secs == 0 {
        issues.push(error("typesafe.timeout_secs", "超时不能是 0 秒"));
    }
    if typesafe.api_key.trim().is_empty() {
        issues.push(error("typesafe.api_key", "api_key 要写凭证的变量名"));
    } else if !snapshot.credentials.contains_key(&typesafe.api_key) {
        // 和模型后端同一个口径：快照里只记"这个变量有没有值"，值只在 `.env` 里。
        issues.push(error(
            &format!("typesafe.api_key ({})", typesafe.api_key),
            "`.env` 里没有这个名字的凭证",
        ));
    }
}

/// §6：Gateway 设置的输出长度。0 会让每一条工具结果都对模型空着——那不是一个配置，
/// 是一个静默的故障。
fn check_execution(snapshot: &ConfigSnapshot, issues: &mut Vec<ConfigIssue>) {
    if snapshot.execution.model_result_bytes == 0 {
        issues.push(error(
            "execution.model_result_bytes",
            "model_result_bytes 不能是 0（模型就什么都看不到了）",
        ));
    }
}

/// 助手定义（§四）。**没有隐含默认**，所以"一个都没声明"与"default_agent 指了个不存在的
/// id"都要在这里说出来——否则装配时才发现，操作者只看到一句"起不来"。
fn check_agents(snapshot: &ConfigSnapshot, issues: &mut Vec<ConfigIssue>) {
    let agent = &snapshot.agent;
    if agent.agents.is_empty() {
        issues.push(error(
            "agents",
            "至少要声明一个 Agent：写上 default_agent 与 [agents.<id>]（没有隐含的默认助手）",
        ));
        return;
    }
    for (id, profile) in &agent.agents {
        if profile.id != *id {
            issues.push(error(
                &format!("agents.{id}.id"),
                format!(
                    "这段的 id 是 `{}`，与它在表里的键 `{id}` 不一致",
                    profile.id
                ),
            ));
        }
        if id.trim().is_empty() {
            issues.push(error("agents.<id>", "Agent 的 id 不能是空的"));
        }
        if profile
            .instructions
            .as_ref()
            .is_some_and(|text| text.trim().is_empty())
        {
            issues.push(error(
                &format!("agents.{id}.instructions"),
                "写了 instructions 就不能是空的；不想要就整行去掉",
            ));
        }
    }
    if agent.default_agent.trim().is_empty() {
        issues.push(error(
            "default_agent",
            "要指名没有归属的入口（TUI / CLI / Cron）走哪个 Agent",
        ));
    } else if agent.get(&agent.default_agent).is_none() {
        issues.push(error(
            "default_agent",
            format!(
                "`{}` 不在 [agents.<id>] 里；现有的是：{}",
                agent.default_agent,
                agent.agents.keys().cloned().collect::<Vec<_>>().join("、")
            ),
        ));
    }
}

fn check_channels(snapshot: &ConfigSnapshot, issues: &mut Vec<ConfigIssue>) {
    for platform in [
        ChannelPlatform::Feishu,
        ChannelPlatform::Telegram,
        ChannelPlatform::Wechat,
    ] {
        let Some(channel) = snapshot.channels.get(platform) else {
            continue;
        };
        let key = format!("channels.{platform}");
        check_channel(channel, platform, &key, snapshot, issues);
    }
}

fn check_channel(
    channel: &ChannelConfig,
    platform: ChannelPlatform,
    key: &str,
    snapshot: &ConfigSnapshot,
    issues: &mut Vec<ConfigIssue>,
) {
    for (index, peer) in channel.allow_from.iter().enumerate() {
        if let Err(message) = check_peer_id(platform, peer) {
            issues.push(error(
                &format!("{key}.allow_from"),
                format!("[{index}] {message}"),
            ));
        }
    }
    for (index, peer) in channel.groups.iter().enumerate() {
        if let Err(message) = check_peer_id(platform, peer) {
            issues.push(error(
                &format!("{key}.groups"),
                format!("[{index}] {message}"),
            ));
        }
    }
    // §11.2：微信只有 DM。
    if platform == ChannelPlatform::Wechat && !channel.groups.is_empty() {
        issues.push(error(
            &format!("{key}.groups"),
            "微信只有私聊，没有可以响应的群",
        ));
    }

    if let Some(home) = &channel.home_chat {
        if let Err(message) = check_peer_id(platform, home) {
            issues.push(error(&format!("{key}.home_chat"), message));
        }
        // §11.2：home_chat 所属渠道必须 enabled 且有凭证。
        if !channel.enabled {
            issues.push(error(
                &format!("{key}.home_chat"),
                "这个渠道没有 enabled，它收不到主动投递",
            ));
        }
    }

    if channel.enabled {
        for var in channel_credentials(platform) {
            if !snapshot.credentials.contains_key(*var) {
                issues.push(error(
                    &format!("{key}.enabled"),
                    format!("渠道已启用，但 .env 里没有 `{var}`"),
                ));
            }
        }
        // §11.2：「enabled 但 allow_from 为空」= 只出不进，`komo doctor` 当作警告列出。
        if channel.is_outbound_only() {
            issues.push(warning(
                &format!("{key}.allow_from"),
                "名单为空：这个渠道只出不进（还能收主动投递，但没人能通过它下指令）",
            ));
        }
    }
}

/// 平台 id 的形态（§11.2 的三种写法）。
fn check_peer_id(platform: ChannelPlatform, peer: &PeerId) -> Result<(), String> {
    let raw = peer.as_str();
    if raw.trim().is_empty() {
        return Err("id 不能为空".into());
    }
    if raw.trim() != raw || raw.contains(char::is_whitespace) {
        return Err(format!("`{raw}` 里有空白字符"));
    }
    match platform {
        // 飞书的 open_id / chat_id：ou_ 用户、oc_ 群、on_ 外部用户、om_ 消息。
        ChannelPlatform::Feishu => {
            if ["ou_", "oc_", "on_", "om_"]
                .iter()
                .any(|prefix| raw.starts_with(prefix))
            {
                Ok(())
            } else {
                Err(format!(
                    "`{raw}` 不像飞书 id（open_id 以 ou_ 开头，群以 oc_ 开头）"
                ))
            }
        }
        // Telegram 的 user / chat id 是整数（群是负数）。
        ChannelPlatform::Telegram => raw
            .parse::<i64>()
            .map(|_| ())
            .map_err(|_| format!("`{raw}` 不是 Telegram 的整数 id")),
        // iLink 的 from：只要求是一个没有空白的字符串。
        ChannelPlatform::Wechat | ChannelPlatform::Api => Ok(()),
    }
}

fn check_policy(snapshot: &ConfigSnapshot, issues: &mut Vec<ConfigIssue>) {
    let mut seen: Vec<&str> = Vec::new();
    for rule in &snapshot.policy.rules {
        if rule.id.trim().is_empty() {
            issues.push(error(
                "policy.rules",
                "规则要有 id——决策理由里要指得出是哪一条放的行",
            ));
            continue;
        }
        if seen.contains(&rule.id.as_str()) {
            issues.push(error(
                "policy.rules",
                format!("规则 id `{}` 重复了", rule.id),
            ));
        }
        seen.push(&rule.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testing::snapshot_fixture;
    use komo_kernel::types::model::Effort;

    fn keys(issues: &[ConfigIssue]) -> Vec<&str> {
        issues.iter().map(|i| i.key.as_str()).collect()
    }

    #[test]
    fn a_healthy_snapshot_has_no_issues() {
        assert_eq!(validate(&snapshot_fixture()), vec![]);
    }

    #[test]
    fn an_unknown_provider_is_reported_at_the_provider_key_not_at_effort() {
        let mut snapshot = snapshot_fixture();
        let model = snapshot.model_catalog.completion_mut("chat").unwrap();
        model.provider = "unknown_chat_api".into();
        model.effort = Some(Effort::new("medium"));
        let issues = validate(&snapshot);
        assert_eq!(keys(&issues), vec!["model.chat.api_backend"]);
        assert!(
            issues[0].message.contains("chat_completions")
                && issues[0].message.contains("responses"),
            "{}",
            issues[0].message
        );
    }

    #[test]
    fn an_unsupported_effort_is_an_error_located_at_its_key() {
        let mut snapshot = snapshot_fixture();
        snapshot
            .model_catalog
            .completion_mut("chat")
            .unwrap()
            .effort = Some(Effort::new("ultra"));
        let issues = validate(&snapshot);
        assert_eq!(keys(&issues), vec!["model.chat.effort"]);
        assert_eq!(issues[0].severity, IssueSeverity::Error);
        assert!(issues[0].message.contains("ultra"), "{:?}", issues[0]);
    }

    #[test]
    fn a_missing_credential_variable_is_located_at_the_role_that_names_it() {
        let mut snapshot = snapshot_fixture();
        snapshot
            .model_catalog
            .completion_mut("memory")
            .unwrap()
            .api_key_env = "KOMO_NOT_IN_ENV".into();
        let issues = validate(&snapshot);
        assert_eq!(keys(&issues), vec!["model.memory.api_key_env"]);
    }

    #[test]
    fn every_broken_key_is_reported_in_one_pass() {
        let mut snapshot = snapshot_fixture();
        let model = snapshot.model_catalog.completion_mut("chat").unwrap();
        model.effort = Some(Effort::new("ultra"));
        model.base_url = "llm.example.com".into();
        snapshot.start_only.listen = "nowhere".into();
        let issues = validate(&snapshot);
        assert_eq!(
            keys(&issues),
            vec![
                "model.chat.base_url",
                "model.chat.effort",
                "start_only.listen"
            ]
        );
    }

    #[test]
    fn a_home_chat_on_a_disabled_channel_is_refused() {
        let mut snapshot = snapshot_fixture();
        snapshot.channels.feishu.enabled = false;
        let issues = validate(&snapshot);
        assert_eq!(keys(&issues), vec!["channels.feishu.home_chat"]);
    }

    #[test]
    fn an_enabled_channel_without_its_credentials_is_refused() {
        let mut snapshot = snapshot_fixture();
        snapshot.channels.telegram.enabled = true;
        snapshot.channels.telegram.allow_from = vec![PeerId::new("123456789")];
        let issues = validate(&snapshot);
        assert!(
            issues
                .iter()
                .any(|i| i.key.as_str() == "channels.telegram.enabled"
                    && i.message.contains("TELEGRAM_BOT_TOKEN")),
            "{issues:?}"
        );
    }

    #[test]
    fn a_malformed_platform_id_is_refused_with_its_index() {
        let mut snapshot = snapshot_fixture();
        snapshot.channels.feishu.allow_from = vec![PeerId::new("ou_ok"), PeerId::new("12345")];
        let issues = validate(&snapshot);
        assert_eq!(keys(&issues), vec!["channels.feishu.allow_from"]);
        assert!(issues[0].message.starts_with("[1]"), "{:?}", issues[0]);
    }

    #[test]
    fn an_outbound_only_channel_is_a_warning_not_an_error() {
        let mut snapshot = snapshot_fixture();
        snapshot.channels.feishu.allow_from.clear();
        let issues = validate(&snapshot);
        assert_eq!(keys(&issues), vec!["channels.feishu.allow_from"]);
        assert_eq!(issues[0].severity, IssueSeverity::Warning);
        assert!(!has_errors(&issues));
    }

    #[test]
    fn hybrid_retrieval_without_an_embedding_backend_is_refused() {
        let mut snapshot = snapshot_fixture();
        snapshot.memory.embedding = None;
        assert_eq!(keys(&validate(&snapshot)), vec!["memory.embedding"]);

        snapshot.memory.retrieval.mode = RetrievalMode::Keyword;
        assert_eq!(validate(&snapshot), vec![]);
    }

    /// §13.3：普通向量接口没有 effort 参数时必须省略——**哪怕聊天侧同名 provider
    /// 支持这一档**。
    #[test]
    fn a_vector_interface_without_an_effort_parameter_refuses_one() {
        for provider in ["ollama_embeddings", "embeddings"] {
            let mut snapshot = snapshot_fixture();
            let embedding = snapshot.model_catalog.embedding_mut("embedding").unwrap();
            embedding.model.provider = provider.into();
            embedding.model.effort = Some(Effort::new("low"));
            let issues = validate(&snapshot);
            assert_eq!(keys(&issues), vec!["model.embedding.effort"], "{provider}");
            assert!(
                issues[0].message.contains("没有 effort 参数"),
                "{:?}",
                issues[0]
            );
        }
    }

    #[test]
    fn a_nonsense_dimension_is_refused_before_the_index_is_built() {
        let mut snapshot = snapshot_fixture();
        snapshot
            .model_catalog
            .embedding_mut("embedding")
            .unwrap()
            .dimensions = Some(0);
        assert_eq!(
            keys(&validate(&snapshot)),
            vec!["model.embedding.dimensions"]
        );
    }

    #[test]
    fn top_k_larger_than_the_candidate_pool_is_refused() {
        let mut snapshot = snapshot_fixture();
        snapshot.memory.retrieval.top_k = 99;
        assert_eq!(keys(&validate(&snapshot)), vec!["memory.retrieval.top_k"]);
    }

    #[test]
    fn a_duplicate_policy_rule_id_is_refused() {
        let mut snapshot = snapshot_fixture();
        let first = snapshot.policy.rules[0].clone();
        snapshot.policy.rules.push(first);
        assert_eq!(keys(&validate(&snapshot)), vec!["policy.rules"]);
    }

    #[test]
    fn a_disabled_memory_section_skips_its_own_checks() {
        let mut snapshot = snapshot_fixture();
        snapshot.memory.enabled = false;
        snapshot.memory.embedding = None;
        snapshot.memory.model.api_key_env = "GONE".into();
        assert_eq!(validate(&snapshot), vec![]);
    }
}
