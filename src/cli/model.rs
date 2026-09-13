//! Model inspection and switching (`komo model list`, `komo model set`, and
//! one verb per secondary role: `aux`, `memory`, `embedding`).
//!
//! `list` shows every model komo runs — the conversation's, the aux backend's,
//! the memory pipeline's and the embedding backend's — with where each value
//! comes from, straight from the shared `ConfigSnapshot` so it can never
//! disagree with what the agent actually resolves. The setters persist into
//! `~/.komo/config.toml`; none of them touches the database or needs the API
//! key to be present.
//!
//! Every role is **validated while the person who typed it is still there**: a
//! qualified id naming an unknown provider and an effort level the backend does
//! not accept are both refused with the accepted list, rather than written and
//! discovered as a warning at the next gateway boot. What is only *warned*
//! about is a missing credential — that is a thing to go fix, not a typo.
//!
//! Config does not hot-reload, so each setter ends by naming
//! `komo config reload`.

use komo_config::{
    ConfigReport, ConfigSnapshot, ModelConfig, Origin, Provider, write_config_values,
    write_model_selection,
};
use komo_infra::codex::{self, CodexAuth};

fn auth_present(provider: Provider, report: &ConfigReport) -> bool {
    match provider {
        Provider::Codex => CodexAuth::load().is_ok(),
        _ => report.key_present(provider),
    }
}

fn credential_line(provider: Provider, report: &ConfigReport) -> String {
    match provider {
        Provider::Codex => format!(
            "Codex OAuth {}  {}",
            codex::codex_auth_file_path().display(),
            if auth_present(provider, report) {
                "✓ logged in"
            } else {
                "✗ missing"
            }
        ),
        _ => format!(
            "{}  {}",
            provider.api_key_var(),
            if report.key_present(provider) {
                "✓ set"
            } else {
                "✗ missing"
            }
        ),
    }
}

/// Human label for a value's provenance.
fn origin_label(origin: Origin, env_var: &str, default_label: &'static str) -> String {
    match origin {
        Origin::Env => format!("env {env_var}"),
        Origin::File => "config.toml".to_string(),
        Origin::Default => default_label.to_string(),
    }
}

fn resolve_set_args(
    provider_or_model: &str,
    model: Option<String>,
) -> anyhow::Result<(Provider, Option<String>, bool)> {
    match Provider::parse(provider_or_model) {
        Ok(provider) => Ok((provider, model, false)),
        Err(parse_err) => {
            if model.is_none() && codex::looks_like_codex_model_id(provider_or_model) {
                Ok((Provider::Codex, Some(provider_or_model.to_string()), true))
            } else {
                Err(parse_err)
            }
        }
    }
}

async fn preferred_codex_model() -> String {
    let token = match CodexAuth::load() {
        Ok(auth) => auth.resolve().await.ok(),
        Err(_) => None,
    };
    codex::codex_model_ids(token.as_deref())
        .await
        .into_iter()
        .next()
        .unwrap_or_else(|| Provider::Codex.default_model().to_string())
}

/// Show the current provider/model (with its source) and list all providers.
pub async fn list(config: &ConfigSnapshot) -> anyhow::Result<()> {
    // A provider that failed to parse resolved to a fallback — surface the
    // problem instead of presenting the fallback as the configuration.
    if let Some(issue) = config
        .report
        .issues
        .iter()
        .find(|i| i.path == "model.provider")
    {
        anyhow::bail!("{}", issue.message);
    }
    let provider = config.runtime.model.provider;
    let model = &config.runtime.model.model;
    let provider_source = origin_label(config.report.provider_origin, "KOMO_PROVIDER", "default");
    let model_source = origin_label(config.report.model_origin, "KOMO_MODEL", "provider default");

    println!("Current");
    println!("  provider  {}  ({provider_source})", provider.name());
    println!("  model     {model}  ({model_source})");
    println!("  auth      {}", credential_line(provider, &config.report));

    if provider == Provider::Codex {
        let token = match CodexAuth::load() {
            Ok(auth) => auth.resolve().await.ok(),
            Err(_) => None,
        };
        let models = codex::codex_model_ids(token.as_deref()).await;
        if !models.is_empty() {
            println!(
                "  codex models {}",
                models
                    .iter()
                    .take(6)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    println!();
    println!("Available providers  (* = active)");
    for p in Provider::ALL {
        println!(
            "  {} {:<11} default {:<26} auth {}",
            if p == provider { "*" } else { " " },
            p.name(),
            p.default_model(),
            if auth_present(p, &config.report) {
                "✓"
            } else {
                "·"
            },
        );
    }

    role_lines(config);

    println!();
    println!("Switch with: komo model set <provider> [model]");
    println!("Codex shortcut: komo model set gpt-5.5");
    println!("Secondary roles: komo model aux|memory <model> [--effort <level>]");
    println!("Embeddings:      komo model embedding <model> [--url <base-url>]");
    Ok(())
}

/// Switch the provider (and optionally the model), persisting to config.toml.
pub async fn set(
    config: &ConfigSnapshot,
    provider_str: &str,
    model: Option<String>,
) -> anyhow::Result<()> {
    let home = &config.runtime.home;
    let (provider, model, inferred_provider) = resolve_set_args(provider_str, model)?;
    let resolved_model = match (provider, model) {
        (Provider::Codex, None) => Some(preferred_codex_model().await),
        (_, model) => model,
    };
    let path = write_model_selection(home, provider, resolved_model.as_deref())?;

    let effective = resolved_model
        .clone()
        .unwrap_or_else(|| provider.default_model().to_string());
    println!("provider = {}", provider.name());
    if inferred_provider {
        println!("model    = {effective}  (inferred codex provider)");
    } else if resolved_model.is_some() {
        println!("model    = {effective}");
    } else {
        println!("model    = {effective}  (provider default)");
    }
    println!("wrote {}", path.display());

    // Env overrides beat the file at resolve time — provenance from the
    // pre-write snapshot still tells us whether any are set.
    if config.report.provider_origin == Origin::Env || config.report.model_origin == Origin::Env {
        eprintln!(
            "note: KOMO_PROVIDER/KOMO_MODEL are set and override config.toml; \
             unset them for this change to take effect"
        );
    }
    if !auth_present(provider, &config.report) {
        match provider {
            // Report the loader's own diagnosis: "missing" and "malformed" want
            // different fixes, and it knows every path it accepted.
            Provider::Codex => match CodexAuth::load() {
                Ok(_) => {}
                Err(e) => eprintln!("note: {e:#}"),
            },
            _ => eprintln!(
                "note: {} is not set — add it to {}/.env before using {}",
                provider.api_key_var(),
                home.display(),
                provider.name()
            ),
        }
    }
    Ok(())
}

/// A secondary model role — everything komo runs that is not the conversation
/// itself. One enum rather than two near-identical command handlers: the two
/// differ only in which keys hold them and what they fall back to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `aux_model`: the policy reviewer, the compactor and the delegate's
    /// preamble.
    Aux,
    /// `[memory] model`: the reflective reviewer, the consolidator, the outcome
    /// verdict and recall screening. Falls back to the aux backend.
    Memory,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::Aux => "aux",
            Role::Memory => "memory",
        }
    }

    /// The dotted config keys holding this role, model first.
    fn keys(self) -> (&'static str, &'static str) {
        match self {
            Role::Aux => ("aux_model", "aux_effort"),
            Role::Memory => ("memory.model", "memory.effort"),
        }
    }

    fn env_vars(self) -> (&'static str, &'static str) {
        match self {
            Role::Aux => ("KOMO_AUX_MODEL", "KOMO_AUX_EFFORT"),
            Role::Memory => ("KOMO_MEMORY_MODEL", "KOMO_MEMORY_EFFORT"),
        }
    }

    /// What the role is configured with — `None` when it inherits.
    fn configured(self, model: &ModelConfig) -> (Option<&str>, Option<&str>) {
        match self {
            Role::Aux => (model.aux_model.as_deref(), model.aux_effort.as_deref()),
            Role::Memory => (
                model.memory_model.as_deref(),
                model.memory_effort.as_deref(),
            ),
        }
    }

    /// What the role actually resolves to, inheritance applied.
    fn resolved(self, model: &ModelConfig) -> ModelConfig {
        match self {
            Role::Aux => model.aux_variant(),
            Role::Memory => model.memory_variant(),
        }
    }

    /// What this role runs once its own keys are gone — the conversation's
    /// model for aux, and the aux backend for memory.
    fn fallback_model(self, config: &ModelConfig) -> String {
        match self {
            Role::Aux => config.model.clone(),
            Role::Memory => config.aux_variant().model,
        }
    }

    /// Where this role's model comes from when it names none of its own.
    fn inherits(self) -> &'static str {
        match self {
            Role::Aux => "inherits model",
            Role::Memory => "inherits aux",
        }
    }
}

fn effort_label(effort: Option<&str>) -> String {
    match effort {
        Some(level) => format!("effort {level}"),
        None => "effort provider default".to_string(),
    }
}

/// The four models komo runs, each with what it resolves to and where that came
/// from. Read from the same resolved snapshot the gateway boots on, so a role
/// that inherits says so rather than repeating a value it does not hold.
fn role_lines(config: &ConfigSnapshot) {
    let model = &config.runtime.model;
    println!();
    println!("Roles  (what each model komo runs resolves to)");
    println!(
        "  {:<10} {:<26} {:<26} {}",
        "main",
        model.model,
        effort_label(model.effort.as_deref()),
        model.provider.name()
    );
    for role in [Role::Aux, Role::Memory] {
        let resolved = role.resolved(model);
        let (configured, _) = role.configured(model);
        let source = match configured {
            Some(_) => role.keys().0.to_string(),
            None => role.inherits().to_string(),
        };
        println!(
            "  {:<10} {:<26} {:<26} {:<10} ({source})",
            role.label(),
            resolved.model,
            effort_label(resolved.effort.as_deref()),
            model.provider_of(&resolved.model).name(),
        );
    }
    match &config.runtime.embedding {
        Some(embedding) => println!(
            "  {:<10} {:<26} {:<26} ollama  ([memory] embedding_model)",
            "embedding", embedding.model, embedding.url
        ),
        // The one role whose absence changes behavior rather than falling back:
        // without it recall is lexical-only, so a Chinese question structurally
        // cannot reach an English memory.
        None => println!(
            "  {:<10} off — recall is lexical-only, so a question cannot reach a \
             memory written in another language",
            "embedding"
        ),
    }
}

/// Set (or clear) one secondary role, persisting both its keys in one write.
///
/// With neither a model nor an effort nor `--clear`, this reports what the role
/// resolves to instead of changing it — the bare verb is a question.
pub fn set_role(
    config: &ConfigSnapshot,
    role: Role,
    model: Option<String>,
    effort: Option<String>,
    clear: bool,
) -> anyhow::Result<()> {
    let current = &config.runtime.model;
    let (model_key, effort_key) = role.keys();
    let (env_model, env_effort) = role.env_vars();

    if !clear && model.is_none() && effort.is_none() {
        let resolved = role.resolved(current);
        let (configured, _) = role.configured(current);
        println!(
            "{} = {}  ({})",
            role.label(),
            resolved.model,
            configured.map(|_| model_key).unwrap_or(role.inherits())
        );
        println!("  {}", effort_label(resolved.effort.as_deref()));
        println!(
            "  set with: komo model {} <model> [--effort <level>]",
            role.label()
        );
        return Ok(());
    }

    if clear {
        let path = write_config_values(
            &config.runtime.home,
            &[(model_key, None), (effort_key, None)],
        )?;
        println!(
            "{} = {}  ({})",
            role.label(),
            role.fallback_model(current),
            role.inherits()
        );
        println!("wrote {}", path.display());
        return reload_note(env_model, env_effort);
    }

    // Effort is validated against whichever backend this role will run on —
    // the model being set now, else the one it already resolves to.
    let target = model
        .clone()
        .unwrap_or_else(|| role.resolved(current).model);
    let provider = current.provider_of(&target);
    let effort = effort.map(|level| level.trim().to_string());
    check_effort(provider, effort.as_deref())?;

    let mut values: Vec<(&str, Option<toml::Value>)> = Vec::new();
    if let Some(model) = model.as_deref() {
        values.push((model_key, Some(toml::Value::String(model.to_string()))));
    }
    if let Some(level) = effort.as_deref() {
        values.push((effort_key, Some(toml::Value::String(level.to_string()))));
    }
    let path = write_config_values(&config.runtime.home, &values)?;

    println!("{} = {target}  ({})", role.label(), provider.name());
    match effort.as_deref() {
        Some(level) => println!("  effort {level}"),
        None => println!(
            "  {}",
            effort_label(role.resolved(current).effort.as_deref())
        ),
    }
    println!("wrote {}", path.display());
    if !auth_present(provider, &config.report) && provider.uses_api_key() {
        eprintln!(
            "note: {} is not set — {} turns will fail until it is added to {}/.env",
            provider.api_key_var(),
            role.label(),
            config.runtime.home.display()
        );
    }
    reload_note(env_model, env_effort)
}

/// Point the embedding backend at another Ollama model (or clear it).
///
/// Vectors are stored with the model that produced them, so a switch does not
/// invalidate the store — it leaves every existing memory unmatched by the
/// semantic arm until `komo memory backfill` re-embeds them, which is the one
/// thing worth saying out loud here.
pub fn set_embedding(
    config: &ConfigSnapshot,
    model: Option<String>,
    url: Option<String>,
    clear: bool,
) -> anyhow::Result<()> {
    let home = &config.runtime.home;
    if clear {
        let path = write_config_values(home, &[("memory.embedding_model", None)])?;
        println!("embedding = off — recall falls back to lexical-only matching");
        println!("wrote {}", path.display());
        return reload_note("", "");
    }
    if model.is_none() && url.is_none() {
        match &config.runtime.embedding {
            Some(embedding) => {
                println!("embedding = {}  ({})", embedding.model, embedding.url);
            }
            None => println!("embedding = off — recall is lexical-only"),
        }
        println!("  set with: komo model embedding <model> [--url <base-url>]");
        return Ok(());
    }
    if let Some(url) = url.as_deref()
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        anyhow::bail!("embedding url must start with http:// or https:// (got {url:?})");
    }

    let mut values: Vec<(&str, Option<toml::Value>)> = Vec::new();
    if let Some(model) = model.as_deref() {
        values.push((
            "memory.embedding_model",
            Some(toml::Value::String(model.trim().to_string())),
        ));
    }
    if let Some(url) = url.as_deref() {
        values.push((
            "memory.embedding_url",
            Some(toml::Value::String(url.trim().to_string())),
        ));
    }
    let path = write_config_values(home, &values)?;
    let effective = model
        .clone()
        .or_else(|| config.runtime.embedding.as_ref().map(|e| e.model.clone()))
        .unwrap_or_default();
    println!("embedding = {effective}");
    if let Some(url) = url.as_deref() {
        println!("  url {url}");
    }
    println!("wrote {}", path.display());
    let changed_model = model.as_deref().is_some_and(|m| {
        config
            .runtime
            .embedding
            .as_ref()
            .is_none_or(|e| e.model != m)
    });
    if changed_model {
        eprintln!(
            "note: vectors are stored with the model that produced them — run \
             `komo memory backfill` to re-embed, or recall stays lexical for \
             everything already stored"
        );
    }
    reload_note("", "")
}

/// Refuse a level the backend's scale does not have, naming the ones it does.
///
/// Resolution would only *warn* about this and read it as unset, which is right
/// for a file written long ago and wrong for a word someone just typed and is
/// waiting on — the same split `/model` makes in the TUI.
fn check_effort(provider: Provider, effort: Option<&str>) -> anyhow::Result<()> {
    let Some(level) = effort else { return Ok(()) };
    if provider.accepts_effort(level) {
        return Ok(());
    }
    anyhow::bail!(
        "effort {level:?} is not valid for {} (accepted: {:?})",
        provider.name(),
        provider.efforts()
    )
}

/// Config is read once at boot, so a write here changes nothing until the
/// gateway restarts — and an env override changes nothing ever.
fn reload_note(env_model: &str, env_effort: &str) -> anyhow::Result<()> {
    for var in [env_model, env_effort] {
        if !var.is_empty() && std::env::var(var).is_ok_and(|v| !v.is_empty()) {
            eprintln!("note: {var} is set and overrides config.toml; unset it for this to apply");
        }
    }
    println!("restart to apply: komo config reload");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_args_accept_provider_plus_model() {
        let (provider, model, inferred) =
            resolve_set_args("openai", Some("gpt-4o".into())).unwrap();
        assert_eq!(provider, Provider::OpenAi);
        assert_eq!(model.as_deref(), Some("gpt-4o"));
        assert!(!inferred);
    }

    #[test]
    fn set_args_infers_codex_provider_from_codex_model() {
        let (provider, model, inferred) = resolve_set_args("gpt-5.5", None).unwrap();
        assert_eq!(provider, Provider::Codex);
        assert_eq!(model.as_deref(), Some("gpt-5.5"));
        assert!(inferred);
    }

    #[test]
    fn set_args_keeps_non_codex_models_as_unknown_providers() {
        assert!(resolve_set_args("gpt-4o-mini", None).is_err());
    }

    #[test]
    fn an_effort_the_backend_lacks_is_refused_with_the_accepted_list() {
        let error = check_effort(Provider::DeepSeek, Some("medium")).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("medium"), "{message}");
        assert!(
            message.contains("none") && message.contains("high"),
            "it names what the backend does accept: {message}"
        );
        assert!(check_effort(Provider::DeepSeek, Some("none")).is_ok());
        assert!(
            check_effort(Provider::DeepSeek, None).is_ok(),
            "setting only a model leaves the effort alone"
        );
    }
}
