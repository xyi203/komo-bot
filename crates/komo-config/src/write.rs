//! The config *write* paths: persisting a model selection, a setting, a
//! credential or a channel table — every write komo makes to its own config.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use super::Provider;

/// Persist the provider/model selection into `<home>/config.toml`, preserving
/// every other key already present (schedule, base_url, aux_model, …) **and the
/// comments around them** — `komo init` scaffolds a heavily documented file,
/// and a setter that reformatted it would delete its documentation.
///
/// `model: None` removes the `model` key so the provider's default applies.
/// Returns the path written. Note: any `KOMO_PROVIDER` / `KOMO_MODEL` env
/// vars still take priority over the file at resolve time.
pub fn write_model_selection(
    home: &Path,
    provider: Provider,
    model: Option<&str>,
) -> anyhow::Result<PathBuf> {
    write_config_values(
        home,
        &[
            (
                "provider",
                Some(toml::Value::String(provider.name().to_string())),
            ),
            ("model", model.map(|m| toml::Value::String(m.to_string()))),
        ],
    )
}

/// Set (or, with `None`, remove) settings in `<home>/config.toml`, addressed by
/// dotted key — `"aux_model"`, `"memory.model"` — leaving every other key,
/// table, comment and blank line exactly where it was.
///
/// One write for the whole batch, so a `komo model` command that changes a
/// model and its effort together can never leave one of the two behind.
/// Removing the last key of a table leaves the table itself, which resolves the
/// same and keeps the operator's comments in it.
pub fn write_config_values(
    home: &Path,
    values: &[(&str, Option<toml::Value>)],
) -> anyhow::Result<PathBuf> {
    let path = home.join("config.toml");
    let mut doc = read_config_document(&path)?;
    for (key, value) in values {
        let (leaf, table) = descend(&mut doc, key, &path)?;
        match value {
            Some(value) => {
                let item = item_of(value)?;
                // Assigning through the entry keeps an existing key's own
                // decoration — the comment trailing it stays trailing it.
                match table.get_mut(&leaf) {
                    Some(existing) => *existing = item,
                    None => {
                        table.insert(&leaf, item);
                    }
                }
            }
            None => {
                table.remove(&leaf);
            }
        }
    }
    atomic_write(&path, &doc.to_string(), None)?;
    Ok(path)
}

/// Walk a dotted key down to the table holding its last segment, creating the
/// tables on the way. Returns that segment and the table it belongs in.
fn descend<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    key: &str,
    path: &Path,
) -> anyhow::Result<(String, &'a mut toml_edit::Table)> {
    let mut segments: Vec<&str> = key.split('.').collect();
    let leaf = segments
        .pop()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("invalid config key `{key}`"))?;
    // A new table is appended after everything the document already holds —
    // including the commented-out keys `komo init` leaves at the root. Those
    // would then sit *under* the new header, where uncommenting `aux_model`
    // would quietly file it as `memory.aux_model`, so the trailing block moves
    // ahead of the table instead.
    let orphaned_trailing = match segments.first() {
        Some(first) if !doc.as_table().contains_key(first) => {
            let trailing = doc.trailing().as_str().unwrap_or_default().to_string();
            doc.set_trailing("");
            trailing
        }
        _ => String::new(),
    };
    let mut table = doc.as_table_mut();
    for segment in segments {
        // A table created on the way is *implicit*: `channels.telegram` should
        // render its own header alone, not an empty `[channels]` above it.
        let mut created = toml_edit::Table::new();
        created.set_implicit(true);
        table = table
            .entry(segment)
            .or_insert(toml_edit::Item::Table(created))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("{} has non-table `{segment}`", path.display()))?;
    }
    // On the deepest table, which is the one that renders a header: an implicit
    // parent may print nothing at all, and a prefix on it would take the
    // comments with it.
    if !orphaned_trailing.is_empty() {
        // One blank line between the block and the header it now precedes.
        let separator = if orphaned_trailing.ends_with("\n\n") {
            ""
        } else {
            "\n"
        };
        table
            .decor_mut()
            .set_prefix(format!("{orphaned_trailing}{separator}"));
    }
    Ok((leaf.to_string(), table))
}

/// The scalar and string-list values komo's own writers use. Anything else is
/// refused rather than guessed at — every caller here is in this crate.
fn item_of(value: &toml::Value) -> anyhow::Result<toml_edit::Item> {
    Ok(match value {
        toml::Value::String(v) => toml_edit::value(v.as_str()),
        toml::Value::Integer(v) => toml_edit::value(*v),
        toml::Value::Float(v) => toml_edit::value(*v),
        toml::Value::Boolean(v) => toml_edit::value(*v),
        toml::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                match item {
                    toml::Value::String(v) => array.push(v.as_str()),
                    other => anyhow::bail!("unsupported config list entry `{other}`"),
                }
            }
            toml_edit::value(array)
        }
        other => anyhow::bail!("unsupported config value `{other}`"),
    })
}

fn read_config_document(path: &Path) -> anyhow::Result<toml_edit::DocumentMut> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    text.parse()
        .map_err(|e| anyhow::anyhow!("{} is invalid TOML: {e}", path.display()))
}

/// Update named credentials in `<home>/.env`, preserving comments and every
/// unrelated line. A commented template entry (`# KEY=`) becomes active.
pub fn write_env_values(home: &Path, values: &[(&str, &str)]) -> anyhow::Result<PathBuf> {
    for (key, value) in values {
        if key.is_empty()
            || !key
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        {
            anyhow::bail!("invalid environment variable name `{key}`");
        }
        if value.contains(['\n', '\r']) {
            anyhow::bail!("credential `{key}` must be one line");
        }
    }

    let path = home.join(".env");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut pending = values
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut lines = Vec::new();
    let mut written = std::collections::BTreeSet::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        let candidate = trimmed
            .strip_prefix("# ")
            .unwrap_or(trimmed)
            .trim_start_matches('#');
        let replacement = values.iter().find_map(|(key, value)| {
            candidate
                .strip_prefix(key)
                .filter(|rest| rest.starts_with('='))
                .map(|_| ((*key).to_string(), env_assignment(key, value)))
        });
        if let Some((key, replacement)) = replacement {
            pending.remove(&key);
            if written.insert(key) {
                lines.push(replacement);
            }
        } else {
            lines.push(line.to_string());
        }
    }
    lines.extend(
        pending
            .into_iter()
            .map(|(key, value)| env_assignment(&key, &value)),
    );
    let output = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    atomic_write(&path, &output, Some(0o600))?;
    Ok(path)
}

/// Merge one `[channels.<name>]` table into config.toml without disturbing
/// unrelated runtime settings or channel tables.
pub fn write_channel_config(
    home: &Path,
    channel: &str,
    values: impl IntoIterator<Item = (&'static str, toml::Value)>,
) -> anyhow::Result<PathBuf> {
    let (path, body) = render_channel_config(home, channel, values)?;
    atomic_write(&path, &body, None)?;
    Ok(path)
}

/// Validate and serialize a channel-table merge without touching disk. Setup
/// uses this before changing credentials, so a malformed config cannot leave
/// a newly written secret with no corresponding channel configuration.
pub fn validate_channel_config(
    home: &Path,
    channel: &str,
    values: impl IntoIterator<Item = (&'static str, toml::Value)>,
) -> anyhow::Result<()> {
    let _ = render_channel_config(home, channel, values)?;
    Ok(())
}

fn render_channel_config(
    home: &Path,
    channel: &str,
    values: impl IntoIterator<Item = (&'static str, toml::Value)>,
) -> anyhow::Result<(PathBuf, String)> {
    if channel.is_empty()
        || !channel
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        anyhow::bail!("invalid channel name `{channel}`");
    }
    let path = home.join("config.toml");
    let mut doc = read_config_document(&path)?;
    for (key, value) in values {
        let key = format!("channels.{channel}.{key}");
        let (leaf, table) = descend(&mut doc, &key, &path)?;
        let item = item_of(&value)?;
        match table.get_mut(&leaf) {
            Some(existing) => *existing = item,
            None => {
                table.insert(&leaf, item);
            }
        }
    }
    Ok((path, doc.to_string()))
}

fn env_assignment(key: &str, value: &str) -> String {
    let plain = value.chars().all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':' | '/' | '@' | '+' | '=')
    });
    let value = if plain {
        value.to_string()
    } else {
        serde_json::to_string(value).expect("string serialization cannot fail")
    };
    format!("{key}={value}")
}

/// Replace a file only after the complete new body has reached a sibling
/// temporary file. Existing mode bits are retained; secret env files force
/// owner-only permissions.
fn atomic_write(path: &Path, body: &str, mode: Option<u32>) -> anyhow::Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("komo"),
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode.unwrap_or(0o600));
    }
    let mut file = options.open(&tmp)?;
    let result = (|| -> anyhow::Result<()> {
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions =
                mode.or_else(|| std::fs::metadata(path).ok().map(|m| m.permissions().mode()));
            if let Some(permissions) = permissions {
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(permissions))?;
            }
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_update_activates_a_commented_credential_without_losing_other_lines() {
        let home =
            std::env::temp_dir().join(format!("komo_write_env_test_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".env"), "# TELEGRAM_BOT_TOKEN=\nOTHER=value\n").unwrap();

        write_env_values(&home, &[("TELEGRAM_BOT_TOKEN", "token")]).unwrap();

        assert_eq!(
            std::fs::read_to_string(home.join(".env")).unwrap(),
            "TELEGRAM_BOT_TOKEN=token\nOTHER=value\n"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn env_update_replaces_every_duplicate_and_quotes_special_values() {
        let home = std::env::temp_dir().join(format!(
            "komo_write_duplicate_env_test_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".env"),
            "TELEGRAM_BOT_TOKEN=old\n# TELEGRAM_BOT_TOKEN=template\nTELEGRAM_BOT_TOKEN=older\n",
        )
        .unwrap();

        write_env_values(&home, &[("TELEGRAM_BOT_TOKEN", "has # and spaces")]).unwrap();

        assert_eq!(
            std::fs::read_to_string(home.join(".env")).unwrap(),
            "TELEGRAM_BOT_TOKEN=\"has # and spaces\"\n"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The template `komo init` writes is mostly comments, and they are the
    /// documentation — a setter that reformatted the file would delete it.
    #[test]
    fn writing_a_setting_keeps_the_files_comments_and_layout() {
        let home =
            std::env::temp_dir().join(format!("komo_write_comments_test_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let original = "# komo runtime settings\nprovider = \"deepseek\"\n\n\
                        # --- memory ---\n[memory]\n# served by ollama\n\
                        embedding_model = \"qwen\"\n";
        std::fs::write(home.join("config.toml"), original).unwrap();

        write_config_values(
            &home,
            &[("memory.model", Some(toml::Value::String("pro".into())))],
        )
        .unwrap();
        write_model_selection(&home, Provider::OpenAi, Some("gpt-5.4-mini")).unwrap();
        write_channel_config(&home, "telegram", [("enabled", toml::Value::Boolean(true))]).unwrap();

        let written = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(written.contains("# komo runtime settings"), "{written}");
        assert!(written.contains("# --- memory ---"), "{written}");
        assert!(written.contains("# served by ollama"), "{written}");
        assert!(written.contains("provider = \"openai\""), "{written}");
        assert!(written.contains("model = \"gpt-5.4-mini\""), "{written}");
        assert!(written.contains("[channels.telegram]"), "{written}");
        assert!(
            !written.contains("[channels]\n[channels.telegram]"),
            "an intermediate table gets no empty header of its own: {written}"
        );
        // Re-reading what was written must give back what was set — the
        // property a misplaced table header would break.
        let value: toml::Value = toml::from_str(&written).unwrap();
        assert_eq!(value["memory"]["model"].as_str(), Some("pro"));
        assert_eq!(
            value["channels"]["telegram"]["enabled"].as_bool(),
            Some(true)
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn setting_a_nested_key_keeps_every_unrelated_setting() {
        let home =
            std::env::temp_dir().join(format!("komo_write_values_test_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "provider = \"deepseek\"\naux_model = \"flash\"\n\n[memory]\nembedding_model = \"qwen\"\n",
        )
        .unwrap();

        write_config_values(
            &home,
            &[
                ("memory.model", Some(toml::Value::String("pro".into()))),
                ("memory.effort", Some(toml::Value::String("high".into()))),
            ],
        )
        .unwrap();

        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(home.join("config.toml")).unwrap()).unwrap();
        assert_eq!(value["memory"]["model"].as_str(), Some("pro"));
        assert_eq!(value["memory"]["effort"].as_str(), Some("high"));
        assert_eq!(
            value["memory"]["embedding_model"].as_str(),
            Some("qwen"),
            "the table's other keys survive"
        );
        assert_eq!(value["aux_model"].as_str(), Some("flash"));

        // Clearing removes the key and leaves the rest of the table standing.
        write_config_values(&home, &[("memory.model", None), ("memory.effort", None)]).unwrap();
        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(home.join("config.toml")).unwrap()).unwrap();
        assert!(value["memory"].get("model").is_none());
        assert_eq!(value["memory"]["embedding_model"].as_str(), Some("qwen"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn channel_config_preserves_unrelated_settings() {
        let home =
            std::env::temp_dir().join(format!("komo_write_channel_test_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), "provider = \"codex\"\n").unwrap();

        write_channel_config(&home, "telegram", [("enabled", toml::Value::Boolean(true))]).unwrap();

        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(home.join("config.toml")).unwrap()).unwrap();
        assert_eq!(value["provider"].as_str(), Some("codex"));
        assert_eq!(
            value["channels"]["telegram"]["enabled"].as_bool(),
            Some(true)
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
