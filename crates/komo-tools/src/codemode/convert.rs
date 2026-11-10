//! serde_json::Value <-> MontyObject。
//!
//! 有损点（monty -> json）：tuple/set/frozenset/namedtuple 都变数组；bytes 变
//! `{"$bytes": base64}`；超出 u64 的大整数变十进制字符串；NaN/inf 变字符串；
//! 非字符串 dict 键用 repr；Path/datetime/exception 等变 repr 字符串。
//! json -> monty 无损（整数/浮点的区别由 serde_json 解析时保留）。
//!
//! `text()` 的非字符串值另走 [`to_text`]：照 Python 的 `json.dumps(indent=2, ensure_ascii=False,
//! default=str)` 逐字节输出，且保留 dict 的插入序（`serde_json::Value` 的对象按键排序）。

use base64::Engine;
use monty_types::{
    MontyObject, ObjectRef,
    unstable::{self, MontyNode},
};
use num_bigint::BigInt;
use serde_json::{Map, Number, Value};

pub fn json_to_monty(value: &Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::none(),
        Value::Bool(b) => MontyObject::bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::int(i)
            } else if let Some(u) = n.as_u64() {
                MontyObject::bigint(BigInt::from(u))
            } else {
                MontyObject::float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => MontyObject::string(s.clone()),
        Value::Array(items) => MontyObject::list(items.iter().map(json_to_monty)),
        Value::Object(map) => MontyObject::dict(
            map.iter()
                .map(|(k, v)| (MontyObject::string(k.clone()), json_to_monty(v))),
        ),
    }
}

pub fn monty_to_json(value: ObjectRef<'_>) -> Value {
    match unstable::node(value) {
        MontyNode::None => Value::Null,
        MontyNode::Bool(b) => Value::Bool(*b),
        MontyNode::Int(i) => Value::Number((*i).into()),
        MontyNode::BigInt(b) => match u64::try_from(b) {
            Ok(u) => Value::Number(u.into()),
            Err(_) => Value::String(b.to_string()),
        },
        MontyNode::Float(f) => match Number::from_f64(*f) {
            Some(n) => Value::Number(n),
            None => Value::String(f.to_string()),
        },
        MontyNode::String(s) => Value::String(s.clone()),
        MontyNode::Bytes(b) => {
            let mut m = Map::new();
            m.insert(
                "$bytes".into(),
                Value::String(base64::engine::general_purpose::STANDARD.encode(b)),
            );
            Value::Object(m)
        }
        MontyNode::List(_)
        | MontyNode::Tuple(_)
        | MontyNode::Set(_)
        | MontyNode::FrozenSet(_)
        | MontyNode::NamedTuple { .. } => Value::Array(
            value
                .items()
                .unwrap_or_default()
                .into_iter()
                .map(monty_to_json)
                .collect(),
        ),
        MontyNode::Dict(_) => {
            let mut m = Map::new();
            for (k, v) in value.pairs().unwrap_or_default() {
                let key = match k.as_str() {
                    Some(s) => s.to_owned(),
                    None => k.py_repr(),
                };
                m.insert(key, monty_to_json(v));
            }
            Value::Object(m)
        }
        MontyNode::Path(p) => Value::String(p.clone()),
        _ => Value::String(value.py_repr()),
    }
}

/// 嵌套再深也不递归下去：超过的部分当字符串 repr 输出。
const TEXT_DEPTH: usize = 512;

/// `text(value)` 的非字符串输出：同 Python `json.dumps(value, indent=2, ensure_ascii=False,
/// default=str)`。dict 保留插入序；set、bytes、日期等 JSON 没有的类型按 `str()` 输出成字符串。
pub fn to_text(value: ObjectRef<'_>) -> String {
    let mut out = String::new();
    write_text(&mut out, value, 0);
    out
}

fn write_text(out: &mut String, value: ObjectRef<'_>, depth: usize) {
    if depth > TEXT_DEPTH {
        return write_string(out, &value.py_repr());
    }
    match unstable::node(value) {
        MontyNode::None => out.push_str("null"),
        MontyNode::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        MontyNode::Int(i) => out.push_str(&i.to_string()),
        MontyNode::BigInt(b) => out.push_str(&b.to_string()),
        MontyNode::Float(f) => match Number::from_f64(*f) {
            Some(n) => out.push_str(&n.to_string()),
            None if f.is_nan() => out.push_str("NaN"),
            None if *f > 0.0 => out.push_str("Infinity"),
            None => out.push_str("-Infinity"),
        },
        MontyNode::String(s) => write_string(out, s),
        MontyNode::List(_) | MontyNode::Tuple(_) | MontyNode::NamedTuple { .. } => {
            let items = value.items().unwrap_or_default();
            if items.is_empty() {
                return out.push_str("[]");
            }
            out.push('[');
            for (index, item) in items.into_iter().enumerate() {
                out.push_str(if index == 0 { "\n" } else { ",\n" });
                indent(out, depth + 1);
                write_text(out, item, depth + 1);
            }
            out.push('\n');
            indent(out, depth);
            out.push(']');
        }
        MontyNode::Dict(_) => {
            let pairs = value.pairs().unwrap_or_default();
            if pairs.is_empty() {
                return out.push_str("{}");
            }
            out.push('{');
            for (index, (key, item)) in pairs.into_iter().enumerate() {
                out.push_str(if index == 0 { "\n" } else { ",\n" });
                indent(out, depth + 1);
                match unstable::node(key) {
                    MontyNode::String(s) => write_string(out, s),
                    MontyNode::Bool(b) => write_string(out, if *b { "true" } else { "false" }),
                    MontyNode::None => write_string(out, "null"),
                    _ => write_string(out, &key.py_repr()),
                }
                out.push_str(": ");
                write_text(out, item, depth + 1);
            }
            out.push('\n');
            indent(out, depth);
            out.push('}');
        }
        _ => write_string(out, &value.py_repr()),
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push_str(&serde_json::to_string(text).unwrap_or_default());
}
