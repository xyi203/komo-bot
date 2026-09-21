//! TypeSafe「System One」判断层的线上格式（§9.4 的可选判断后端）。
//!
//! 契约照 <https://docs.typesafe.ai/api.md>：一次请求带一份 `state` 与一张 `questions`
//! 表，回来的是同名的 `answers`。**它不是一个对话模型**：输出不是文本，而是几个能直接
//! 进代码的数——概率、分数、选择。所以它在这里是纯值类型，HTTP 在 runtime（§13.4）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 一次判断请求。`state` 可以是字符串、对象或数组；问题按 `id` 命名，答复按同一个 `id`
/// 回来（`id` 不发进模型，只为对号）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemOneRequest {
    pub state: serde_json::Value,
    pub model: String,
    pub questions: BTreeMap<String, Question>,
}

/// 三种问句，由 `type` 定形。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// 一个是 / 否问题：回来的是"是"的概率（0–1）。
    Noul {
        instructions: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// 从一组选项里挑一个：回来的是选项与**整张概率分布**，所以它同时是一张排序表。
    Choice {
        instructions: serde_json::Value,
        criteria: BTreeMap<String, serde_json::Value>,
    },
    /// 沿一组有序档位打分：回来的是概率加权的位置。
    Score {
        instructions: serde_json::Value,
        criteria: Vec<serde_json::Value>,
    },
}

/// 是 / 否各自是什么意思。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<serde_json::Value>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<serde_json::Value>,
}

/// 一次判断的答复。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemOneResponse {
    /// 真正干活的模型（例如 `jev-1.13.0`）。记下来，事后答得出这一条判断是谁给的。
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: SystemOneUsage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemOneUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// 一条答复，形状跟着它的问句。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Noul {
        /// "是"的概率。
        noul: f64,
    },
    Choice {
        /// 概率最高的那个选项。
        choice: String,
        /// 每个选项的概率。**排序就靠它**——挑一个只是它的 argmax。
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    Score {
        /// 概率加权的位置，可能落在两档之间。
        score: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        #[serde(default)]
        confidence: Option<f64>,
    },
}

impl Answer {
    /// 这条答复属于哪一问——形状对不上就是解析出了错，交调用方决定怎么办。
    pub fn kind(&self) -> &'static str {
        match self {
            Answer::Noul { .. } => "noul",
            Answer::Choice { .. } => "choice",
            Answer::Score { .. } => "score",
        }
    }
}

/// 判断后端这一次没给出结果。
///
/// **它不该让主流程失败**：判断层是加分项（§9.4 的重排关掉照样能召回），所以调用方
/// 拿到它一律降级到"没有判断"的那条路，并把它写进日志——把"判断不可用"说成"没有相关
/// 记忆"才是不能犯的错。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SystemOneError {
    #[error("判断后端连不上：{0}")]
    Transport(String),
    #[error("判断后端回了 {status}：{message}")]
    Status { status: u16, message: String },
    #[error("判断后端的答复看不懂：{0}")]
    Decode(String),
    #[error("判断后端没配好：{0}")]
    Unconfigured(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 文档给的例子：请求照原样发得出去，答复照原样解得开。
    #[test]
    fn the_documented_exchange_round_trips() {
        let request = SystemOneRequest {
            state: serde_json::json!("Help! My payouts have been failing for 3 days."),
            model: "jev-latest".into(),
            questions: BTreeMap::from([(
                "is_urgent".to_string(),
                Question::Noul {
                    instructions: serde_json::json!("Does this convey urgency?"),
                    criteria: Some(NoulCriteria {
                        yes: Some(serde_json::json!("Explicitly time-sensitive")),
                        no: Some(serde_json::json!("No urgency expressed")),
                    }),
                },
            )]),
        };
        let sent = serde_json::to_value(&request).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({
                "state": "Help! My payouts have been failing for 3 days.",
                "model": "jev-latest",
                "questions": {
                    "is_urgent": {
                        "type": "noul",
                        "instructions": "Does this convey urgency?",
                        "criteria": { "true": "Explicitly time-sensitive", "false": "No urgency expressed" }
                    }
                }
            })
        );

        let response: SystemOneResponse = serde_json::from_value(serde_json::json!({
            "model": "jev-1.13.0",
            "answers": { "is_urgent": { "type": "noul", "noul": 0.95 } },
            "usage": { "input_tokens": 296, "output_tokens": 20 }
        }))
        .unwrap();
        assert_eq!(response.model, "jev-1.13.0");
        assert_eq!(response.usage.input_tokens, 296);
        assert!(matches!(
            response.answers["is_urgent"],
            Answer::Noul { noul } if (noul - 0.95).abs() < 1e-9
        ));
    }

    /// Choice 的整张概率分布要留住——重排用的就是它，不是 `choice` 那一个名字。
    #[test]
    fn a_choice_answer_keeps_the_whole_distribution() {
        let response: SystemOneResponse = serde_json::from_value(serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "most_relevant": {
                    "type": "choice",
                    "choice": "billing",
                    "probabilities": { "billing": 0.88, "technical": 0.12, "sales": 0.0 },
                    "confidence": 0.81
                }
            }
        }))
        .unwrap();
        let Answer::Choice {
            choice,
            probabilities,
            confidence,
        } = &response.answers["most_relevant"]
        else {
            panic!("{:?}", response.answers["most_relevant"])
        };
        assert_eq!(choice, "billing");
        assert_eq!(probabilities.len(), 3);
        assert_eq!(confidence, &Some(0.81));
    }

    /// Choice 的选项值可以是 null，Noul 的 criteria 可以整块省掉。
    #[test]
    fn optional_pieces_may_be_left_out() {
        let request = SystemOneRequest {
            state: serde_json::json!({ "input": "把空调调到 24 度" }),
            model: "jev-latest".into(),
            questions: BTreeMap::from([
                (
                    "pick".to_string(),
                    Question::Choice {
                        instructions: serde_json::json!("选一条"),
                        criteria: BTreeMap::from([
                            ("m1".to_string(), serde_json::Value::Null),
                            ("m2".to_string(), serde_json::json!({ "note": "摘要" })),
                        ]),
                    },
                ),
                (
                    "ask".to_string(),
                    Question::Noul {
                        instructions: serde_json::json!("要不要先问一句？"),
                        criteria: None,
                    },
                ),
            ]),
        };
        let sent = serde_json::to_value(&request).unwrap();
        assert!(sent["questions"]["ask"].get("criteria").is_none());
        assert!(sent["questions"]["pick"]["criteria"]["m1"].is_null());
    }
}
