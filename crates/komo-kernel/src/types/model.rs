//! 模型角色、effort 与向量空间（§13.3、§9.5）。

use std::fmt;

use serde::{Deserialize, Serialize};

use super::digest::ContentHash;

/// 三个角色各自独立解析配置，不互相继承运行时覆盖（§13.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    /// 对话、计划、tool call 与任务输出。Session / Cron 可覆盖本角色。
    Main,
    /// 记忆提取、去重和冲突整理。
    Memory,
    /// 记忆文本与检索查询的向量生成。不继承聊天模型。
    Embedding,
}

impl ModelRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelRole::Main => "model",
            ModelRole::Memory => "memory.model",
            ModelRole::Embedding => "memory.embedding",
        }
    }
}

impl fmt::Display for ModelRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一档推理强度。
///
/// §13.3：「low / medium / high / max 等值不是所有模型共享的固定集合」，所以这是一个
/// 规范化过的字符串而不是封闭枚举——把它写死成五个变体，等于替每个后来的模型宣布
/// 它支持哪几档。常量给出常见的几档，适配器按目标模型的能力声明校验。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Effort(String);

impl Effort {
    pub const NONE: &'static str = "none";
    pub const LOW: &'static str = "low";
    pub const MEDIUM: &'static str = "medium";
    pub const HIGH: &'static str = "high";
    pub const MAX: &'static str = "max";

    /// 规范化：去空白、小写。
    pub fn new(raw: impl AsRef<str>) -> Self {
        Self(raw.as_ref().trim().to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 一次请求**实际**用了什么强度，记进日志用（§13.3 最后一段）。
///
/// 「未配置」和「显式 none」不是一回事：未配置时不发送 effort 字段，由服务端采用默认
/// 行为，日志只能记"服务端默认"，不能虚构当时采用的强度。
/// **邻接标记，不是内部标记。**`Explicit` 装的是一个 newtype（序列化成字符串），而
/// serde 的内部标记（`tag = "kind"` 单独用）序列化不了"装着字符串的 newtype 变体"——
/// 它会在运行时报 `cannot serialize tagged newtype variant`。`content = "effort"` 把值
/// 放进自己的一格，于是 `{"kind":"explicit","effort":"low"}` 写得出也读得回，而
/// `ProviderDefault` 仍然是 `{"kind":"provider_default"}`，与之前逐字一致（那是唯一
/// 写得出去过的形态，所以这不是一次线上格式变更）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "effort", rename_all = "snake_case")]
pub enum EffortSetting {
    /// 没有配置：请求里不带这个字段。
    ProviderDefault,
    Explicit(Effort),
}

impl EffortSetting {
    pub fn from_option(effort: Option<Effort>) -> Self {
        match effort {
            Some(e) => EffortSetting::Explicit(e),
            None => EffortSetting::ProviderDefault,
        }
    }

    pub fn as_option(&self) -> Option<&Effort> {
        match self {
            EffortSetting::ProviderDefault => None,
            EffortSetting::Explicit(e) => Some(e),
        }
    }
}

/// 所有模型角色复用的公共字段（§13.3）。凭证只以**变量名**出现。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    /// 凭证所在的环境变量名。值不在快照里。
    pub api_key_env: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// 操作者显式声明这个模型支持哪些档位。
    ///
    /// §13.3 的「能力未知：需要显式能力声明或在实际启用前完成最小探测。无法确定时拒绝
    /// 该显式参数」——这就是那个显式声明。`None` **不是"没有档位"**，而是"没人在配置
    /// 里说过，问适配器自己的内建表去"；写成空 `Vec` 才是"这个模型一档都不支持"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<Effort>>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl ModelConfig {
    /// 配置里这个 effort 立不立得住。`efforts` 没声明就答 `None`——**不是 `true`**：
    /// 「无法确定时拒绝该显式参数」，由适配器拿自己的能力表来答。
    pub fn declares_effort(&self, effort: &Effort) -> Option<bool> {
        self.efforts
            .as_ref()
            .map(|declared| declared.contains(effort))
    }
}

fn default_timeout_secs() -> u64 {
    120
}

/// Embedding 在公共字段之外多出的三项（§13.3、§9.5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    #[serde(flatten)]
    pub model: ModelConfig,
    /// 服务端同名模型的权重版本，由操作者指定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// 省略时使用模型返回的维度，校验后固定到索引代次。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    /// 文档侧的输入前缀，例如 e5 的 `passage: `、bge 的 `Represent this sentence…`。
    ///
    /// §9.5：「查询与文档必须使用同一空间规定的模型与**各自**输入规则」，而这两条规则
    /// 进空间指纹——同一个模型换了前缀就是另一个空间，旧向量不能拿来比。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_prefix: Option<String>,
    /// 查询侧的输入前缀，例如 e5 的 `query: `。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_prefix: Option<String>,
}

/// 向量空间指纹（§9.5）。同维度不代表同一空间；凭证值不进入指纹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingSpace {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub dimensions: u32,
    /// 文本预处理版本。
    pub preprocessing: String,
    /// 文档 / 查询各自的前缀规则。
    #[serde(default)]
    pub document_prefix: String,
    #[serde(default)]
    pub query_prefix: String,
    pub normalized: bool,
    pub distance: DistanceRule,
    /// 影响向量生成的 effort（接口支持时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

impl EmbeddingSpace {
    /// 指纹：整份结构的规范化哈希。两个空间只要有一个字段不同就不是同一个空间。
    pub fn fingerprint(&self) -> ContentHash {
        ContentHash::of_json(self).expect("EmbeddingSpace 的每个字段都可序列化")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistanceRule {
    Cosine,
}

/// 文档侧还是查询侧——两者的输入规则不同，必须用同一空间规定的那一套（§9.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Document,
    Query,
}

/// 一条向量。存储时是 f32 + 维度 + 代次。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vector(pub Vec<f32>);

impl Vector {
    pub fn dimensions(&self) -> usize {
        self.0.len()
    }

    /// 校验：维度对得上、数值有限、范数非零（§9.5）。截断或结构错误的向量不接受。
    pub fn is_usable(&self, expected_dimensions: u32) -> bool {
        self.0.len() == expected_dimensions as usize
            && self.0.iter().all(|v| v.is_finite())
            && self.0.iter().any(|v| *v != 0.0)
    }
}

/// 一次模型往返的用量。结果与用量都未知时保留未知标记，不能当成零（§8.5）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
}

impl TokenUsage {
    /// 用量未知——不是零。
    pub fn is_unknown(&self) -> bool {
        self.input.is_none() && self.output.is_none() && self.reasoning.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_is_normalized() {
        assert_eq!(Effort::new(" High ").as_str(), "high");
    }

    #[test]
    fn unset_effort_is_not_the_same_as_explicit_none() {
        let unset = EffortSetting::from_option(None);
        let explicit_none = EffortSetting::from_option(Some(Effort::new(Effort::NONE)));
        assert_ne!(unset, explicit_none);
        assert!(unset.as_option().is_none());
        assert_eq!(explicit_none.as_option().map(|e| e.as_str()), Some("none"));
    }

    /// 两个变体都要**写得出去、读得回来**。
    ///
    /// `Explicit` 曾经写不出去：内部标记 + 装着字符串的 newtype 变体，serde 在运行时
    /// 报错，于是任何带 effort 的 `ExtractionMetadata` 都存不进 `memory_items`。回归就
    /// 是这一条。
    #[test]
    fn both_effort_settings_survive_a_json_round_trip() {
        for setting in [
            EffortSetting::ProviderDefault,
            EffortSetting::Explicit(Effort::new("low")),
        ] {
            let json = serde_json::to_string(&setting).expect("写得出去");
            let back: EffortSetting = serde_json::from_str(&json).expect("读得回来");
            assert_eq!(back, setting, "{json}");
        }
        // 「未配置」那一格的写法与从前逐字一致——它是唯一写得出去过的形态。
        assert_eq!(
            serde_json::to_string(&EffortSetting::ProviderDefault).unwrap(),
            r#"{"kind":"provider_default"}"#
        );
    }

    #[test]
    fn spaces_with_the_same_dimensions_but_different_models_are_different() {
        let base = EmbeddingSpace {
            provider: "openai_compatible".into(),
            endpoint: "https://e/v1".into(),
            model: "a".into(),
            revision: None,
            dimensions: 1024,
            preprocessing: "v1".into(),
            document_prefix: String::new(),
            query_prefix: String::new(),
            normalized: true,
            distance: DistanceRule::Cosine,
            effort: None,
        };
        let mut other = base.clone();
        other.model = "b".into();
        assert_ne!(base.fingerprint(), other.fingerprint());

        let mut same_model_new_weights = base.clone();
        same_model_new_weights.revision = Some("2026-09".into());
        assert_ne!(base.fingerprint(), same_model_new_weights.fingerprint());
    }

    #[test]
    fn an_undeclared_effort_list_is_not_an_empty_one() {
        let mut config = ModelConfig {
            provider: "openai_compatible".into(),
            base_url: "https://x/v1".into(),
            model: "m".into(),
            api_key_env: "K".into(),
            effort: None,
            efforts: None,
            timeout_secs: 120,
        };
        assert_eq!(
            config.declares_effort(&Effort::new("high")),
            None,
            "没人声明过，配置说不出话，问适配器去"
        );

        config.efforts = Some(vec![]);
        assert_eq!(
            config.declares_effort(&Effort::new("high")),
            Some(false),
            "空表是「一档都不支持」"
        );

        config.efforts = Some(vec![Effort::new("low"), Effort::new("high")]);
        assert_eq!(config.declares_effort(&Effort::new("high")), Some(true));
        assert_eq!(config.declares_effort(&Effort::new("max")), Some(false));
    }

    #[test]
    fn a_model_written_before_efforts_existed_reads_as_undeclared() {
        let json = r#"{"provider":"p","base_url":"u","model":"m","api_key_env":"K"}"#;
        let config: ModelConfig = serde_json::from_str(json).unwrap();
        assert!(config.efforts.is_none());
        assert_eq!(config.timeout_secs, 120);
    }

    #[test]
    fn changing_an_input_prefix_changes_the_space() {
        let base = EmbeddingSpace {
            provider: "p".into(),
            endpoint: "https://e/v1".into(),
            model: "e5".into(),
            revision: None,
            dimensions: 1024,
            preprocessing: "v1".into(),
            document_prefix: "passage: ".into(),
            query_prefix: "query: ".into(),
            normalized: true,
            distance: DistanceRule::Cosine,
            effort: None,
        };
        let mut swapped = base.clone();
        swapped.query_prefix = String::new();
        assert_ne!(
            base.fingerprint(),
            swapped.fingerprint(),
            "前缀是空间的一部分，不是调用处的装饰"
        );
    }

    #[test]
    fn a_truncated_or_zero_vector_is_not_usable() {
        assert!(Vector(vec![0.1, 0.2]).is_usable(2));
        assert!(!Vector(vec![0.1]).is_usable(2), "维度不符");
        assert!(!Vector(vec![0.0, 0.0]).is_usable(2), "零范数");
        assert!(!Vector(vec![f32::NAN, 0.2]).is_usable(2), "非有限数值");
    }

    #[test]
    fn unknown_usage_is_not_zero() {
        assert!(TokenUsage::default().is_unknown());
        assert!(
            !TokenUsage {
                input: Some(0),
                ..Default::default()
            }
            .is_unknown()
        );
    }
}
