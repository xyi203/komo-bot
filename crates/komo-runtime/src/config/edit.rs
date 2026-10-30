//! 在 `config.toml` / `.env` 的**原文**上改模型配置（§3「从 Web 改模型配置」）。
//!
//! 这里只做文本到文本：读进 `toml_edit` 的文档、改几个键、交回新文本；注释、空行、键序
//! 原样保留。校验与落盘不在这里——调用方拿新文本走 [`super::check_candidate`]（与加载同一
//! 个函数），过了才写。`config.toml` 只碰三处：`[model_providers.<id>]`、completion 类的
//! `[model.<alias>]`、`[models] default`；`.env` 按行改，只认名字、值只进不出。常用设置的
//! 白名单在 [`super::settings`]。

use std::collections::BTreeMap;

use komo_kernel::protocol::http::{
    ConfigEnvResponse, ConfigModelEntry, ConfigModelsResponse, EnvVarEntry, ProviderEntry,
};
use komo_kernel::types::chat::ChannelPlatform;
use komo_kernel::types::digest::ContentHash;
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::Secrets;

/// 改不成的原因。HTTP 层按种类给状态码。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    /// 请求本身不对：名字不合规、值里有换行……
    #[error("{0}")]
    Invalid(String),
    /// 要改的东西不在。
    #[error("{0}")]
    NotFound(String),
    /// 还有别的地方引用它，或它不归这里改。
    #[error("{0}")]
    InUse(String),
    /// 文件本身读不成 TOML（有人在编辑器里改坏了）：先去文件里修。
    #[error("config.toml 现在不是合法的 TOML，先在文件里改好：{0}")]
    Unreadable(String),
}

/// 两份文件此刻内容的版本：写的时候带回来，对不上说明中间有人改过。
pub fn config_version(config_text: &str, env_text: &str) -> String {
    ContentHash::of_str(&format!("{config_text}\u{0}{env_text}"))
        .as_str()
        .to_string()
}

fn parse(text: &str) -> Result<DocumentMut, EditError> {
    text.parse::<DocumentMut>()
        .map_err(|e| EditError::Unreadable(e.to_string()))
}

fn str_of(table: &Table, key: &str) -> Option<String> {
    table.get(key).and_then(Item::as_str).map(str::to_string)
}

fn int_of(table: &Table, key: &str) -> Option<u64> {
    table
        .get(key)
        .and_then(Item::as_integer)
        .and_then(|n| u64::try_from(n).ok())
}

/// `env_key` 与旧拼法 `api_key_env` 都认（`ModelProviderSection` 同一个别名）。
fn env_key_of(table: &Table) -> Option<String> {
    str_of(table, "env_key").or_else(|| str_of(table, "api_key_env"))
}

fn sub_table<'a>(doc: &'a DocumentMut, section: &str, name: &str) -> Option<&'a Table> {
    doc.get(section)?.as_table()?.get(name)?.as_table()
}

/// 文件里写着的模型配置。`secrets` 是按**同一份** `.env` 读出来的，凭证只报在不在。
pub fn models_view(
    config_text: &str,
    secrets: &Secrets,
    version: String,
    config_path: String,
    env_path: String,
) -> Result<ConfigModelsResponse, EditError> {
    let doc = parse(config_text)?;
    let mut providers = Vec::new();
    if let Some(table) = doc.get("model_providers").and_then(Item::as_table) {
        for (id, item) in table.iter() {
            let Some(t) = item.as_table() else { continue };
            let env_key = env_key_of(t);
            let auth = str_of(t, "auth");
            providers.push(ProviderEntry {
                id: id.to_string(),
                base_url: str_of(t, "base_url"),
                api_backend: str_of(t, "api_backend"),
                key_set: auth.is_some() || env_key.as_deref().is_some_and(|k| secrets.has(k)),
                env_key,
                auth,
                timeout_secs: int_of(t, "timeout_secs"),
            });
        }
    }
    let mut models = Vec::new();
    if let Some(table) = doc.get("model").and_then(Item::as_table) {
        for (alias, item) in table.iter() {
            let Some(t) = item.as_table() else { continue };
            if str_of(t, "type").as_deref() != Some("completion") {
                continue;
            }
            let provider = str_of(t, "model_provider");
            models.push(ConfigModelEntry {
                alias: alias.to_string(),
                standalone: provider.is_none(),
                provider,
                model: str_of(t, "model").unwrap_or_default(),
                name: str_of(t, "name"),
                effort: str_of(t, "effort"),
                efforts: t.get("efforts").and_then(Item::as_array).map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                }),
                context_window: int_of(t, "context_window"),
            });
        }
    }
    let default = sub_table_value(&doc, "models", "default");
    Ok(ConfigModelsResponse {
        version,
        providers,
        models,
        default,
        config_path,
        env_path,
    })
}

fn sub_table_value(doc: &DocumentMut, section: &str, key: &str) -> Option<String> {
    doc.get(section)?
        .as_table()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

/// provider id 与模型 alias：表名里放得下、URL 里也放得下的那些字符。
fn check_name(what: &str, name: &str) -> Result<(), EditError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(EditError::Invalid(format!(
            "{what} `{name}` 只能用字母、数字、`-`、`_`、`.`，最长 64 个字符"
        )))
    }
}

fn check_env_name(name: &str) -> Result<(), EditError> {
    let mut chars = name.chars();
    let ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(EditError::Invalid(format!(
            "变量名 `{name}` 只能用大写字母、数字与 `_`，且不以数字开头"
        )))
    }
}

/// 一行文字值：去首尾空白，空串当没写；不许换行（一个键一行）。
fn line(what: &str, text: Option<&str>) -> Result<Option<String>, EditError> {
    let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    if text.contains(['\n', '\r']) {
        return Err(EditError::Invalid(format!("{what} 不能有换行")));
    }
    Ok(Some(text.to_string()))
}

/// 拿到（没有就建）`[section.name]`。父表是隐式的：文件里不多出一个空的 `[section]`。
fn table_mut<'a>(doc: &'a mut DocumentMut, section: &str, name: &str) -> &'a mut Table {
    if doc.get(section).and_then(Item::as_table).is_none() {
        let mut parent = Table::new();
        parent.set_implicit(true);
        doc.insert(section, Item::Table(parent));
    }
    let parent = doc[section].as_table_mut().expect("刚确认过是表");
    if parent.get(name).and_then(Item::as_table).is_none() {
        parent.insert(name, Item::Table(Table::new()));
    }
    parent[name].as_table_mut().expect("刚确认过是表")
}

fn set_or_remove(table: &mut Table, key: &str, text: Option<String>) {
    match text {
        Some(text) => {
            table[key] = value(text);
        }
        None => {
            table.remove(key);
        }
    }
}

fn set_or_remove_int(table: &mut Table, key: &str, number: Option<u64>) -> Result<(), EditError> {
    match number {
        Some(n) => {
            let n = i64::try_from(n).map_err(|_| EditError::Invalid(format!("{key} 太大了")))?;
            table[key] = value(n);
        }
        None => {
            table.remove(key);
        }
    }
    Ok(())
}

/// provider 没写 `env_key` 又要存 key 时用的变量名：`<ID 大写>_API_KEY`。
fn default_env_key(id: &str) -> String {
    let stem: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}_API_KEY")
}

/// 一个 provider 要写的东西（`ProviderUpsertRequest` 去掉版本）。
#[derive(Debug, Clone, Default)]
pub struct ProviderFields<'a> {
    pub base_url: Option<&'a str>,
    pub api_backend: Option<&'a str>,
    pub env_key: Option<&'a str>,
    pub auth: Option<&'a str>,
    pub timeout_secs: Option<u64>,
    pub api_key: Option<&'a str>,
}

/// 新建或整份改 `[model_providers.<id>]` 的这几个键；给了 `api_key` 就同时改 `.env`。
///
/// 返回新的 `config.toml` 文本，与（要改的话）新的 `.env` 文本。
pub fn upsert_provider(
    config_text: &str,
    env_text: &str,
    id: &str,
    fields: &ProviderFields<'_>,
) -> Result<(String, Option<String>), EditError> {
    check_name("provider", id)?;
    let base_url = line("base_url", fields.base_url)?;
    let api_backend = line("api_backend", fields.api_backend)?;
    let auth = line("auth", fields.auth)?;
    let mut env_key = line("env_key", fields.env_key)?;
    let api_key = fields
        .api_key
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);
    if let Some(key) = &env_key {
        check_env_name(key)?;
    }
    if auth.is_some() && (env_key.is_some() || api_key.is_some()) {
        return Err(EditError::Invalid(
            "`auth` 与 env_key / API key 只能二选一：auth 的凭证不走 .env".into(),
        ));
    }
    if api_key.is_some() && env_key.is_none() {
        env_key = Some(default_env_key(id));
    }

    let mut doc = parse(config_text)?;
    let table = table_mut(&mut doc, "model_providers", id);
    set_or_remove(table, "base_url", base_url);
    set_or_remove(table, "api_backend", api_backend);
    set_or_remove(table, "auth", auth);
    // 原来用的是哪种拼法就写回哪种，另一种删掉，免得两个键各说各的。
    let spelling = if table.contains_key("api_key_env") && !table.contains_key("env_key") {
        "api_key_env"
    } else {
        "env_key"
    };
    table.remove(if spelling == "env_key" {
        "api_key_env"
    } else {
        "env_key"
    });
    set_or_remove(table, spelling, env_key.clone());
    set_or_remove_int(table, "timeout_secs", fields.timeout_secs)?;

    let env = match (api_key, env_key) {
        (Some(key), Some(name)) => Some(set_env_value(env_text, &name, &key)?),
        _ => None,
    };
    Ok((doc.to_string(), env))
}

/// 删 `[model_providers.<id>]`；还有模型挂在它上面就不删。
pub fn remove_provider(config_text: &str, id: &str) -> Result<String, EditError> {
    let mut doc = parse(config_text)?;
    if sub_table(&doc, "model_providers", id).is_none() {
        return Err(EditError::NotFound(format!("provider `{id}`")));
    }
    let users: Vec<String> = doc
        .get("model")
        .and_then(Item::as_table)
        .map(|models| {
            models
                .iter()
                .filter(|(_, item)| {
                    item.as_table()
                        .and_then(|t| str_of(t, "model_provider"))
                        .as_deref()
                        == Some(id)
                })
                .map(|(alias, _)| alias.to_string())
                .collect()
        })
        .unwrap_or_default();
    if !users.is_empty() {
        return Err(EditError::InUse(format!(
            "还有模型用着 provider `{id}`：{}。先删掉或改挂别的 provider",
            users.join("、")
        )));
    }
    doc["model_providers"]
        .as_table_mut()
        .expect("刚确认过有这一项")
        .remove(id);
    Ok(doc.to_string())
}

/// 一个 completion 模型要写的东西（`ModelUpsertRequest` 去掉版本）。
#[derive(Debug, Clone, Default)]
pub struct ModelFields<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub name: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub efforts: Option<&'a [String]>,
    pub context_window: Option<u64>,
}

/// 新建或整份改 completion 类的 `[model.<alias>]` 这几个键。
pub fn upsert_model(
    config_text: &str,
    alias: &str,
    fields: &ModelFields<'_>,
) -> Result<String, EditError> {
    check_name("模型", alias)?;
    let model = line("model", Some(fields.model))?
        .ok_or_else(|| EditError::Invalid("要写上游的模型 id（`model`）".into()))?;
    let name = line("name", fields.name)?;
    let effort = line("effort", fields.effort)?;
    let mut doc = parse(config_text)?;
    if sub_table(&doc, "model_providers", fields.provider).is_none() {
        return Err(EditError::NotFound(format!(
            "provider `{}`（先建 provider 再挂模型）",
            fields.provider
        )));
    }
    if let Some(existing) = sub_table(&doc, "model", alias)
        && str_of(existing, "type").as_deref() != Some("completion")
    {
        return Err(EditError::InUse(format!(
            "`{alias}` 不是 completion 模型（embedding 之类在文件里改）"
        )));
    }
    let table = table_mut(&mut doc, "model", alias);
    table["type"] = value("completion");
    table["model_provider"] = value(fields.provider);
    table["model"] = value(model);
    set_or_remove(table, "name", name);
    set_or_remove(table, "effort", effort);
    match fields.efforts {
        Some(levels) => {
            let mut array = Array::new();
            for level in levels {
                if let Some(level) = line("efforts", Some(level))? {
                    array.push(level);
                }
            }
            table["efforts"] = value(array);
        }
        None => {
            table.remove("efforts");
        }
    }
    set_or_remove_int(table, "context_window", fields.context_window)?;
    Ok(doc.to_string())
}

/// 删 completion 模型；是默认模型、或被记忆、某个 Agent 指着就不删。
pub fn remove_model(config_text: &str, alias: &str) -> Result<String, EditError> {
    let mut doc = parse(config_text)?;
    let Some(existing) = sub_table(&doc, "model", alias) else {
        return Err(EditError::NotFound(format!("模型 `{alias}`")));
    };
    if str_of(existing, "type").as_deref() != Some("completion") {
        return Err(EditError::InUse(format!(
            "`{alias}` 不是 completion 模型（embedding 之类在文件里改）"
        )));
    }
    let mut users = Vec::new();
    if sub_table_value(&doc, "models", "default").as_deref() == Some(alias) {
        users.push("默认模型（[models] default）".to_string());
    }
    for key in ["model", "embedding"] {
        if sub_table_value(&doc, "memory", key).as_deref() == Some(alias) {
            users.push(format!("[memory] {key}"));
        }
    }
    if let Some(agents) = doc.get("agents").and_then(Item::as_table) {
        for (id, item) in agents.iter() {
            if item.as_table().and_then(|t| str_of(t, "model")).as_deref() == Some(alias) {
                users.push(format!("Agent `{id}`"));
            }
        }
    }
    if !users.is_empty() {
        return Err(EditError::InUse(format!(
            "`{alias}` 还被用着：{}。先换掉再删",
            users.join("、")
        )));
    }
    doc["model"]
        .as_table_mut()
        .expect("刚确认过有这一项")
        .remove(alias);
    Ok(doc.to_string())
}

/// 改 `[models] default`。只能指向一个 completion 模型。
pub fn set_default_model(config_text: &str, alias: &str) -> Result<String, EditError> {
    let mut doc = parse(config_text)?;
    match sub_table(&doc, "model", alias) {
        None => return Err(EditError::NotFound(format!("模型 `{alias}`"))),
        Some(t) if str_of(t, "type").as_deref() != Some("completion") => {
            return Err(EditError::Invalid(format!(
                "`{alias}` 不是 completion 模型，不能当默认"
            )));
        }
        Some(_) => {}
    }
    if doc.get("models").and_then(Item::as_table).is_none() {
        doc.insert("models", Item::Table(Table::new()));
    }
    doc["models"]["default"] = value(alias);
    Ok(doc.to_string())
}

/// 在 `.env` 原文里把 `name` 改成 `secret`：已有的那一行就地换掉（重复的只留第一行），
/// 没有就追加。值用单引号原样保存，所以不许含单引号与换行。别的行一个字节不动。
pub fn set_env_value(env_text: &str, name: &str, secret: &str) -> Result<String, EditError> {
    check_env_name(name)?;
    if secret.contains(['\'', '\n', '\r']) {
        return Err(EditError::Invalid(format!(
            "`{name}` 的值里不能有单引号或换行"
        )));
    }
    let wanted = format!("{name}='{secret}'");
    let names_it = |l: &str| assigns(l, name);
    let mut out = String::with_capacity(env_text.len() + wanted.len() + 1);
    let mut placed = false;
    for l in env_text.split_inclusive('\n') {
        let body = l.strip_suffix('\n').unwrap_or(l);
        if names_it(body) {
            if !placed {
                out.push_str(&wanted);
                out.push('\n');
                placed = true;
            }
            continue;
        }
        out.push_str(l);
    }
    if !placed {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&wanted);
        out.push('\n');
    }
    Ok(out)
}

/// 一行是不是在给 `name` 赋值（`NAME=`、`export NAME =` 都算）。
fn assigns(line: &str, name: &str) -> bool {
    let l = line.trim_start();
    let l = l.strip_prefix("export ").map(str::trim_start).unwrap_or(l);
    l.strip_prefix(name)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// 一次设一批。每个值都要非空：空值等于没配，要去掉就删那一行。
pub fn set_env_values<'a, I>(env_text: &str, vars: I) -> Result<String, EditError>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut out = env_text.to_string();
    let mut any = false;
    for (name, secret) in vars {
        if secret.trim().is_empty() {
            return Err(EditError::Invalid(format!(
                "`{name}` 的值是空的；不要这个变量就删掉它"
            )));
        }
        out = set_env_value(&out, name, secret)?;
        any = true;
    }
    if !any {
        return Err(EditError::Invalid("没有要设的变量".into()));
    }
    Ok(out)
}

/// 删掉给 `name` 赋值的行（重复的一起删）。还被配置引用就不删，说出引用方。
pub fn remove_env_value(
    config_text: &str,
    env_text: &str,
    name: &str,
) -> Result<String, EditError> {
    check_env_name(name)?;
    let users = env_references(&parse(config_text)?)
        .remove(name)
        .unwrap_or_default();
    if !users.is_empty() {
        return Err(EditError::InUse(format!(
            "`{name}` 还在被用：{}",
            users.join("、")
        )));
    }
    let mut out = String::with_capacity(env_text.len());
    let mut found = false;
    for l in env_text.split_inclusive('\n') {
        if assigns(l.strip_suffix('\n').unwrap_or(l), name) {
            found = true;
            continue;
        }
        out.push_str(l);
    }
    if !found {
        return Err(EditError::NotFound(format!(".env 里没有 `{name}`")));
    }
    Ok(out)
}

/// `.env` 里的变量名（按文件顺序），再补上配置引用了、文件里却没有的。**不带任何值**。
/// `secrets` 按同一份 `.env` 读出（含进程环境），只用来答"有没有值"。
pub fn env_view(
    config_text: &str,
    env_text: &str,
    secrets: &Secrets,
    version: String,
) -> Result<ConfigEnvResponse, EditError> {
    let mut refs = env_references(&parse(config_text)?);
    let mut vars = Vec::new();
    for entry in dotenvy::from_read_iter(env_text.as_bytes()) {
        let (name, _) = entry
            .map_err(|e| EditError::Invalid(format!(".env 现在读不成，先在文件里改好：{e}")))?;
        if vars.iter().any(|v: &EnvVarEntry| v.name == name) {
            continue;
        }
        vars.push(EnvVarEntry {
            set: secrets.has(&name),
            used_by: refs.remove(&name).unwrap_or_default(),
            in_file: true,
            name,
        });
    }
    for (name, used_by) in refs {
        vars.push(EnvVarEntry {
            set: secrets.has(&name),
            in_file: false,
            name,
            used_by,
        });
    }
    Ok(ConfigEnvResponse { version, vars })
}

/// 配置里按名字指向 `.env` 的地方：provider / 模型的凭证变量、MCP 的 `bearer_token_env`
/// 与 `env`、启用渠道的凭证。名字 → 引用方（"provider magpie" 这样的短语）。
fn env_references(doc: &DocumentMut) -> BTreeMap<String, Vec<String>> {
    let mut refs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut add = |name: &str, who: String| {
        if !name.trim().is_empty() {
            refs.entry(name.to_string()).or_default().push(who);
        }
    };
    let tables = |section: &str| -> Vec<(String, Table)> {
        doc.get(section)
            .and_then(Item::as_table)
            .into_iter()
            .flat_map(|t| t.iter())
            .filter_map(|(k, item)| item.as_table().map(|t| (k.to_string(), t.clone())))
            .collect()
    };
    for (id, t) in tables("model_providers") {
        if t.get("auth").is_none()
            && let Some(name) = env_key_of(&t)
        {
            add(&name, format!("provider {id}"));
        }
    }
    for (alias, t) in tables("model") {
        if t.get("auth").is_none()
            && let Some(name) = env_key_of(&t)
        {
            add(&name, format!("模型 {alias}"));
        }
    }
    if let Some(servers) = doc
        .get("mcp")
        .and_then(|m| m.get("servers"))
        .and_then(Item::as_table)
    {
        for (server, item) in servers.iter() {
            let Some(t) = item.as_table() else { continue };
            if let Some(name) = str_of(t, "bearer_token_env") {
                add(&name, format!("MCP {server}"));
            }
            for name in t.get("env").and_then(Item::as_array).into_iter().flatten() {
                if let Some(name) = name.as_str() {
                    add(name, format!("MCP {server}"));
                }
            }
        }
    }
    for (platform, key) in [
        (ChannelPlatform::Feishu, "feishu"),
        (ChannelPlatform::Telegram, "telegram"),
    ] {
        let enabled = sub_table(doc, "channels", key)
            .and_then(|t| t.get("enabled"))
            .and_then(Item::as_bool)
            .unwrap_or(false);
        if enabled {
            for name in super::channel_credentials(platform) {
                add(name, format!("渠道 {key}"));
            }
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"default_agent = "assistant"

[agents.assistant]
instructions = "hi"

# 本机代理
[model_providers.magpie]
base_url = "http://127.0.0.1:3425/v1"
api_backend = "responses"
env_key = "MAGPIE_API_KEY"
# 大上下文会被掐断
timeout_secs = 600

[model."magpie-sol"]
type = "completion"
model_provider = "magpie"
model = "codex/gpt-6.1-sol"
name = "Sol"

[model.embedding]
type = "embedding"
api_backend = "ollama_embeddings"
base_url = "http://127.0.0.1:11434"
model = "qwen3-embedding:0.6b"

[models]
default = "magpie-sol"
"#;

    fn secrets(pairs: &[(&str, &str)]) -> Secrets {
        Secrets::from_pairs(pairs.iter().copied())
    }

    #[test]
    fn the_view_lists_what_the_file_says_and_only_whether_a_key_is_set() {
        let view = models_view(
            CONFIG,
            &secrets(&[("MAGPIE_API_KEY", "sk-secret")]),
            "v".into(),
            "c".into(),
            "e".into(),
        )
        .unwrap();
        assert_eq!(view.providers.len(), 1);
        let p = &view.providers[0];
        assert_eq!(p.env_key.as_deref(), Some("MAGPIE_API_KEY"));
        assert!(p.key_set);
        assert_eq!(p.timeout_secs, Some(600));
        assert_eq!(view.models.len(), 1, "embedding 不在这里");
        assert_eq!(view.models[0].alias, "magpie-sol");
        assert_eq!(view.default.as_deref(), Some("magpie-sol"));
        assert!(!serde_json::to_string(&view).unwrap().contains("sk-secret"));

        let unset = models_view(CONFIG, &secrets(&[]), "v".into(), "c".into(), "e".into()).unwrap();
        assert!(!unset.providers[0].key_set);
    }

    #[test]
    fn editing_a_provider_keeps_comments_and_writes_the_key_only_to_env() {
        let (config, env) = upsert_provider(
            CONFIG,
            "OTHER=1\nMAGPIE_API_KEY=old\n",
            "magpie",
            &ProviderFields {
                base_url: Some("http://127.0.0.1:9999/v1"),
                api_backend: Some("responses"),
                env_key: Some("MAGPIE_API_KEY"),
                timeout_secs: Some(900),
                api_key: Some("sk-new"),
                ..ProviderFields::default()
            },
        )
        .unwrap();
        assert!(config.contains("# 本机代理"), "{config}");
        assert!(config.contains("# 大上下文会被掐断"), "{config}");
        assert!(config.contains("http://127.0.0.1:9999/v1"));
        assert!(config.contains("timeout_secs = 900"));
        assert!(!config.contains("sk-new"), "凭证不进 config.toml");
        assert_eq!(env.unwrap(), "OTHER=1\nMAGPIE_API_KEY='sk-new'\n");
    }

    #[test]
    fn a_new_provider_with_a_key_gets_a_derived_variable_name() {
        let (config, env) = upsert_provider(
            CONFIG,
            "",
            "deep-seek",
            &ProviderFields {
                base_url: Some("https://api.deepseek.com"),
                api_backend: Some("chat_completions"),
                api_key: Some("sk-ds"),
                ..ProviderFields::default()
            },
        )
        .unwrap();
        assert!(config.contains("[model_providers.deep-seek]"), "{config}");
        assert!(
            config.contains("env_key = \"DEEP_SEEK_API_KEY\""),
            "{config}"
        );
        assert_eq!(env.unwrap(), "DEEP_SEEK_API_KEY='sk-ds'\n");
        assert!(!config.contains("[model_providers]\n"), "父表保持隐式");
    }

    #[test]
    fn the_old_spelling_of_the_variable_key_is_kept() {
        let text = CONFIG.replace(
            "env_key = \"MAGPIE_API_KEY\"",
            "api_key_env = \"MAGPIE_API_KEY\"",
        );
        let (config, env) = upsert_provider(
            &text,
            "",
            "magpie",
            &ProviderFields {
                env_key: Some("MAGPIE_KEY"),
                ..ProviderFields::default()
            },
        )
        .unwrap();
        assert!(config.contains("api_key_env = \"MAGPIE_KEY\""), "{config}");
        assert!(!config.contains("env_key = "), "{config}");
        assert!(env.is_none(), "没给 key 就不动 .env");
    }

    #[test]
    fn auth_and_a_key_are_refused_together() {
        let error = upsert_provider(
            CONFIG,
            "",
            "codex",
            &ProviderFields {
                auth: Some("chatgpt"),
                api_key: Some("sk"),
                ..ProviderFields::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, EditError::Invalid(_)));
    }

    #[test]
    fn a_provider_in_use_is_not_removed() {
        let error = remove_provider(CONFIG, "magpie").unwrap_err();
        assert!(
            matches!(error, EditError::InUse(ref m) if m.contains("magpie-sol")),
            "{error}"
        );
        assert!(matches!(
            remove_provider(CONFIG, "nope"),
            Err(EditError::NotFound(_))
        ));
    }

    #[test]
    fn models_are_added_edited_and_guarded() {
        let text = upsert_model(
            CONFIG,
            "magpie-terra",
            &ModelFields {
                provider: "magpie",
                model: "codex/gpt-6.1-terra",
                name: Some("Terra"),
                efforts: Some(&["low".into(), "high".into()]),
                context_window: Some(500_000),
                ..ModelFields::default()
            },
        )
        .unwrap();
        assert!(text.contains("[model.magpie-terra]"), "{text}");
        assert!(text.contains("efforts = [\"low\", \"high\"]"), "{text}");
        assert!(text.contains("context_window = 500000"), "{text}");

        let text = set_default_model(&text, "magpie-terra").unwrap();
        assert!(text.contains("default = \"magpie-terra\""));
        let text = remove_model(&text, "magpie-sol").unwrap();
        assert!(!text.contains("magpie-sol"), "{text}");

        assert!(matches!(
            remove_model(&text, "magpie-terra"),
            Err(EditError::InUse(_))
        ));
        assert!(matches!(
            remove_model(&text, "embedding"),
            Err(EditError::InUse(_))
        ));
        assert!(matches!(
            set_default_model(&text, "embedding"),
            Err(EditError::Invalid(_))
        ));
        assert!(matches!(
            upsert_model(
                &text,
                "x",
                &ModelFields {
                    provider: "nope",
                    model: "m",
                    ..ModelFields::default()
                }
            ),
            Err(EditError::NotFound(_))
        ));
    }

    #[test]
    fn an_agent_or_memory_using_a_model_blocks_its_removal() {
        let text = CONFIG
            .replace(
                "instructions = \"hi\"",
                "instructions = \"hi\"\nmodel = \"magpie-sol\"",
            )
            .replace("default = \"magpie-sol\"", "default = \"other\"");
        let error = remove_model(&text, "magpie-sol").unwrap_err();
        assert!(
            matches!(error, EditError::InUse(ref m) if m.contains("assistant")),
            "{error}"
        );
    }

    #[test]
    fn env_values_are_replaced_in_place_and_quoted_literally() {
        let text = "# keys\nexport A_KEY = \"x\"\nB=1\nA_KEY=dup";
        assert_eq!(
            set_env_value(text, "A_KEY", "s$k").unwrap(),
            "# keys\nA_KEY='s$k'\nB=1\n"
        );
        assert_eq!(set_env_value("B=1", "C", "v").unwrap(), "B=1\nC='v'\n");
        assert!(set_env_value("", "C", "it's").is_err());
        assert!(set_env_value("", "bad name", "v").is_err());
        let parsed: Vec<_> =
            dotenvy::from_read_iter(set_env_value("", "C", "s$k\\x").unwrap().as_bytes())
                .map(Result::unwrap)
                .collect();
        assert_eq!(
            parsed,
            vec![("C".to_string(), "s$k\\x".to_string())],
            "单引号里原样"
        );
    }

    #[test]
    fn names_that_do_not_fit_a_table_key_are_refused() {
        for bad in ["", "a b", "a/b", "中文"] {
            assert!(matches!(
                upsert_provider(CONFIG, "", bad, &ProviderFields::default()),
                Err(EditError::Invalid(_))
            ));
        }
    }

    #[test]
    fn the_env_view_lists_names_and_missing_references_but_never_values() {
        let config = format!(
            "{CONFIG}\n[mcp.servers.gh]\ncommand = \"gh-mcp\"\nenv = [\"GH_TOKEN\"]\n\n[channels.telegram]\nenabled = true\n"
        );
        let env = "MAGPIE_API_KEY='sk-secret'\nOTHER=1\nMAGPIE_API_KEY=dup\n";
        let view = env_view(
            &config,
            env,
            &secrets(&[("MAGPIE_API_KEY", "sk-secret"), ("OTHER", "1")]),
            "v".into(),
        )
        .unwrap();
        let names: Vec<_> = view
            .vars
            .iter()
            .map(|v| (v.name.as_str(), v.in_file, v.set))
            .collect();
        assert_eq!(
            names,
            [
                ("MAGPIE_API_KEY", true, true),
                ("OTHER", true, true),
                ("GH_TOKEN", false, false),
                ("TELEGRAM_BOT_TOKEN", false, false),
            ]
        );
        assert_eq!(view.vars[0].used_by, ["provider magpie"]);
        assert_eq!(view.vars[2].used_by, ["MCP gh"]);
        assert_eq!(view.vars[3].used_by, ["渠道 telegram"]);
        assert!(view.vars[1].used_by.is_empty());
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("sk-secret") && !json.contains("dup"));
    }

    #[test]
    fn env_values_are_set_in_a_batch_and_removed_unless_referenced() {
        let env = "# 凭证\nOTHER=1\n";
        let out = set_env_values(env, [("NEW_KEY", "a b"), ("OTHER", "2")]).unwrap();
        assert_eq!(out, "# 凭证\nOTHER='2'\nNEW_KEY='a b'\n");
        assert!(matches!(
            set_env_values(env, [("X", "  ")]),
            Err(EditError::Invalid(_))
        ));
        assert!(matches!(
            set_env_values(env, [("lower", "v")]),
            Err(EditError::Invalid(_))
        ));

        assert_eq!(
            remove_env_value(CONFIG, &out, "NEW_KEY").unwrap(),
            "# 凭证\nOTHER='2'\n"
        );
        assert!(matches!(
            remove_env_value(CONFIG, &out, "MISSING"),
            Err(EditError::NotFound(_))
        ));
        let used = remove_env_value(CONFIG, "MAGPIE_API_KEY=x\n", "MAGPIE_API_KEY");
        assert!(matches!(used, Err(EditError::InUse(m)) if m.contains("provider magpie")));
    }
}
