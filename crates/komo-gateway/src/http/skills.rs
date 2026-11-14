//! `GET /v1/skills` 与 `PUT /v1/skills/{name}`：设置页「连接 → 技能」那张表。
//!
//! 启停落在 `runtime/skills-disabled.json`（`komo skills enable|disable` 同一个文件，`docs/komo_bot.md`
//! §5.6）：关掉只是不进系统提示的目录行、`skill://` 不再挂它，**不删文件**。

use std::path::Path as FsPath;

use axum::Json;
use axum::extract::{Path, State};
use komo_agent::skills::{OfferContext, Skill, SkillRegistry, one_line};
use komo_kernel::protocol::http::{EnabledRequest, SkillSource, SkillView, SkillsResponse};

use super::Api;
use super::error::{ApiFailure, ApiResult};

pub async fn list(State(api): State<Api>) -> Json<SkillsResponse> {
    Json(view(&api))
}

/// 开 / 关一个 skill，持久；换完马上重渲目录行，之后受理的 Run 按新的来（在跑的 Run 冻结着旧的）。
pub async fn set(
    State(api): State<Api>,
    Path(name): Path<String>,
    Json(request): Json<EnabledRequest>,
) -> ApiResult<Json<SkillsResponse>> {
    let registry = registry(&api);
    if registry.find(&name).is_none() {
        return Err(ApiFailure::not_found(format!("skill `{name}`")));
    }
    let written = if request.enabled {
        registry.enable(&name)
    } else {
        registry.disable(&name)
    };
    written.map_err(|error| {
        ApiFailure::new(
            komo_kernel::protocol::http::ErrorCode::Internal,
            format!("写 skill 的停用名单失败：{error}"),
        )
    })?;
    api.state.refresh_skills_prompt();
    Ok(Json(view(&api)))
}

fn registry(api: &Api) -> SkillRegistry {
    api.state.skills.read().expect("skills 注册表").clone()
}

fn view(api: &Api) -> SkillsResponse {
    let registry = registry(api);
    let disabled = registry.disabled();
    // 门控口径同系统提示的目录行：这台 Gateway 此刻挂着的工具
    let offer = OfferContext::here(api.state.tool_names());
    let snapshot = api.state.snapshot();
    let komo_dir = snapshot.start_only.data_dir.join("skills");
    let shared_dir = api
        .state
        .shared_home
        .clone()
        .or_else(|| komo_config::user_home().ok())
        .map(|home| home.join(".agents").join("skills"));
    let skills = registry
        .list()
        .into_iter()
        // 被同名的、更靠前的那份盖住的不列：按名字开关，只有生效的那份有意义
        .filter(|skill| skill.shadowed_by.is_none())
        .map(|skill| {
            let enabled = !disabled.contains(&skill.name);
            SkillView {
                source: source_of(&skill, &komo_dir, shared_dir.as_deref()),
                unavailable: unavailable(&skill, &offer),
                path: skill
                    .path
                    .parent()
                    .unwrap_or(&skill.path)
                    .display()
                    .to_string(),
                description: one_line(&skill.description),
                name: skill.name,
                enabled,
            }
        })
        .collect();
    SkillsResponse { skills }
}

fn source_of(skill: &Skill, komo_dir: &FsPath, shared_dir: Option<&FsPath>) -> SkillSource {
    if skill.path.starts_with(komo_dir) {
        SkillSource::Komo
    } else if shared_dir.is_some_and(|dir| skill.path.starts_with(dir)) {
        SkillSource::Shared
    } else {
        SkillSource::Configured
    }
}

/// 开着也进不了目录行的原因。
fn unavailable(skill: &Skill, offer: &OfferContext) -> Option<String> {
    if skill.offered(offer) {
        return None;
    }
    if !skill.issues.is_empty() {
        return Some(skill.issues.join("；"));
    }
    Some("平台或工具不满足（platforms / requires_tools）".to_string())
}
