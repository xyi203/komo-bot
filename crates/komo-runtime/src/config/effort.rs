//! 各后端支持哪几档 effort（§13.3）。
//!
//! §13.3 的四条里有两条落在这个文件上：「不支持：返回具体错误，指出模型、配置位置及
//! 支持值；不静默忽略或强行映射为另一档」，以及「能力未知：需要显式能力声明……无法
//! 确定时拒绝该显式参数」。所以这里是一张**声明表**，不是一个猜测函数：认识的后端列
//! 出它的档位，不认识的后端答"未知"，而未知 + 显式 effort = 拒绝。
//!
//! 两级声明，操作者的那一级在上：配置里写了
//! [`ModelConfig::efforts`](komo_kernel::types::model::ModelConfig::efforts) 就以它为准
//! （那就是 §13.3 说的"显式能力声明"），没人写过才问下面这张内建表。

use std::collections::BTreeMap;

use komo_kernel::types::model::{Effort, ModelConfig};

/// 一个后端的档位声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortSupport {
    pub(super) levels: Vec<Effort>,
}

impl EffortSupport {
    pub fn new<I: IntoIterator<Item = S>, S: AsRef<str>>(levels: I) -> Self {
        EffortSupport {
            levels: levels.into_iter().map(Effort::new).collect(),
        }
    }

    /// 这个接口根本没有 effort 参数（普通向量接口就是，§13.3）。
    pub fn none() -> Self {
        EffortSupport { levels: Vec::new() }
    }

    pub fn accepts(&self, effort: &Effort) -> bool {
        self.levels.contains(effort)
    }

    pub fn levels(&self) -> &[Effort] {
        &self.levels
    }

    /// 给错误消息用的"支持值"。
    pub fn describe(&self) -> String {
        if self.levels.is_empty() {
            "（这个接口没有 effort 参数）".to_string()
        } else {
            self.levels
                .iter()
                .map(Effort::as_str)
                .collect::<Vec<_>>()
                .join(" / ")
        }
    }
}

/// provider / 模型 → 档位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortCapabilities {
    providers: BTreeMap<String, EffortSupport>,
    /// **向量后端的表是另一张**：同一个 provider 名下，聊天接口有 `reasoning_effort`
    /// 而普通向量接口没有这个参数（§13.3：「dimensions、批大小和输入长度是另一组控制，
    /// 不用 effort 冒充」）。共用一张表就会让"聊天支持 high"替向量端点作保。
    embedding_providers: BTreeMap<String, EffortSupport>,
    /// 模型名**前缀** → 档位。同一 provider 下不同模型家族的档位确实不同（§13.3）。
    model_prefixes: Vec<(String, EffortSupport)>,
}

impl Default for EffortCapabilities {
    fn default() -> Self {
        Self::builtin()
    }
}

impl EffortCapabilities {
    /// 空表：什么都不认识，于是任何显式 effort 都被拒绝。最保守的起点。
    pub fn empty() -> Self {
        EffortCapabilities {
            providers: BTreeMap::new(),
            embedding_providers: BTreeMap::new(),
            model_prefixes: Vec::new(),
        }
    }

    /// 内建声明。
    ///
    /// - `responses` 与 `chat_completions` 都支持常见 effort 档位；具体模型可用
    ///   `efforts` 进一步收窄。
    /// - `deepseek-*` 模型：`none / low / high / max`，没有 `medium`。
    /// - 向量后端（下面那张表）：`/embeddings` 与 `/api/embed` 都没有 effort 参数。
    pub fn builtin() -> Self {
        EffortCapabilities {
            providers: BTreeMap::from([
                (
                    crate::llm::RESPONSES.to_string(),
                    EffortSupport::new(["none", "minimal", "low", "medium", "high"]),
                ),
                (
                    "openai_responses".to_string(),
                    EffortSupport::new(["none", "minimal", "low", "medium", "high"]),
                ),
                (
                    crate::llm::CHAT_COMPLETIONS.to_string(),
                    EffortSupport::new(["none", "minimal", "low", "medium", "high"]),
                ),
            ]),
            // 两个实现了的向量后端（`/embeddings` 与 `/api/embed`）都没有 effort 参数。
            embedding_providers: BTreeMap::from([
                (
                    crate::embedding::OPENAI_COMPATIBLE.to_string(),
                    EffortSupport::none(),
                ),
                ("openai_compatible".to_string(), EffortSupport::none()),
                (crate::embedding::OLLAMA.to_string(), EffortSupport::none()),
                ("ollama".to_string(), EffortSupport::none()),
            ]),
            model_prefixes: vec![(
                "deepseek".to_string(),
                EffortSupport::new(["none", "low", "high", "max"]),
            )],
        }
    }

    /// 这个生成协议有人声明过吗。
    pub fn knows_provider(&self, provider: &str) -> bool {
        self.providers.contains_key(provider)
    }

    /// 这个向量后端有人声明过吗。
    pub fn knows_embedding_provider(&self, provider: &str) -> bool {
        self.embedding_providers.contains_key(provider)
    }

    /// 补一个 provider 的声明。
    pub fn with_provider(mut self, provider: impl Into<String>, support: EffortSupport) -> Self {
        self.providers.insert(provider.into(), support);
        self
    }

    /// 补一个**向量**后端的声明。
    pub fn with_embedding_provider(
        mut self,
        provider: impl Into<String>,
        support: EffortSupport,
    ) -> Self {
        self.embedding_providers.insert(provider.into(), support);
        self
    }

    /// 补一个模型前缀的声明。它比 provider 的声明更具体，先匹配。
    pub fn with_model(mut self, prefix: impl Into<String>, support: EffortSupport) -> Self {
        self.model_prefixes.push((prefix.into(), support));
        self
    }

    /// 这个模型支持哪几档；`None` = **能力未知**。
    pub fn support(&self, config: &ModelConfig) -> Option<&EffortSupport> {
        self.model_prefixes
            .iter()
            .find(|(prefix, _)| config.model.starts_with(prefix.as_str()))
            .map(|(_, support)| support)
            .or_else(|| self.providers.get(config.provider.as_str()))
    }

    /// 向量角色支持哪几档；`None` = 能力未知。
    pub fn embedding_support(&self, config: &ModelConfig) -> Option<&EffortSupport> {
        self.embedding_providers.get(config.provider.as_str())
    }

    /// 校验一份配置里的 effort。`Ok(())` = 没配 effort，或者配了而且支持。
    pub fn check(&self, config: &ModelConfig) -> Result<(), EffortProblem> {
        self.check_against(config, self.support(config))
    }

    /// 同上，但按**向量**角色的声明判（§13.3）。
    pub fn check_embedding(&self, config: &ModelConfig) -> Result<(), EffortProblem> {
        self.check_against(config, self.embedding_support(config))
    }

    fn check_against(
        &self,
        config: &ModelConfig,
        support: Option<&EffortSupport>,
    ) -> Result<(), EffortProblem> {
        let Some(effort) = &config.effort else {
            return Ok(());
        };
        // §13.3 的"显式能力声明"：配置里写了 `efforts` 就以它为准，内建表只是没人说过
        // 时的回答。
        match config.declares_effort(effort) {
            Some(true) => return Ok(()),
            Some(false) => {
                return Err(EffortProblem::Unsupported {
                    model: config.model.clone(),
                    effort: effort.clone(),
                    supported: EffortSupport {
                        levels: config.efforts.clone().unwrap_or_default(),
                    }
                    .describe(),
                });
            }
            None => {}
        }
        match support {
            None => Err(EffortProblem::UnknownCapability {
                model: config.model.clone(),
                provider: config.provider.clone(),
                effort: effort.clone(),
            }),
            Some(support) if support.accepts(effort) => Ok(()),
            Some(support) => Err(EffortProblem::Unsupported {
                model: config.model.clone(),
                effort: effort.clone(),
                supported: support.describe(),
            }),
        }
    }
}

/// effort 配错了。消息里有模型、档位与支持值——位置（键路径）由调用方补上，因为同
/// 一份 `ModelConfig` 会出现在三个角色下。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EffortProblem {
    #[error("模型 {model} 不支持 effort = {effort}；支持的是 {supported}")]
    Unsupported {
        model: String,
        effort: Effort,
        supported: String,
    },
    /// §13.3：「能力未知……无法确定时拒绝该显式参数」。
    #[error(
        "不认识 provider `{provider}` 的模型 {model}，无法确认它支持 effort = {effort}；\
         去掉这个键（由服务端默认），或为这个后端补一份档位声明"
    )]
    UnknownCapability {
        model: String,
        provider: String,
        effort: Effort,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, name: &str, effort: Option<&str>) -> ModelConfig {
        ModelConfig {
            provider: provider.into(),
            base_url: "https://x/v1".into(),
            model: name.into(),
            api_key_env: "K".into(),
            auth: None,
            effort: effort.map(Effort::new),
            efforts: None,
            timeout_secs: 120,
        }
    }

    #[test]
    fn an_unset_effort_never_trips_the_check() {
        let caps = EffortCapabilities::builtin();
        assert!(caps.check(&model("whoever", "whatever", None)).is_ok());
    }

    #[test]
    fn an_unsupported_level_names_the_model_and_the_supported_ones() {
        let caps = EffortCapabilities::builtin();
        let error = caps
            .check(&model("openai_responses", "gpt-x", Some("max")))
            .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("gpt-x"), "{text}");
        assert!(text.contains("max"), "{text}");
        assert!(text.contains("medium"), "支持值要印出来：{text}");
    }

    #[test]
    fn a_model_family_declaration_beats_its_providers() {
        let caps = EffortCapabilities::builtin();
        // DeepSeek 没有 medium，哪怕它走的也是 Responses 协议。
        assert!(
            caps.check(&model(
                "openai_responses",
                "deepseek-v4-pro",
                Some("medium")
            ))
            .is_err()
        );
        assert!(
            caps.check(&model("openai_responses", "deepseek-v4-pro", Some("max")))
                .is_ok()
        );
    }

    #[test]
    fn an_unknown_backend_refuses_an_explicit_effort_rather_than_guessing() {
        let caps = EffortCapabilities::builtin();
        let error = caps
            .check(&model("brand_new", "m", Some("low")))
            .unwrap_err();
        assert!(matches!(error, EffortProblem::UnknownCapability { .. }));
    }

    #[test]
    fn an_interface_without_the_parameter_says_so() {
        let caps = EffortCapabilities::builtin();
        let error = caps
            .check_embedding(&model("ollama", "embeddinggemma", Some("low")))
            .unwrap_err();
        assert!(error.to_string().contains("没有 effort 参数"), "{error}");
    }

    /// 配置里写了 `efforts` 就以它为准——§13.3 的"显式能力声明"。
    #[test]
    fn an_operators_declaration_beats_the_builtin_table_both_ways() {
        let caps = EffortCapabilities::builtin();

        let mut declared = model("brand_new", "m", Some("low"));
        declared.efforts = Some(vec![Effort::new("low"), Effort::new("high")]);
        assert!(caps.check(&declared).is_ok(), "不认识的后端也能靠声明过关");

        let mut refused = model("openai_responses", "gpt-x", Some("high"));
        refused.efforts = Some(vec![Effort::new("low")]);
        let error = caps.check(&refused).unwrap_err();
        assert!(
            matches!(error, EffortProblem::Unsupported { .. }),
            "声明里没有就是没有：{error}"
        );
        assert!(error.to_string().contains("low"), "{error}");
    }

    #[test]
    fn a_caller_can_declare_a_backend_the_builtin_table_does_not_know() {
        let caps = EffortCapabilities::builtin()
            .with_provider("brand_new", EffortSupport::new(["low", "high"]));
        assert!(caps.check(&model("brand_new", "m", Some("low"))).is_ok());
        assert!(
            caps.check(&model("brand_new", "m", Some("medium")))
                .is_err()
        );
    }
}
