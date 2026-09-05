use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use komo_core::domain::{
    approval::{ActionRef, ApprovalRequest, Decision},
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput, parse_args},
};

/// Cap on the textual size of a tool result (entity/service lists are large).
const MAX_BYTES: usize = 8 * 1024;

/// Per-request HTTP timeout — a hung HA instance must not wedge a turn.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Service domains refused outright (hermes' "blocked domains" floor). Home
/// Assistant has *no* service-level access control: anyone with the token can
/// call these to run arbitrary commands as root in the HA container or make the
/// HA server issue arbitrary HTTP requests (SSRF on the LAN). No approval
/// unlocks them — the floor lives in this client, like shell's hardline list.
const BLOCKED_DOMAINS: &[&str] = &[
    "shell_command", // arbitrary shell commands in the HA container
    "command_line",  // sensors/switches that execute shell commands
    "python_script", // sandboxed but can escalate via hass.services.call()
    "pyscript",      // scripting integration with broader access
    "hassio",        // addon control, host shutdown/reboot
    "rest_command",  // arbitrary HTTP requests from the HA server (SSRF)
];

#[derive(Deserialize)]
struct HassArgs {
    action: String,
    /// Service domain (e.g. "light", "switch") for call_service / list_services,
    /// or an entity-id prefix filter for list_entities.
    #[serde(default)]
    domain: Option<String>,
    /// Service name (e.g. "turn_on") for call_service.
    #[serde(default)]
    service: Option<String>,
    /// Target entity id (e.g. "light.kitchen") for get_state / call_service.
    #[serde(default)]
    entity_id: Option<String>,
    /// Area/room name filter for list_entities (matched against friendly_name).
    #[serde(default)]
    area: Option<String>,
    /// Extra service data merged into the call_service body (e.g.
    /// {"brightness_pct": 50}).
    #[serde(default)]
    data: Option<Value>,
    /// Automation config id (the `id` field, e.g. "1718900000000") for
    /// get_automation / save_automation / delete_automation. This is *not* the
    /// `automation.*` entity id — `list_automations` surfaces both.
    #[serde(default)]
    id: Option<String>,
    /// The automation config object for save_automation (alias / trigger /
    /// condition / action / mode).
    #[serde(default)]
    config: Option<Value>,
}

/// Talks to a Home Assistant instance over its REST API: read entity states,
/// discover services, and call services (turn devices on/off, etc.). Configured
/// via `HASS_TOKEN` + `HASS_URL` (see `config.rs`).
pub struct HomeAssistantTool {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl HomeAssistantTool {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("failed to build reqwest client"),
            base_url,
            token,
        }
    }

    async fn get_json(&self, path: &str) -> anyhow::Result<Value> {
        let resp = self
            .client
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| crate::http::transport_error(e, "request to Home Assistant failed"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(crate::http::status_error(status, "Home Assistant", &body));
        }
        serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("invalid JSON from Home Assistant: {e}"))
    }
}

#[async_trait]
impl Tool for HomeAssistantTool {
    fn name(&self) -> &'static str {
        "homeassistant"
    }

    fn description(&self) -> &'static str {
        "Query and control a Home Assistant smart-home instance. \
         action=\"list_entities\" lists entities + current state (optional \
         `domain` and/or `area` filter); \
         action=\"get_state\" returns one entity's full state + attributes \
         (requires `entity_id`); \
         action=\"list_services\" discovers callable services per domain (use it \
         to learn what `call_service` accepts); \
         action=\"call_service\" invokes a service to change something (requires \
         `domain` + `service`, e.g. light/turn_on, usually with `entity_id`, \
         plus optional `data`). \
         To edit automations: action=\"list_automations\" lists each automation's \
         entity id, on/off state, name, and config `id`; \
         action=\"get_automation\" returns one automation's full config (requires \
         `id`); action=\"save_automation\" creates or updates one (requires `id` + \
         a `config` object with at least `trigger` and `action`; pass the whole \
         config — it replaces the automation); action=\"delete_automation\" removes \
         one (requires `id`). Control and automation-edit actions ask for approval; \
         saving/deleting an automation persists to automations.yaml and reloads HA."
    }

    /// These calls can park on an approval prompt, so they must outlast one.
    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(komo_core::domain::tool::APPROVAL_BOUND)
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list_entities", "get_state", "list_services", "call_service",
                             "list_automations", "get_automation", "save_automation", "delete_automation"],
                    "description": "The operation to perform."
                },
                "domain": {
                    "type": "string",
                    "description": "Service domain for call_service/list_services (e.g. \"light\"); or an entity-id prefix filter for list_entities."
                },
                "service": {
                    "type": "string",
                    "description": "Service name for call_service (e.g. \"turn_on\", \"turn_off\", \"toggle\", \"set_temperature\")."
                },
                "entity_id": {
                    "type": "string",
                    "description": "Target entity id (e.g. \"light.kitchen\") for get_state or call_service."
                },
                "area": {
                    "type": "string",
                    "description": "Area/room name filter for list_entities (e.g. \"kitchen\"); matched against friendly names."
                },
                "data": {
                    "type": "object",
                    "description": "Extra service data for call_service, e.g. {\"brightness_pct\": 50}."
                },
                "id": {
                    "type": "string",
                    "description": "Automation config id for get/save/delete_automation (e.g. \"1718900000000\" or a slug). NOT the automation.* entity id; list_automations shows both."
                },
                "config": {
                    "type": "object",
                    "description": "Automation config for save_automation: an object with `alias`, `trigger`, optional `condition`, `action`, and `mode`. Pass the complete config — save replaces the whole automation."
                }
            },
            "required": ["action"]
        })
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: HassArgs = parse_args(&input)?;
        Ok(ToolOutput::text(self.run(args, ctx).await?))
    }
}

impl HomeAssistantTool {
    /// The action dispatch, kept as one `String`-returning helper rather than
    /// wrapping ~20 return points in `ToolOutput::text`. Every arm's text is
    /// already model-facing prose (refusals included — HA refusals are terminal
    /// answers, not errors), and there is no structured view to expose yet.
    async fn run(&self, args: HassArgs, ctx: &ToolContext) -> anyhow::Result<String> {
        match args.action.as_str() {
            "list_entities" => {
                let states = self.get_json("/api/states").await?;
                let mut out =
                    format_entities(&states, args.domain.as_deref(), args.area.as_deref());
                truncate_to_char_boundary(&mut out, MAX_BYTES);
                Ok(out)
            }

            "list_services" => {
                // Validate the optional domain filter so a bogus value can't be
                // mistaken for "no filter".
                if let Some(d) = &args.domain
                    && !valid_name(d)
                {
                    return Ok(format!("Refused: invalid domain `{d}`."));
                }
                let services = self.get_json("/api/services").await?;
                let mut out = format_services(&services, args.domain.as_deref());
                truncate_to_char_boundary(&mut out, MAX_BYTES);
                Ok(out)
            }

            "get_state" => {
                let id = args.entity_id.ok_or_else(|| {
                    anyhow::anyhow!("`entity_id` is required for action=get_state")
                })?;
                if !valid_entity_id(&id) {
                    return Ok(format!("Refused: invalid entity_id `{id}`."));
                }
                let state = self.get_json(&format!("/api/states/{id}")).await?;
                let mut out =
                    serde_json::to_string_pretty(&state).unwrap_or_else(|_| state.to_string());
                truncate_to_char_boundary(&mut out, MAX_BYTES);
                Ok(out)
            }

            "call_service" => {
                let domain = args.domain.ok_or_else(|| {
                    anyhow::anyhow!("`domain` is required for action=call_service")
                })?;
                let service = args.service.ok_or_else(|| {
                    anyhow::anyhow!("`service` is required for action=call_service")
                })?;

                // Validate name *shape* before anything else: the domain and
                // service are interpolated into the request path, so rejecting
                // non-`[a-z_0-9]` tokens closes path-traversal/SSRF (e.g.
                // domain="../../api/config") and blocklist-bypass (e.g.
                // "shell_command/../light").
                if !valid_name(&domain) {
                    return Ok(format!("Refused: invalid service domain `{domain}`."));
                }
                if !valid_name(&service) {
                    return Ok(format!("Refused: invalid service name `{service}`."));
                }
                if BLOCKED_DOMAINS.contains(&domain.as_str()) {
                    return Ok(format!(
                        "Refused: service domain `{domain}` is blocked for security \
                         (arbitrary code execution / SSRF on the HA host). Blocked: {}.",
                        BLOCKED_DOMAINS.join(", ")
                    ));
                }
                if let Some(id) = &args.entity_id
                    && !valid_entity_id(id)
                {
                    return Ok(format!("Refused: invalid entity_id `{id}`."));
                }

                // Assemble the service body: caller-supplied `data` (must be an
                // object) plus the target `entity_id` if given.
                let mut body = match args.data {
                    Some(Value::Object(m)) => m,
                    Some(_) => anyhow::bail!("`data` must be a JSON object"),
                    None => serde_json::Map::new(),
                };
                if let Some(id) = &args.entity_id {
                    body.insert("entity_id".to_string(), json!(id));
                }

                // Changing physical-world state is side-effecting: gate it
                // through the approver (session-scoped per service so repeats of
                // the same action don't re-prompt). This sits *above* the
                // blocklist floor — blocked domains never reach here.
                let target = args
                    .entity_id
                    .as_deref()
                    .map(|id| format!(" on {id}"))
                    .unwrap_or_default();
                let request =
                    ApprovalRequest::normal(format!("Home Assistant: {domain}.{service}{target}"))
                        .with_scope_key(format!("homeassistant:{domain}.{service}"))
                        .with_action(ActionRef::Service {
                            domain: domain.to_string(),
                            service: service.to_string(),
                        });
                if let Decision::Deny { feedback } = ctx.decide(&request).await {
                    return Ok(refusal("Service call", feedback));
                }

                let resp = self
                    .client
                    .post(format!("{}/api/services/{domain}/{service}", self.base_url))
                    .bearer_auth(&self.token)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| {
                        crate::http::transport_error(e, "request to Home Assistant failed")
                    })?;
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(crate::http::status_error(status, "Home Assistant", &text));
                }
                // The response is the array of entities that changed state.
                let body = serde_json::from_str::<Value>(&text).ok();
                let changed = body
                    .as_ref()
                    .and_then(|v| v.as_array().map(|a| a.len()))
                    .unwrap_or(0);
                let mut out = format!(
                    "Called {domain}.{service}{target}; {changed} entit{} changed.",
                    if changed == 1 { "y" } else { "ies" }
                );
                // The post-call state, not the requested one: a device may clamp,
                // round or ignore what was asked for, and the model reports what
                // it was told here.
                if let Some(body) = &body {
                    let states = format_changed(body);
                    if !states.is_empty() {
                        out.push('\n');
                        out.push_str(&states);
                    }
                }
                truncate_to_char_boundary(&mut out, MAX_BYTES);
                Ok(out)
            }

            "list_automations" => {
                let states = self.get_json("/api/states").await?;
                let mut out = format_automations(&states);
                truncate_to_char_boundary(&mut out, MAX_BYTES);
                Ok(out)
            }

            "get_automation" => {
                let id = require_automation_id(&args.id)?;
                let cfg = self
                    .get_json(&format!("/api/config/automation/config/{id}"))
                    .await?;
                let mut out =
                    serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| cfg.to_string());
                truncate_to_char_boundary(&mut out, MAX_BYTES);
                Ok(out)
            }

            "save_automation" => {
                let id = require_automation_id(&args.id)?;
                let config = match args.config {
                    Some(c @ Value::Object(_)) => c,
                    Some(_) => anyhow::bail!("`config` must be a JSON object"),
                    None => anyhow::bail!("`config` is required for action=save_automation"),
                };

                // Same hardline floor as call_service: an automation can invoke
                // services too, so writing one that calls `shell_command` /
                // `python_script` / etc. would smuggle arbitrary code execution
                // past the call_service blocklist. Refuse before any write — no
                // approval unlocks it.
                if let Some(bad) = blocked_service_in(&config) {
                    return Ok(format!(
                        "Refused: automation calls blocked service domain `{bad}` \
                         (arbitrary code execution / SSRF on the HA host). Blocked: {}.",
                        BLOCKED_DOMAINS.join(", ")
                    ));
                }

                let name = config
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(|a| format!(" \"{a}\""))
                    .unwrap_or_default();
                let request =
                    ApprovalRequest::normal(format!("Home Assistant: save automation {id}{name}"))
                        .with_scope_key("homeassistant:automation.save".to_string())
                        .with_action(ActionRef::Service {
                            domain: "automation".to_string(),
                            service: "save".to_string(),
                        });
                if let Decision::Deny { feedback } = ctx.decide(&request).await {
                    return Ok(refusal("Automation save", feedback));
                }

                let resp = self
                    .client
                    .post(format!(
                        "{}/api/config/automation/config/{id}",
                        self.base_url
                    ))
                    .bearer_auth(&self.token)
                    .json(&config)
                    .send()
                    .await
                    .map_err(|e| {
                        crate::http::transport_error(e, "request to Home Assistant failed")
                    })?;
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(crate::http::status_error(status, "Home Assistant", &text));
                }
                Ok(format!(
                    "Saved automation {id}{name}; HA reloaded automations."
                ))
            }

            "delete_automation" => {
                let id = require_automation_id(&args.id)?;
                // Deleting an automation is irreversible (its YAML block is gone):
                // gate it as Dangerous so the approver warns prominently.
                let request = ApprovalRequest::dangerous(
                    format!("Home Assistant: delete automation {id}"),
                    "The automation's config is permanently removed from automations.yaml."
                        .to_string(),
                )
                .with_scope_key("homeassistant:automation.delete".to_string())
                .with_action(ActionRef::Service {
                    domain: "automation".to_string(),
                    service: "delete".to_string(),
                });
                if let Decision::Deny { feedback } = ctx.decide(&request).await {
                    return Ok(refusal("Automation delete", feedback));
                }

                let resp = self
                    .client
                    .delete(format!(
                        "{}/api/config/automation/config/{id}",
                        self.base_url
                    ))
                    .bearer_auth(&self.token)
                    .send()
                    .await
                    .map_err(|e| {
                        crate::http::transport_error(e, "request to Home Assistant failed")
                    })?;
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(crate::http::status_error(status, "Home Assistant", &text));
                }
                Ok(format!("Deleted automation {id}; HA reloaded automations."))
            }

            other => Err(anyhow::anyhow!(
                "unknown action `{other}` (expected list_entities/get_state/list_services/call_service/list_automations/get_automation/save_automation/delete_automation)"
            )),
        }
    }
}

/// A refusal, carrying the reason the user gave (if any) so the model adjusts
/// rather than re-asking for the same action.
fn refusal(what: &str, feedback: Option<String>) -> String {
    match feedback {
        Some(reason) => {
            format!("{what} rejected by the user; nothing was changed. They said: {reason}")
        }
        None => format!("{what} rejected by user; nothing was changed."),
    }
}

/// A valid HA domain/service token: `^[a-z][a-z0-9_]*$`. Rejecting anything
/// else stops path traversal / SSRF when the token is interpolated into
/// `/api/services/{domain}/{service}`.
fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A valid HA entity id: `^[a-z_][a-z0-9_]*\.[a-z0-9_]+$` — exactly one dot,
/// with a valid domain and a non-empty object id.
fn valid_entity_id(s: &str) -> bool {
    let Some((domain, object)) = s.split_once('.') else {
        return false;
    };
    if object.is_empty() || object.contains('.') {
        return false;
    }
    let domain_ok = {
        let mut c = domain.chars();
        matches!(c.next(), Some(ch) if ch.is_ascii_lowercase() || ch == '_')
            && c.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    };
    let object_ok = object
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    domain_ok && object_ok
}

/// Require the `id` arg for automation actions and validate its shape: the id
/// is interpolated into `/api/config/automation/config/{id}`, so only
/// `[A-Za-z0-9_-]` is allowed — no dots or slashes that could traverse to
/// another endpoint. HA's generated ids are digit strings; hand-written slugs
/// add letters/underscores/hyphens.
fn require_automation_id(id: &Option<String>) -> anyhow::Result<String> {
    let id = id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("`id` is required for this automation action"))?;
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("invalid automation id `{id}` (expected [A-Za-z0-9_-])");
    }
    Ok(id.to_string())
}

/// Walk an automation config for any service call whose domain is on the
/// hardline `BLOCKED_DOMAINS` list. HA names service calls under a `service`
/// key (or the newer `action` key) as a `"domain.name"` string; we check those
/// values only, so a benign `alias` mentioning a domain name won't trip the
/// floor. Returns the offending domain if found.
fn blocked_service_in(config: &Value) -> Option<&'static str> {
    match config {
        Value::Object(map) => {
            for (key, val) in map {
                if (key == "service" || key == "action")
                    && let Some(s) = val.as_str()
                    && let Some((domain, _)) = s.split_once('.')
                    && let Some(blocked) = BLOCKED_DOMAINS.iter().find(|d| **d == domain)
                {
                    return Some(blocked);
                }
                if let Some(hit) = blocked_service_in(val) {
                    return Some(hit);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(blocked_service_in),
        _ => None,
    }
}

/// Render `/api/states` into one line per automation:
/// `automation.foo = on (Friendly Name) [id=123]`, sorted.
fn format_automations(states: &Value) -> String {
    let Some(arr) = states.as_array() else {
        return "Unexpected response from Home Assistant.".to_string();
    };
    let mut lines: Vec<String> = arr
        .iter()
        .filter_map(|s| {
            let entity_id = s.get("entity_id").and_then(Value::as_str)?;
            if !entity_id.starts_with("automation.") {
                return None;
            }
            let attrs = s.get("attributes");
            let name = attrs
                .and_then(|a| a.get("friendly_name"))
                .and_then(Value::as_str);
            let cfg_id = attrs.and_then(|a| a.get("id")).and_then(Value::as_str);
            let state = s.get("state").and_then(Value::as_str).unwrap_or("unknown");
            let name_part = name.map(|n| format!(" ({n})")).unwrap_or_default();
            let id_part = cfg_id
                .map(|i| format!(" [id={i}]"))
                .unwrap_or_else(|| " [id=?]".to_string());
            Some(format!("{entity_id} = {state}{name_part}{id_part}"))
        })
        .collect();
    lines.sort();
    if lines.is_empty() {
        "No automations found.".to_string()
    } else {
        lines.join("\n")
    }
}

/// Render `/api/states` (a JSON array) into sorted `entity_id = state (name)`
/// lines, optionally keeping only entities in `domain` (the `domain.` entity-id
/// prefix) and/or whose friendly name / area mentions `area`.
fn format_entities(states: &Value, domain: Option<&str>, area: Option<&str>) -> String {
    let Some(arr) = states.as_array() else {
        return "Unexpected response from Home Assistant.".to_string();
    };
    let area_lc = area.map(str::to_lowercase);
    let mut lines: Vec<String> = arr
        .iter()
        .filter_map(|s| {
            let id = s.get("entity_id").and_then(Value::as_str)?;
            if let Some(d) = domain
                && !id.starts_with(&format!("{d}."))
            {
                return None;
            }
            let attrs = s.get("attributes");
            let name = attrs
                .and_then(|a| a.get("friendly_name"))
                .and_then(Value::as_str);
            if let Some(area_lc) = &area_lc {
                let hay_name = name.unwrap_or("").to_lowercase();
                let hay_area = attrs
                    .and_then(|a| a.get("area"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                if !hay_name.contains(area_lc.as_str()) && !hay_area.contains(area_lc.as_str()) {
                    return None;
                }
            }
            let state = s.get("state").and_then(Value::as_str).unwrap_or("unknown");
            Some(entity_line(id, state, name))
        })
        .collect();
    lines.sort();
    if lines.is_empty() {
        "No matching entities found.".to_string()
    } else {
        lines.join("\n")
    }
}

/// One entity's `entity_id = state (Friendly Name)` line.
fn entity_line(id: &str, state: &str, name: Option<&str>) -> String {
    match name {
        Some(n) => format!("{id} = {state} ({n})"),
        None => format!("{id} = {state}"),
    }
}

/// Attributes a climate entity's line carries: the state alone (`cool`) says
/// nothing about the setpoint the user asked about, so a report built on it is
/// a report of what was requested rather than of what the device holds.
const CLIMATE_ATTRS: &[&str] = &[
    "temperature",
    "current_temperature",
    "hvac_action",
    "fan_mode",
];

/// Render a `POST /api/services/...` response — the entities whose state
/// changed — one per line, in `format_entities`' style.
fn format_changed(changed: &Value) -> String {
    let Some(arr) = changed.as_array() else {
        return String::new();
    };
    let mut lines: Vec<String> = arr
        .iter()
        .filter_map(|s| {
            let id = s.get("entity_id").and_then(Value::as_str)?;
            let attrs = s.get("attributes");
            let state = s.get("state").and_then(Value::as_str).unwrap_or("unknown");
            let name = attrs
                .and_then(|a| a.get("friendly_name"))
                .and_then(Value::as_str);
            let mut line = entity_line(id, state, name);
            if id.starts_with("climate.") {
                let extras: Vec<String> = CLIMATE_ATTRS
                    .iter()
                    .filter_map(|key| {
                        let v = attrs?.get(*key).filter(|v| !v.is_null())?;
                        let shown = match v.as_str() {
                            Some(s) => s.to_string(),
                            None => v.to_string(),
                        };
                        Some(format!("{key}: {shown}"))
                    })
                    .collect();
                if !extras.is_empty() {
                    line.push_str(&format!(" [{}]", extras.join(", ")));
                }
            }
            Some(line)
        })
        .collect();
    lines.sort();
    lines.join("\n")
}

/// Render `/api/services` into compact `domain.service — description` lines,
/// optionally limited to one `domain`.
fn format_services(services: &Value, domain: Option<&str>) -> String {
    let Some(arr) = services.as_array() else {
        return "Unexpected response from Home Assistant.".to_string();
    };
    let mut lines = Vec::new();
    for entry in arr {
        let d = entry.get("domain").and_then(Value::as_str).unwrap_or("");
        if let Some(want) = domain
            && d != want
        {
            continue;
        }
        if let Some(map) = entry.get("services").and_then(Value::as_object) {
            for (name, info) in map {
                let desc = info
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if desc.is_empty() {
                    lines.push(format!("{d}.{name}"));
                } else {
                    lines.push(format!("{d}.{name} — {desc}"));
                }
            }
        }
    }
    if lines.is_empty() {
        "No matching services found.".to_string()
    } else {
        lines.join("\n")
    }
}

/// Truncates to at most `max_bytes`, backing up so the cut never splits a
/// multi-byte UTF-8 character (mirrors `web_fetch`).
fn truncate_to_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n…[truncated]");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states() -> Value {
        json!([
            {"entity_id": "light.kitchen", "state": "on",
             "attributes": {"friendly_name": "Kitchen Light"}},
            {"entity_id": "light.bedroom", "state": "off",
             "attributes": {"friendly_name": "Bedroom Light"}},
            {"entity_id": "switch.fan", "state": "off", "attributes": {}}
        ])
    }

    // ── validation ────────────────────────────────────────────────────────

    #[test]
    fn valid_name_accepts_ha_tokens_and_rejects_traversal() {
        assert!(valid_name("light"));
        assert!(valid_name("turn_on"));
        assert!(valid_name("media_player"));
        assert!(!valid_name("")); // empty
        assert!(!valid_name("Light")); // uppercase
        assert!(!valid_name("3d")); // leading digit
        assert!(!valid_name("../../api/config")); // path traversal
        assert!(!valid_name("shell_command/../light")); // bypass attempt
    }

    #[test]
    fn valid_entity_id_requires_one_dot_and_clean_parts() {
        assert!(valid_entity_id("light.kitchen"));
        assert!(valid_entity_id("sensor.temperature_1"));
        assert!(!valid_entity_id("light")); // no dot
        assert!(!valid_entity_id("light.")); // empty object
        assert!(!valid_entity_id("light.a.b")); // two dots
        assert!(!valid_entity_id("../secrets")); // traversal
        assert!(!valid_entity_id("Light.Kitchen")); // uppercase
    }

    // ── format_entities ───────────────────────────────────────────────────

    #[test]
    fn format_entities_renders_sorted_lines_with_names() {
        let out = format_entities(&states(), None, None);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "light.bedroom = off (Bedroom Light)");
        assert_eq!(lines[1], "light.kitchen = on (Kitchen Light)");
        assert_eq!(lines[2], "switch.fan = off");
    }

    #[test]
    fn format_entities_filters_by_domain() {
        let out = format_entities(&states(), Some("light"), None);
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("light.kitchen"));
        assert!(!out.contains("switch.fan"));
    }

    #[test]
    fn format_entities_filters_by_area_against_friendly_name() {
        let out = format_entities(&states(), None, Some("kitchen"));
        assert_eq!(out, "light.kitchen = on (Kitchen Light)");
    }

    #[test]
    fn format_entities_empty_filter_reports_none() {
        let out = format_entities(&states(), Some("climate"), None);
        assert_eq!(out, "No matching entities found.");
    }

    // ── format_changed ────────────────────────────────────────────────────

    #[test]
    fn format_changed_reports_climate_setpoint_after_the_call() {
        let body = json!([
            {"entity_id": "climate.living_room", "state": "cool",
             "attributes": {"friendly_name": "Living Room AC", "temperature": 24,
                            "current_temperature": 27.5, "hvac_action": "cooling",
                            "fan_mode": "auto"}}
        ]);
        let out = format_changed(&body);
        assert_eq!(
            out,
            "climate.living_room = cool (Living Room AC) \
             [temperature: 24, current_temperature: 27.5, hvac_action: cooling, fan_mode: auto]"
        );
    }

    #[test]
    fn format_changed_renders_other_domains_as_state_alone() {
        let body = json!([
            {"entity_id": "switch.fan", "state": "on",
             "attributes": {"friendly_name": "Fan", "device_class": "switch"}}
        ]);
        assert_eq!(format_changed(&body), "switch.fan = on (Fan)");
    }

    #[test]
    fn format_changed_on_an_empty_or_unexpected_body_adds_nothing() {
        assert_eq!(format_changed(&json!([])), "");
        assert_eq!(format_changed(&json!({"result": "ok"})), "");
    }

    // ── format_services ───────────────────────────────────────────────────

    #[test]
    fn format_services_compacts_and_filters_by_domain() {
        let services = json!([
            {"domain": "light", "services": {
                "turn_on": {"description": "Turn on lights"},
                "turn_off": {"description": "Turn off lights"}}},
            {"domain": "switch", "services": {
                "toggle": {"description": "Toggle a switch"}}}
        ]);
        let out = format_services(&services, Some("light"));
        assert!(out.contains("light.turn_on — Turn on lights"));
        assert!(out.contains("light.turn_off — Turn off lights"));
        assert!(!out.contains("switch.toggle"));
    }

    // ── call_service guards ───────────────────────────────────────────────

    /// Unreachable base_url: every test here is refused *before* any HTTP.
    fn tool() -> HomeAssistantTool {
        HomeAssistantTool::new("http://127.0.0.1:1".to_string(), "token".to_string())
    }

    /// The approver now rides on the turn's context, not the tool.
    fn allow() -> ToolContext {
        crate::test_support::approving_ctx("cli:test")
    }

    fn deny() -> ToolContext {
        crate::test_support::detached_ctx("cli:test")
    }

    #[tokio::test]
    async fn call_service_blocked_domain_refused_even_when_approved() {
        // AllowAll proves the blocklist sits below approval: still refused.
        let out = tool()
            .call(json!({"action": "call_service", "domain": "shell_command", "service": "run", "data": {"command": "rm -rf /"}}), &allow())
            .await
            .unwrap()
            .text;
        assert!(out.contains("blocked for security"));
    }

    #[tokio::test]
    async fn call_service_invalid_domain_refused() {
        let out = tool()
            .call(
                json!({"action": "call_service", "domain": "../../api/config", "service": "get"}),
                &allow(),
            )
            .await
            .unwrap()
            .text;
        assert!(out.contains("invalid service domain"));
    }

    #[tokio::test]
    async fn call_service_rejected_when_approval_denied() {
        let out = tool()
            .call(json!({"action": "call_service", "domain": "light", "service": "turn_on", "entity_id": "light.kitchen"}), &deny())
            .await
            .unwrap()
            .text;
        assert!(out.contains("rejected by user"));
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let err = tool()
            .call(json!({"action": "bogus"}), &deny())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown action"));
    }

    // ── automation editing ────────────────────────────────────────────────

    fn automation_states() -> Value {
        json!([
            {"entity_id": "automation.morning", "state": "on",
             "attributes": {"friendly_name": "Morning Routine", "id": "1700000000001"}},
            {"entity_id": "automation.away", "state": "off",
             "attributes": {"friendly_name": "Away Mode", "id": "1700000000002"}},
            {"entity_id": "light.kitchen", "state": "on", "attributes": {}}
        ])
    }

    #[test]
    fn require_automation_id_validates_shape() {
        assert_eq!(
            require_automation_id(&Some("1700000000001".into())).unwrap(),
            "1700000000001"
        );
        assert_eq!(
            require_automation_id(&Some("porch_light-2".into())).unwrap(),
            "porch_light-2"
        );
        assert!(require_automation_id(&None).is_err());
        assert!(require_automation_id(&Some("".into())).is_err());
        assert!(require_automation_id(&Some("../secrets".into())).is_err()); // traversal
        assert!(require_automation_id(&Some("a/b".into())).is_err()); // slash
        assert!(require_automation_id(&Some("a.b".into())).is_err()); // dot
    }

    #[test]
    fn format_automations_lists_only_automations_with_id() {
        let out = format_automations(&automation_states());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "automation.away = off (Away Mode) [id=1700000000002]"
        );
        assert_eq!(
            lines[1],
            "automation.morning = on (Morning Routine) [id=1700000000001]"
        );
        assert!(!out.contains("light.kitchen"));
    }

    #[test]
    fn blocked_service_in_catches_nested_blocked_call() {
        let cfg = json!({
            "alias": "evil",
            "trigger": [{"platform": "state", "entity_id": "binary_sensor.x"}],
            "action": [
                {"service": "light.turn_on", "target": {"entity_id": "light.a"}},
                {"service": "shell_command.rm_rf"}
            ]
        });
        assert_eq!(blocked_service_in(&cfg), Some("shell_command"));
    }

    #[test]
    fn blocked_service_in_catches_newer_action_key() {
        let cfg = json!({"action": [{"action": "python_script.exec"}]});
        assert_eq!(blocked_service_in(&cfg), Some("python_script"));
    }

    #[test]
    fn blocked_service_in_allows_clean_config_and_ignores_alias_text() {
        let cfg = json!({
            "alias": "talk about shell_command.run in text",
            "trigger": [{"platform": "sun", "event": "sunset"}],
            "action": [{"service": "light.turn_on", "target": {"entity_id": "light.porch"}}]
        });
        assert_eq!(blocked_service_in(&cfg), None);
    }

    #[tokio::test]
    async fn save_automation_blocked_service_refused_even_when_approved() {
        let out = tool()
            .call(json!({"action": "save_automation", "id": "1", "config": { "action": [{"service": "command_line.run"}]}}), &allow())
            .await
            .unwrap()
            .text;
        assert!(out.contains("blocked service domain"));
    }

    #[tokio::test]
    async fn save_automation_rejected_when_approval_denied() {
        let out = tool()
            .call(json!({"action": "save_automation", "id": "1", "config": { "alias": "ok", "action": [{"service": "light.turn_on"}]}}), &deny())
            .await
            .unwrap()
            .text;
        assert!(out.contains("rejected by user"));
    }

    #[tokio::test]
    async fn delete_automation_rejected_when_approval_denied() {
        let out = tool()
            .call(json!({"action": "delete_automation", "id": "1"}), &deny())
            .await
            .unwrap()
            .text;
        assert!(out.contains("rejected by user"));
    }

    #[tokio::test]
    async fn save_automation_invalid_id_errors() {
        let err = tool()
            .call(
                json!({"action": "save_automation", "id": "../x", "config": {"action": []}}),
                &allow(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid automation id"));
    }
}
