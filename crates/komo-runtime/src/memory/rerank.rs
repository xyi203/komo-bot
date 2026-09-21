//! 用判断后端给召回短名单排序（§9.4 的可选一步）。
//!
//! 关键词 + 向量融合（RRF）负责**找出候选**，但它排不出细微的先后——两条描述同一件事的
//! 记忆在 RRF 里可能只差一位。判断层看的是"这条与这次输入的相关性"本身，于是它能把真正
//! 该想起来的那条抬到前面。**只重排，不增删**：候选集合与融合结果逐条相同，变的只是顺序
//! ——也就是"谁进得了 `top_k`"。

use std::collections::BTreeMap;

use komo_kernel::traits::SystemOne;
use komo_kernel::types::memory::MemoryItem;
use komo_kernel::types::systemone::{Answer, Question, SystemOneError, SystemOneRequest};

/// 送进判断层的每条候选最多带多少字符。判断只需要认出"这条讲的是什么"，不需要全文；
/// 而且这些正文会被发到第三方（§9.4 的代价写在配置注释里）。
const SUMMARY_CHARS: usize = 200;

/// 问句的 id。只用于对号，不进模型。
const QUESTION: &str = "most_relevant";

/// `criteria` 里代表"都不相关"的那个选项。
const NOTHING: &str = "none";

/// 让判断后端给这批候选排序。
///
/// 返回 `None` = **不采纳它的排序**（它挑中了"都不相关"）——不是错误。失败是
/// [`SystemOneError`]，调用方据此分开记日志："模型说都不相关"与"判断层这一次不可用"
/// 是两件事，混成一句会让下一次调阈值的人看错证据。
pub(super) async fn order(
    backend: &dyn SystemOne,
    model: &str,
    input: &str,
    items: &[MemoryItem],
) -> Result<Option<Vec<String>>, SystemOneError> {
    let mut candidates: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut criteria: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for item in items {
        let summary = summary(&item.content);
        candidates.insert(item.id.to_string(), serde_json::json!(summary.clone()));
        criteria.insert(item.id.to_string(), serde_json::json!(summary));
    }
    criteria.insert(NOTHING.to_string(), serde_json::Value::Null);

    let request = SystemOneRequest {
        state: serde_json::json!({ "input": input, "candidates": candidates }),
        model: model.to_string(),
        questions: BTreeMap::from([(
            QUESTION.to_string(),
            Question::Choice {
                instructions: serde_json::json!(
                    "`input` 是用户这一次说的话。`candidates` 里哪一条最该在这次回答之前\
                     想起来？只按相关性挑一条；都不相关就选 `none`。"
                ),
                criteria,
            },
        )]),
    };

    let response = backend.ask(request).await?;
    let Some(Answer::Choice {
        choice,
        probabilities,
        ..
    }) = response.answers.get(QUESTION)
    else {
        return Err(SystemOneError::Decode(format!(
            "答复不是我们问的那个 Choice：{:?}",
            response.answers.get(QUESTION)
        )));
    };
    if choice == NOTHING {
        return Ok(None);
    }

    // 概率降序；答复里没提到的 id 按 0 算，且**稳定排序**让它们保持原来的相对顺序。
    let probability = |id: &str| probabilities.get(id).copied().unwrap_or(0.0);
    let mut ranked: Vec<String> = items.iter().map(|item| item.id.to_string()).collect();
    ranked.sort_by(|a, b| {
        probability(b)
            .partial_cmp(&probability(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Some(ranked))
}

/// 按判断给的顺序重排；它没提到的条目留在后面，保持原相对顺序（`sort_by` 是稳定的，
/// 这里用同一招）。
pub(super) fn apply_order(items: Vec<MemoryItem>, order: &[String]) -> Vec<MemoryItem> {
    let rank = |item: &MemoryItem| {
        order
            .iter()
            .position(|id| id == item.id.as_str())
            .unwrap_or(usize::MAX)
    };
    let mut items = items;
    items.sort_by_key(rank);
    items
}

/// 一行摘要：空白压成一个空格，超长截断（按字符，不劈开一个汉字）。
fn summary(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SUMMARY_CHARS {
        return flat;
    }
    let mut out: String = flat.chars().take(SUMMARY_CHARS).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_is_one_line_and_never_splits_a_character() {
        assert_eq!(summary("喜欢  深色\n主题"), "喜欢 深色 主题");
        let long = "汉".repeat(SUMMARY_CHARS + 10);
        let short = summary(&long);
        assert_eq!(short.chars().count(), SUMMARY_CHARS + 1);
        assert!(short.ends_with('…'));
    }
}

#[cfg(test)]
mod live {
    //! 真机验收：`cargo test -p komo-runtime --all-features --lib rerank::live -- --ignored`。
    //!
    //! 需要网络与 `.env` 里的 `TYPESAFE_API_KEY`（用它 `export`，测试不从文件里读凭证）。
    //! 它证明的是**整条路**：真的 Jev 把"该想起来的那条"挑了出来，而不是只有 HTTP 通。

    use time::OffsetDateTime;

    use komo_kernel::protocol::config::TypesafeConfig;
    use komo_kernel::types::ids::MemoryId;
    use komo_kernel::types::memory::{
        Confirmation, ExtractionMetadata, MemoryItem, MemoryKind, MemoryScope, MemoryState,
        Provenance,
    };

    use super::*;
    use crate::config::Secrets;

    fn item(id: &str, content: &str) -> MemoryItem {
        MemoryItem {
            id: MemoryId::from_raw(id),
            revision: 1,
            content: content.into(),
            kind: MemoryKind::Preference,
            scope: MemoryScope::Personal,
            provenance: Provenance::UserStatement,
            confirmation: Confirmation::Unconfirmed,
            state: MemoryState::Active,
            evidence: vec![],
            observed_at: OffsetDateTime::now_utc(),
            valid_until: None,
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
            extraction: ExtractionMetadata::new("memory-model", None, "v1"),
            usage: Default::default(),
            supersedes: None,
        }
    }

    #[tokio::test]
    #[ignore = "真机：要网络与 TYPESAFE_API_KEY"]
    async fn a_real_jev_call_lifts_the_relevant_memory() {
        let Some(key) = std::env::var("TYPESAFE_API_KEY").ok() else {
            panic!("先 export TYPESAFE_API_KEY");
        };
        // `reqwest` 用的是 rustls-no-provider：装 provider 是进程的事（§13.4，bin 的
        // `main` 里那一行）。测试进程自己装一次，装过就算了。
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = TypesafeConfig {
            enabled: true,
            ..TypesafeConfig::default()
        };
        let secrets = Secrets::from_pairs([("TYPESAFE_API_KEY", key.as_str())]);
        let backend = crate::typesafe::connect(&config, &secrets, crate::llm::default_transport())
            .expect("配好了就造得出来");

        let items = vec![
            item("m-1", "用户住处的空调只在夏天用，平时设定 26 度。"),
            item(
                "m-2",
                "家里的 Home Assistant 实例地址是 http://192.168.50.9:8123。",
            ),
            item("m-3", "用户偏好：下厨时不喜欢被打断。"),
            item("m-4", "用户提到过热水器是燃气式的，开关在阳台。"),
        ];
        let order = order(
            backend.as_ref(),
            &config.model,
            "帮我把客厅空调调到 24 度，顺便看看热水器开着没",
            &items,
        )
        .await
        .expect("真机这一路要通");
        let order = order.expect("不该是「都不相关」");
        println!("jev 给的顺序：{order:?}");
        assert!(
            order[0] == "m-4" || order[0] == "m-1",
            "空调 / 热水器那两条该排在前面，实际：{order:?}"
        );
    }
}
