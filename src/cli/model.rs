//! Model inspection and switching (`komo model list`, `komo model set`).
//!
//! `list` shows the resolved provider/model and where each value comes from —
//! straight from the shared `ConfigSnapshot`'s provenance report, so it can
//! never disagree with what the agent actually resolves — plus every available
//! provider. `set` persists a new selection into `~/.komo/config.toml`.
//! Neither touches the database or requires the API key to be present.

use komo_config::{ConfigReport, ConfigSnapshot, Origin, Provider, write_model_selection};
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

    println!();
    println!("Switch with: komo model set <provider> [model]");
    println!("Codex shortcut: komo model set gpt-5.5");
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
}
