//! 常用设置：一张写死的白名单，一项一个键，从 Web 改（§3「从 Web 改常用设置与环境变量」）。
//!
//! 与 [`super::edit`] 同一个做法：文本进、文本出，注释与键序原样保留，校验与落盘归调用方。
//! 白名单之外的键仍只在文件里改——开放一个键就要说得清它的种类、范围和"不写"是什么意思。

use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::protocol::http::{ConfigSetting, ConfigSettingsResponse, SettingKind};
use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table, value};

use super::edit::EditError;

/// 选项从哪儿来。
#[derive(Clone, Copy)]
enum Options {
    None,
    Fixed(&'static [&'static str]),
    /// 文件里的 `[agents.*]`。
    Agents,
    /// 文件里 completion 类的 `[model.*]`。
    CompletionModels,
    /// 文件里 embedding 类的 `[model.*]`。
    EmbeddingModels,
}

struct Spec {
    key: &'static str,
    kind: SettingKind,
    options: Options,
    min: Option<i64>,
    max: Option<i64>,
    /// 能不能删掉回默认。`default_agent` 没有隐含默认（§四），不能空着。
    clearable: bool,
}

const fn spec(key: &'static str, kind: SettingKind, options: Options) -> Spec {
    Spec {
        key,
        kind,
        options,
        min: None,
        max: None,
        clearable: true,
    }
}

const fn int(key: &'static str, min: i64, max: Option<i64>) -> Spec {
    Spec {
        key,
        kind: SettingKind::Integer,
        options: Options::None,
        min: Some(min),
        max,
        clearable: true,
    }
}

const SETTINGS: &[Spec] = &[
    Spec {
        clearable: false,
        ..spec("default_agent", SettingKind::Choice, Options::Agents)
    },
    spec("memory.enabled", SettingKind::Bool, Options::None),
    spec(
        "memory.model",
        SettingKind::Choice,
        Options::CompletionModels,
    ),
    // 换它要重启 Gateway 才生效（向量后端只在启动后探测装上，§9.5）；Web 写完会说。
    spec(
        "memory.embedding",
        SettingKind::Choice,
        Options::EmbeddingModels,
    ),
    spec(
        "memory.retrieval.mode",
        SettingKind::Choice,
        Options::Fixed(&["hybrid", "keyword", "vector"]),
    ),
    spec("compaction.enabled", SettingKind::Bool, Options::None),
    // 上限是工具结果投影的硬顶（§8.6）：配置只能往小调。
    int("execution.model_result_bytes", 1, Some(30 * 1024)),
    int("execution.call_timeout_secs", 1, None),
    int("execution.confirm_timeout_mins", 1, None),
];

fn find(key: &str) -> Result<&'static Spec, EditError> {
    SETTINGS
        .iter()
        .find(|s| s.key == key)
        .ok_or_else(|| EditError::NotFound(format!("常用设置里没有 `{key}`")))
}

fn parse(text: &str) -> Result<DocumentMut, EditError> {
    text.parse::<DocumentMut>()
        .map_err(|e| EditError::Unreadable(e.to_string()))
}

/// 沿点路径找到那一项；中间哪一节不在就是没写。
fn lookup<'a>(doc: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    let mut parts = key.split('.');
    let mut item = doc.as_item().get(parts.next()?)?;
    for part in parts {
        item = item.get(part)?;
    }
    Some(item)
}

fn to_json(item: &Item) -> Option<Value> {
    let v = item.as_value()?;
    if let Some(b) = v.as_bool() {
        return Some(Value::Bool(b));
    }
    if let Some(n) = v.as_integer() {
        return Some(Value::from(n));
    }
    v.as_str().map(|s| Value::String(s.to_string()))
}

fn table_keys<'a>(
    doc: &'a DocumentMut,
    section: &str,
) -> impl Iterator<Item = (&'a str, &'a Table)> {
    doc.get(section)
        .and_then(Item::as_table)
        .into_iter()
        .flat_map(|t| t.iter())
        .filter_map(|(k, item)| item.as_table().map(|t| (k, t)))
}

fn options_of(doc: &DocumentMut, options: Options) -> Vec<String> {
    match options {
        Options::None => Vec::new(),
        Options::Fixed(list) => list.iter().map(|s| s.to_string()).collect(),
        Options::Agents => table_keys(doc, "agents")
            .map(|(k, _)| k.to_string())
            .collect(),
        Options::CompletionModels => models_of_type(doc, "completion"),
        Options::EmbeddingModels => models_of_type(doc, "embedding"),
    }
}

fn models_of_type(doc: &DocumentMut, kind: &str) -> Vec<String> {
    table_keys(doc, "model")
        .filter(|(_, t)| t.get("type").and_then(Item::as_str) == Some(kind))
        .map(|(k, _)| k.to_string())
        .collect()
}

/// 生效值从**当前快照**读：文件改了还没装上时，两者会不一样，这正是要让人看见的。
fn effective(key: &str, snapshot: &ConfigSnapshot, doc: &DocumentMut) -> Option<Value> {
    Some(match key {
        "default_agent" => Value::String(snapshot.agent.default_agent.clone()),
        "memory.enabled" => Value::Bool(snapshot.memory.enabled),
        // 快照里放的是展开后的模型，不是 alias；没写时继承默认模型（file.rs 同一条规则）。
        "memory.model" => lookup(doc, "memory.model")
            .or_else(|| lookup(doc, "models.default"))
            .and_then(to_json)?,
        // 快照里也是展开后的模型；后端换没换上要看重启，这里只报文件里指的是谁。
        "memory.embedding" => lookup(doc, "memory.embedding").and_then(to_json)?,
        "memory.retrieval.mode" => serde_json::to_value(snapshot.memory.retrieval.mode).ok()?,
        "compaction.enabled" => Value::Bool(snapshot.compaction.enabled),
        "execution.model_result_bytes" => Value::from(snapshot.execution.model_result_bytes),
        "execution.call_timeout_secs" => Value::from(snapshot.execution.call_timeout_secs),
        "execution.confirm_timeout_mins" => Value::from(snapshot.execution.confirm_timeout_mins),
        _ => return None,
    })
}

/// 白名单里每一项：文件里写的值、当前生效的值、选项或范围。
pub fn settings_view(
    config_text: &str,
    snapshot: &ConfigSnapshot,
    version: String,
) -> Result<ConfigSettingsResponse, EditError> {
    let doc = parse(config_text)?;
    let settings = SETTINGS
        .iter()
        .map(|s| ConfigSetting {
            key: s.key.to_string(),
            kind: s.kind,
            value: lookup(&doc, s.key).and_then(to_json),
            effective: effective(s.key, snapshot, &doc),
            options: options_of(&doc, s.options),
            min: s.min,
            max: s.max,
        })
        .collect();
    Ok(ConfigSettingsResponse { version, settings })
}

/// 改一项。`None` 删掉这一行回到默认；节不在就建上。只动这一行。
pub fn set_setting(config_text: &str, key: &str, new: Option<&Value>) -> Result<String, EditError> {
    let s = find(key)?;
    let mut doc = parse(config_text)?;
    let item = match new {
        None if !s.clearable => {
            return Err(EditError::Invalid(format!("`{key}` 不能空着")));
        }
        None => None,
        Some(v) => Some(checked(&doc, s, v)?),
    };
    let mut parts: Vec<&str> = key.split('.').collect();
    let leaf = parts.pop().expect("白名单里的键不为空");
    let mut table = doc.as_table_mut();
    for part in parts {
        if !table.contains_key(part) {
            if item.is_none() {
                return Ok(doc.to_string());
            }
            table.insert(part, Item::Table(Table::new()));
        }
        table = table[part].as_table_mut().ok_or_else(|| {
            EditError::Invalid(format!("`{part}` 在文件里不是一节，先在文件里改"))
        })?;
    }
    match item {
        Some(item) => {
            table.insert(leaf, item);
        }
        None => {
            table.remove(leaf);
        }
    }
    Ok(doc.to_string())
}

fn checked(doc: &DocumentMut, s: &Spec, v: &Value) -> Result<Item, EditError> {
    let wrong = |what: &str| EditError::Invalid(format!("`{}` 要{what}", s.key));
    match s.kind {
        SettingKind::Bool => v.as_bool().map(value).ok_or_else(|| wrong("开或关")),
        SettingKind::Integer => {
            let n = v.as_i64().ok_or_else(|| wrong("一个整数"))?;
            let low = s.min.unwrap_or(i64::MIN);
            let high = s.max.unwrap_or(i64::MAX);
            if n < low || n > high {
                return Err(match s.max {
                    Some(max) => wrong(&format!("在 {low} 到 {max} 之间")),
                    None => wrong(&format!("不小于 {low}")),
                });
            }
            Ok(value(n))
        }
        SettingKind::Choice => {
            let text = v.as_str().ok_or_else(|| wrong("一个选项"))?;
            let options = options_of(doc, s.options);
            if !options.iter().any(|o| o == text) {
                return Err(wrong(&format!("是这几个之一：{}", options.join("、"))));
            }
            Ok(value(text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testing;

    const CONFIG: &str = r#"default_agent = "assistant"

[agents.assistant]
instructions = "hi"

[agents.coder]
instructions = "code"

[model.sol]
type = "completion"
model_provider = "p"
model = "sol"

[model.embedding]
type = "embedding"
model = "e"

[models]
default = "sol"

# 记忆
[memory]
enabled = true # 开着
"#;

    fn view() -> ConfigSettingsResponse {
        // 生效值来自任意一份装好的快照；这几条测的是文件那一侧
        let snapshot = testing::Fixture::valid().holder().current();
        settings_view(CONFIG, &snapshot, "v".into()).unwrap()
    }

    fn get<'a>(v: &'a ConfigSettingsResponse, key: &str) -> &'a ConfigSetting {
        v.settings.iter().find(|s| s.key == key).unwrap()
    }

    #[test]
    fn the_view_says_what_the_file_wrote_and_what_the_choices_are() {
        let v = view();
        let agent = get(&v, "default_agent");
        assert_eq!(agent.value, Some(Value::from("assistant")));
        assert_eq!(agent.options, ["assistant", "coder"]);
        let model = get(&v, "memory.model");
        assert_eq!(model.value, None, "没写");
        assert_eq!(model.effective, Some(Value::from("sol")), "跟默认模型");
        assert_eq!(model.options, ["sol"], "embedding 不是选项");
        let embedding = get(&v, "memory.embedding");
        assert_eq!(embedding.options, ["embedding"], "只列向量模型");
        assert_eq!(embedding.value, None);
        assert_eq!(get(&v, "memory.enabled").value, Some(Value::Bool(true)));
        assert_eq!(get(&v, "execution.model_result_bytes").max, Some(30 * 1024));
    }

    #[test]
    fn a_setting_is_written_in_place_and_comments_survive() {
        let out = set_setting(CONFIG, "memory.enabled", Some(&Value::Bool(false))).unwrap();
        assert!(out.contains("# 记忆\n[memory]\nenabled = false"), "{out}");
        let out =
            set_setting(&out, "memory.retrieval.mode", Some(&Value::from("keyword"))).unwrap();
        assert!(
            out.contains("[memory.retrieval]\nmode = \"keyword\""),
            "{out}"
        );
        let out =
            set_setting(&out, "execution.call_timeout_secs", Some(&Value::from(300))).unwrap();
        assert!(
            out.contains("[execution]\ncall_timeout_secs = 300"),
            "{out}"
        );
        let out = set_setting(&out, "default_agent", Some(&Value::from("coder"))).unwrap();
        assert!(out.starts_with("default_agent = \"coder\"\n"), "{out}");
    }

    #[test]
    fn clearing_a_setting_removes_its_line() {
        let out = set_setting(CONFIG, "memory.enabled", None).unwrap();
        assert!(!out.contains("enabled"), "{out}");
        assert!(out.contains("[memory]"));
        assert_eq!(
            set_setting(CONFIG, "compaction.enabled", None).unwrap(),
            CONFIG,
            "没写的清了也不建节"
        );
        assert!(matches!(
            set_setting(CONFIG, "default_agent", None),
            Err(EditError::Invalid(_))
        ));
    }

    #[test]
    fn values_outside_the_kind_range_or_options_are_refused() {
        let bad = |key: &str, v: Value| {
            matches!(
                set_setting(CONFIG, key, Some(&v)),
                Err(EditError::Invalid(_))
            )
        };
        assert!(bad("memory.enabled", Value::from("yes")));
        assert!(bad("execution.model_result_bytes", Value::from(40_000)));
        assert!(bad("execution.call_timeout_secs", Value::from(0)));
        assert!(bad("memory.model", Value::from("embedding")));
        assert!(bad("default_agent", Value::from("nobody")));
        assert!(matches!(
            set_setting(CONFIG, "gateway.listen", Some(&Value::from("0.0.0.0:1"))),
            Err(EditError::NotFound(_))
        ));
    }
}
