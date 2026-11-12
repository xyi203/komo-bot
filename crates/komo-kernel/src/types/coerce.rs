//! 工具参数按 JSON Schema 做宽松转换（`docs/tasks.md` §4.10 P3），在 `Tool::prepare` 之前。
//!
//! 轻量模型常把数字写成 `"3"`、布尔写成 `"true"`，或给可选字段填 `null`。这里只做无歧义的
//! 几种，其余原样交给 `prepare` 报错：
//!
//! - 字符串 → 数字 / 整数 / 布尔（schema 的 `type` 不含 `string` 时；整数只收整数写法）；
//! - 可选字段（不在 `required` 里）的 `null` 去掉（schema 本身接受 `null` 的保留）。
//!
//! 递归进 `properties` 与 `items`；`anyOf` / `oneOf` / `$ref` 之类不展开，对应值原样不动。

use serde_json::{Map, Value};

/// 按 `schema` 转换 `arguments`；不是对象 schema 或没有可转换的东西就原样返回。
pub fn coerce_arguments(arguments: Value, schema: &Value) -> Value {
    let mut arguments = arguments;
    coerce(&mut arguments, schema);
    arguments
}

fn types_of(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(ty)) => vec![ty.as_str()],
        Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn accepts_null(schema: &Value) -> bool {
    if types_of(schema).contains(&"null") {
        return true;
    }
    ["anyOf", "oneOf"].iter().any(|key| {
        schema
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|members| members.iter().any(accepts_null))
    })
}

fn coerce(value: &mut Value, schema: &Value) {
    match value {
        Value::String(text) => {
            let types = types_of(schema);
            if types.contains(&"string") {
                return;
            }
            if let Some(converted) = convert_string(text, &types) {
                *value = converted;
            }
        }
        Value::Object(map) => coerce_object(map, schema),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items").filter(|items| items.is_object()) {
                for item in items {
                    coerce(item, item_schema);
                }
            }
        }
        _ => {}
    }
}

fn coerce_object(map: &mut Map<String, Value>, schema: &Value) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let nulls: Vec<String> = map
        .iter()
        .filter(|(key, value)| {
            value.is_null()
                && !required.contains(&key.as_str())
                && properties.get(*key).is_some_and(|prop| !accepts_null(prop))
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in nulls {
        map.remove(&key);
    }
    for (key, value) in map.iter_mut() {
        if let Some(property) = properties.get(key) {
            coerce(value, property);
        }
    }
}

/// 字符串按 `types` 能无歧义转成什么就转成什么；用 JSON 自己的数字语法判定（`012`、`+5`、
/// 带空白的都不算数字）。
fn convert_string(text: &str, types: &[&str]) -> Option<Value> {
    let wants = |ty: &str| types.contains(&ty);
    if wants("boolean") && !wants("number") && !wants("integer") {
        return match text {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        };
    }
    if wants("integer") || wants("number") {
        // serde_json 容忍首尾空白，这里不容忍。
        if text.trim() != text {
            return None;
        }
        let Ok(Value::Number(number)) = serde_json::from_str::<Value>(text) else {
            return None;
        };
        if wants("number") || number.is_i64() || number.is_u64() {
            return Some(Value::Number(number));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"},
                "ratio": {"type": "number"},
                "flag": {"type": "boolean"},
                "maybe": {"type": ["string", "null"]},
                "nested": {
                    "type": "object",
                    "properties": {"count": {"type": "integer"}},
                },
                "list": {"type": "array", "items": {"type": "number"}},
            },
            "required": ["path"],
        })
    }

    #[test]
    fn numeric_and_boolean_strings_become_their_types() {
        let out = coerce_arguments(
            json!({"path": "a", "limit": "20", "ratio": "1.5", "flag": "true"}),
            &schema(),
        );
        assert_eq!(
            out,
            json!({"path": "a", "limit": 20, "ratio": 1.5, "flag": true})
        );
    }

    #[test]
    fn conversion_recurses_into_objects_and_arrays() {
        let out = coerce_arguments(
            json!({"path": "a", "nested": {"count": "3"}, "list": ["1", "2.5"]}),
            &schema(),
        );
        assert_eq!(
            out,
            json!({"path": "a", "nested": {"count": 3}, "list": [1, 2.5]})
        );
    }

    #[test]
    fn null_is_dropped_only_from_optional_fields_that_do_not_accept_it() {
        let out = coerce_arguments(
            json!({"path": null, "limit": null, "maybe": null}),
            &schema(),
        );
        // `path` 必填：留着 null 让 `prepare` 报错；`maybe` 本身接受 null。
        assert_eq!(out, json!({"path": null, "maybe": null}));
    }

    #[test]
    fn ambiguous_values_are_left_for_prepare_to_reject() {
        let args = json!({
            "path": "a",
            "limit": "2.5",
            "ratio": " 3",
            "flag": "yes",
            "nested": {"count": "012"},
        });
        assert_eq!(coerce_arguments(args.clone(), &schema()), args);
    }

    #[test]
    fn strings_stay_strings_when_the_schema_asks_for_strings() {
        let schema = json!({"properties": {"id": {"type": ["string", "integer"]}}});
        assert_eq!(
            coerce_arguments(json!({"id": "7"}), &schema),
            json!({"id": "7"})
        );
    }

    #[test]
    fn a_schema_without_properties_changes_nothing() {
        let args = json!({"a": "1", "b": null});
        assert_eq!(coerce_arguments(args.clone(), &json!({})), args);
    }
}
