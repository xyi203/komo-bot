//! 委派（delegate）：一次子任务的说明、结果契约，与**父子共用的结果校验器**。
//!
//! 一个子代理 = 同 Session 里的一条子 Run（§4、§8.4 的 `dependency`）。这条 Run 的受理
//! 事件带着这里的 [`DelegateSpec`]：谁派的、父侧那次调用是哪一次、结果要长什么样、它有几
//! 轮预算。**契约必须落在事件里**——重启之后它是子代理唯一的"结果要长什么样"的依据，
//! 而它同时就是父侧复验时用的那一份（`Event.run` 已经有了，子代理的轮次因此认得出来）。
//!
//! 校验器是**父子共用的一份代码**：子代理提交结果时用它，父侧收到结果时用同一份再验一遍。
//! 两份实现会漂移，漂移的症状是"子代理说成功、父侧说结果不合规"——那正是 §8.6 那句
//! "先判断是否发生，再决定是否重试"的另一面：判断只能有一个出处。
//!
//! **它不是通用 JSON Schema 校验器。** 下面那几条关键字之外的一律**不检查**，并且
//! **明说**是哪些（[`Validation::ignored`]）。看见 `oneOf` 就当"已校验"是在撒谎，而撒谎
//! 的校验器比没有校验器更坏——§7.1 那句"任何更强的归一都会让人以为这道网能认出实际要跑
//! 的东西"在这里同样成立。

use serde::{Deserialize, Serialize};

use super::ids::{RunId, ToolCallId};

/// 子代理的默认轮次预算。
pub const DEFAULT_DELEGATE_ROUNDS: u32 = 8;

/// 校验失败之后允许的修复轮次：把不合规的地方原文喂回去让它改（§8.6 的核对同理，
/// 判断之后要给出**能行动**的下一步）。
pub const DELEGATE_REPAIR_ROUNDS: u32 = 2;

/// 一次委派。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateSpec {
    /// 派它的那条 Run。
    pub parent: RunId,
    /// 父侧承载这次委派的调用。子代理的终态要回到**它**上面：那条 `tool.result` 是父
    /// Run 续跑时拿到的工具结果。
    pub call: ToolCallId,
    /// 交给子代理的自包含任务。它就是子 Run 的输入正文（§8.5），也是审批卡上给人看的
    /// 那一句——两者是同一个值，写在 `prepare` 那一处。
    pub task: String,
    /// 子代理能跑几轮模型。默认 [`DEFAULT_DELEGATE_ROUNDS`]。
    #[serde(default = "default_rounds")]
    pub rounds: u32,
    /// 结果契约。不给 = 自由文本（旧 `delegate` 的形状，父侧只能自己读）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<DelegateContract>,
}

fn default_rounds() -> u32 {
    DEFAULT_DELEGATE_ROUNDS
}

impl DelegateSpec {
    pub fn new(parent: RunId, call: ToolCallId, task: impl Into<String>) -> Self {
        Self {
            parent,
            call,
            task: task.into(),
            rounds: DEFAULT_DELEGATE_ROUNDS,
            contract: None,
        }
    }

    pub fn with_contract(mut self, schema: serde_json::Value, mode: SchemaMode) -> Self {
        self.contract = Some(DelegateContract { schema, mode });
        self
    }

    pub fn with_rounds(mut self, rounds: u32) -> Self {
        self.rounds = rounds;
        self
    }
}

/// 结果契约。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateContract {
    /// 结果的 schema（本模块认得的那个子集）。
    pub schema: serde_json::Value,
    #[serde(default)]
    pub mode: SchemaMode,
}

/// 校验不过时怎么办。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaMode {
    /// 修不好也把结果交给父侧，但**标记出来**（`schemaOverridden`）。默认。
    #[default]
    Permissive,
    /// 修不好就算这次委派失败。
    Strict,
}

// ---------------------------------------------------------------- 校验

/// 一条不合规。`at` 是 JSON-Pointer 风格的路径（`/files/0/path`），根是空串。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub at: String,
    /// 这里要的是什么——写成模型能照着改的话，不是"验证失败"。
    pub expect: String,
}

/// 一次校验的结论。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validation {
    /// 全部不合规，**一次给全**：只报第一条会让模型改一处再撞一次，一轮一个问题。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<Violation>,
    /// 认得但不检查的关键字。非空 = 这次校验没有它看上去那么全，必须说出来。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<String>,
}

impl Validation {
    pub fn is_valid(&self) -> bool {
        self.violations.is_empty()
    }

    /// 给模型看的一段话。合规时为空串。
    pub fn describe(&self) -> String {
        if self.violations.is_empty() && self.ignored.is_empty() {
            return String::new();
        }
        let mut lines: Vec<String> = self
            .violations
            .iter()
            .map(|v| {
                let at = if v.at.is_empty() { "/" } else { v.at.as_str() };
                format!("{at}: {}", v.expect)
            })
            .collect();
        if !self.ignored.is_empty() {
            lines.push(format!(
                "（这些关键字没有检查，别把它们当成已经过关：{}）",
                self.ignored.join("、")
            ));
        }
        lines.join("; ")
    }
}

/// 认得的类型名。
const TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// 只用来当注解、不必校验也不需要报的关键字。
const ANNOTATIONS: [&str; 5] = ["$schema", "title", "description", "default", "examples"];

/// 按契约校验一份结果。
///
/// 认得的：`type`（名字或名字数组）、`required`、`properties`、`items`、`enum`、
/// `additionalProperties: false`，以及上面那几个注解。**其余一律不检查并记录下来。**
pub fn validate(contract: &DelegateContract, data: &serde_json::Value) -> Validation {
    let mut validation = Validation::default();
    check(&contract.schema, data, "", &mut validation);
    validation
}

fn check(schema: &serde_json::Value, data: &serde_json::Value, at: &str, out: &mut Validation) {
    let Some(schema) = schema.as_object() else {
        // `true` / `false` / 别的形状：`false` 是 JSON Schema 里的"永远不合规"，明说它。
        if is_false_schema(schema) {
            out.violations.push(Violation {
                at: at.to_string(),
                expect: "schema 写了 false，这里不接受任何值".into(),
            });
        }
        return;
    };

    for keyword in schema.keys() {
        if !SUPPORTED.contains(&keyword.as_str()) && !ANNOTATIONS.contains(&keyword.as_str()) {
            push_once(&mut out.ignored, keyword);
        }
    }

    if let Some(wanted) = schema.get("type") {
        let names: Vec<&str> = match wanted {
            serde_json::Value::String(name) => vec![name.as_str()],
            serde_json::Value::Array(items) => {
                items.iter().filter_map(|item| item.as_str()).collect()
            }
            _ => Vec::new(),
        };
        if names.is_empty() {
            push_once(&mut out.ignored, "type（认不出的写法）");
        } else {
            for name in &names {
                if !TYPES.contains(name) {
                    push_once(
                        &mut out.ignored,
                        &format!("type: {name}（不认识这个类型名）"),
                    );
                }
            }
            if !names.iter().any(|name| matches_type(name, data)) {
                out.violations.push(Violation {
                    at: at.to_string(),
                    expect: format!("要是 {}，收到的是 {}", names.join(" 或 "), kind_of(data)),
                });
                // 类型都不对，再看 properties / items 只会产生一堆噪音。
                return;
            }
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(|value| value.as_array())
        && !allowed.iter().any(|one| one == data)
    {
        out.violations.push(Violation {
            at: at.to_string(),
            expect: format!("只能是 {} 里的一个", compact(allowed)),
        });
    }

    match data {
        serde_json::Value::Object(map) => {
            if let Some(required) = schema.get("required").and_then(|value| value.as_array()) {
                for name in required.iter().filter_map(|name| name.as_str()) {
                    if !map.contains_key(name) {
                        out.violations.push(Violation {
                            at: join(at, name),
                            expect: "必填，结果里没有这个字段".into(),
                        });
                    }
                }
            }
            let properties = schema.get("properties").and_then(|value| value.as_object());
            for (name, value) in map {
                match properties.and_then(|properties| properties.get(name)) {
                    Some(child) => check(child, value, &join(at, name), out),
                    None => {
                        let closed = schema
                            .get("additionalProperties")
                            .is_some_and(is_false_schema);
                        if closed {
                            out.violations.push(Violation {
                                at: join(at, name),
                                expect: "schema 里没有这个字段（additionalProperties: false）"
                                    .into(),
                            });
                        }
                    }
                }
            }
            if properties.is_none() && schema.get("additionalProperties").is_some() {
                // 没有 properties 却想约束未知键：那套语义在这里没有实现。
                push_once(
                    &mut out.ignored,
                    "additionalProperties（没有 properties 时）",
                );
            }
        }
        serde_json::Value::Array(items) => {
            if let Some(child) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check(child, item, &format!("{at}/{index}"), out);
                }
            }
        }
        _ => {}
    }
}

/// JSON Schema 里的 `false`：这一处不接受任何值。
fn is_false_schema(value: &serde_json::Value) -> bool {
    matches!(value, serde_json::Value::Bool(false))
}

fn matches_type(name: &str, data: &serde_json::Value) -> bool {
    match name {
        "object" => data.is_object(),
        "array" => data.is_array(),
        "string" => data.is_string(),
        "number" => data.is_number(),
        // `1.0` 在 JSON 里是数字，但整数语义下得是整的——`is_i64`/`is_u64` 之外还有
        // 浮点整数值这一种，用 `as_f64().fract() == 0` 认。
        "integer" => data.as_f64().is_some_and(|number| number.fract() == 0.0),
        "boolean" => data.is_boolean(),
        "null" => data.is_null(),
        _ => false,
    }
}

fn kind_of(data: &serde_json::Value) -> &'static str {
    match data {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(number) => {
            if number.as_f64().is_some_and(|value| value.fract() == 0.0) {
                "number（整数值）"
            } else {
                "number"
            }
        }
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn join(at: &str, name: &str) -> String {
    format!("{at}/{name}")
}

fn compact(values: &[serde_json::Value]) -> String {
    values
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("、")
}

fn push_once(into: &mut Vec<String>, keyword: &str) {
    if !into.iter().any(|existing| existing == keyword) {
        into.push(keyword.to_string());
    }
}

const SUPPORTED: [&str; 6] = [
    "type",
    "required",
    "properties",
    "items",
    "enum",
    "additionalProperties",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contract(schema: serde_json::Value) -> DelegateContract {
        DelegateContract {
            schema,
            mode: SchemaMode::Permissive,
        }
    }

    fn schema() -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["verdict", "files"],
            "additionalProperties": false,
            "properties": {
                "verdict": { "type": "string", "enum": ["pass", "fail"] },
                "files": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["path"],
                        "properties": { "path": { "type": "string" }, "lines": { "type": "integer" } }
                    }
                }
            }
        })
    }

    #[test]
    fn a_well_formed_result_passes() {
        let ok = json!({
            "verdict": "pass",
            "files": [{ "path": "src/a.rs", "lines": 3 }]
        });
        let validation = validate(&contract(schema()), &ok);
        assert!(validation.is_valid(), "{validation:?}");
        assert!(validation.ignored.is_empty());
        assert_eq!(validation.describe(), "");
    }

    /// **一次给全**：两条必填缺失 + 一处类型不对，必须在同一份报文里，而不是改一处再撞
    /// 一次（那就是一轮一个问题）。
    #[test]
    fn every_violation_is_reported_at_once_with_its_path() {
        let bad = json!({ "files": [{ "lines": "三" }] });
        let validation = validate(&contract(schema()), &bad);
        assert!(!validation.is_valid());
        let text = validation.describe();
        assert!(text.contains("/verdict: 必填"), "{text}");
        assert!(text.contains("/files/0/path: 必填"), "{text}");
        assert!(text.contains("/files/0/lines: 要是 integer"), "{text}");
    }

    #[test]
    fn a_closed_schema_names_the_field_it_does_not_know() {
        let extra = json!({ "verdict": "pass", "files": [], "confidence": 0.9 });
        let validation = validate(&contract(schema()), &extra);
        assert!(!validation.is_valid());
        assert!(
            validation.describe().contains("/confidence"),
            "{validation:?}"
        );
    }

    #[test]
    fn an_enum_only_accepts_its_values() {
        let wrong = json!({ "verdict": "maybe", "files": [] });
        let validation = validate(&contract(schema()), &wrong);
        assert!(validation.describe().contains("\"pass\""), "{validation:?}");
    }

    /// 认不出的关键字**不检查**，而且必须说出来——把它当成"已校验"是撒谎，撒谎的校验器
    /// 比没有校验器更坏。
    #[test]
    fn unsupported_keywords_are_reported_not_silently_passed() {
        let only = json!({ "type": "object", "oneOf": [{ "required": ["a"] }] });
        let validation = validate(&contract(only), &json!({ "b": 1 }));
        assert!(validation.is_valid(), "没实现的东西不该假装判它不合规");
        assert_eq!(validation.ignored, vec!["oneOf".to_string()]);
        assert!(validation.describe().contains("oneOf"), "{validation:?}");
    }

    #[test]
    fn the_default_mode_is_permissive() {
        assert_eq!(SchemaMode::default(), SchemaMode::Permissive);
        // 老事件里没有 mode 这个键时也是 permissive（默认值跟着走）。
        let decoded: DelegateContract =
            serde_json::from_value(json!({ "schema": { "type": "object" } })).expect("解得出");
        assert_eq!(decoded.mode, SchemaMode::Permissive);
    }

    /// 预算与契约缺席时的默认值：老事件、以及"不给 schema"都要解得开。
    #[test]
    fn a_spec_without_rounds_or_contract_still_decodes() {
        let decoded: DelegateSpec = serde_json::from_value(json!({
            "parent": "run-1",
            "call": "call-1",
            "task": "看一下这个 PR"
        }))
        .expect("解得出");
        assert_eq!(decoded.task, "看一下这个 PR");
        assert_eq!(decoded.rounds, DEFAULT_DELEGATE_ROUNDS);
        assert!(decoded.contract.is_none());
    }
}
