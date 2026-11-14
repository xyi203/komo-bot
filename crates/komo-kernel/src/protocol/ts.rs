//! 协议类型 → TypeScript（`docs/clients.md` D3），只在 `ts` feature 下编进来。
//!
//! 生成物提交在 `packages/protocol/src/generated/`，由下面的测试保证它是最新的：
//! `cargo test -p komo-kernel --features ts`；类型改了要重新生成，加 `KOMO_UPDATE_TS=1`。
//!
//! 导出的是客户端用得到的集合（会话列表与生命周期、提交 Run、模型清单、每轮改动、事件页与 SSE 帧、错误体、healthz、登录）。其余
//! 协议类型留到界面迁移（C1）用到时再加：给类型补 `cfg_attr(feature = "ts", derive(ts_rs::TS))`，
//! 再把它列进 [`export_all`]。
//!
//! 与线上 JSON 的几处约定：
//! - `OffsetDateTime` 是 RFC 3339 字符串；`Seq` 是 JSON 数字（大整数配置成 `number`）。
//! - `Event` 没有 derive 能描述的形状（自定义序列化），线上给 [`WireEvent`] 这个裸信封；`data` 随
//!   `type` 变，客户端按 `type` 自己收窄。
//! - `skip_serializing_if` 的 `Vec` 字段（如 `ApiError.keys`）线上为空时不出现，TS 里仍写成必填，
//!   客户端读的时候按可选处理。

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use ts_rs::{Config, TS};

use super::{
    CancelRunResponse, ChatListResponse, ConfigCheckResponse, ConfigEditResponse,
    ConfigEnvResponse, ConfigModelsResponse, ConfigReloadResponse, ConfigSettingsResponse,
    ConfigVersionRequest, CreateSessionRequest,
    DefaultModelRequest, DeleteSessionRequest, EmbeddingUpsertRequest, EnvSetRequest, ErrorBody,
    EventPage, EventQuery, FileDiff, ForkSessionResponse, HealthResponse,
    InterventionAnswerRequest, InterventionAnswerResponse, InterventionListResponse,
    ManualCronRunResponse, MatterActionRequest, MatterAddRequest, MatterChatResponse, MatterDetail,
    MatterListResponse, MatterRunsResponse,
    EnabledRequest, McpReconnectRequest, McpReconnectResponse, McpServersResponse, SkillsResponse, MemoryAddRequest,
    MemoryAddResponse, MemoryClassifyRequest, MemoryDetail, MemoryIndexStatus, MemoryListResponse,
    MemoryRevisionRequest, ModelUpsertRequest, ModelsResponse, MonitorListResponse,
    NameSessionRequest, ProviderModelsRequest,
    AuthMeResponse, AuthRevokeResponse, AuthSessionsResponse, DeviceTokenRequest, DeviceTokenResponse,
    ProviderModelsResponse, ProviderUpsertRequest, PurgeBlocked, PurgeSessionResponse,
    RebuildIndexResponse, SessionChanges, SessionLifecycleResponse, SessionListResponse,
    SettingRequest, SseFrame, SubmitRunRequest, SubmitRunResponse, TaskListResponse,
    UploadAttachmentResponse,
};

/// `GET /v1/sessions/{id}/events` 与 SSE `event` 帧里一条事件的线上信封（[`crate::events::Event`] 的 JSON）。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct WireEvent {
    pub v: u32,
    #[ts(type = "number")]
    pub seq: u64,
    pub event_id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    /// RFC 3339。
    pub at: String,
    /// 事件类型，如 `message.user`、`run.completed`。
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    pub event_type: String,
    /// 随 `type` 变的负载。
    #[ts(type = "unknown")]
    pub data: serde_json::Value,
}

/// 把全部导出类型（连同依赖）写进 `dir`，外加一个 `index.ts`。
pub fn export_all(dir: &Path) -> Result<(), String> {
    let cfg = Config::new().with_large_int("number").with_out_dir(dir);
    let each = |result: Result<(), ts_rs::ExportError>| result.map_err(|error| error.to_string());
    each(ErrorBody::export_all(&cfg))?;
    each(HealthResponse::export_all(&cfg))?;
    each(SessionListResponse::export_all(&cfg))?;
    each(TaskListResponse::export_all(&cfg))?;
    each(ChatListResponse::export_all(&cfg))?;
    each(EventQuery::export_all(&cfg))?;
    each(EventPage::export_all(&cfg))?;
    each(SseFrame::export_all(&cfg))?;
    each(AuthMeResponse::export_all(&cfg))?;
    each(AuthSessionsResponse::export_all(&cfg))?;
    each(AuthRevokeResponse::export_all(&cfg))?;
    each(DeviceTokenRequest::export_all(&cfg))?;
    each(DeviceTokenResponse::export_all(&cfg))?;
    each(WireEvent::export_all(&cfg))?;
    // C1：会话列表与菜单、对话、改动页用到的。
    each(CreateSessionRequest::export_all(&cfg))?;
    each(ForkSessionResponse::export_all(&cfg))?;
    each(NameSessionRequest::export_all(&cfg))?;
    each(DeleteSessionRequest::export_all(&cfg))?;
    each(SessionLifecycleResponse::export_all(&cfg))?;
    each(PurgeSessionResponse::export_all(&cfg))?;
    each(PurgeBlocked::export_all(&cfg))?;
    each(SubmitRunRequest::export_all(&cfg))?;
    each(SubmitRunResponse::export_all(&cfg))?;
    each(UploadAttachmentResponse::export_all(&cfg))?;
    each(CancelRunResponse::export_all(&cfg))?;
    each(ModelsResponse::export_all(&cfg))?;
    each(SessionChanges::export_all(&cfg))?;
    each(FileDiff::export_all(&cfg))?;
    // C1 B 段：待处理页。
    each(InterventionListResponse::export_all(&cfg))?;
    each(InterventionAnswerRequest::export_all(&cfg))?;
    each(InterventionAnswerResponse::export_all(&cfg))?;
    // C1 C 段：记忆、事项、定时、设置。
    each(MemoryListResponse::export_all(&cfg))?;
    each(MemoryDetail::export_all(&cfg))?;
    each(MemoryAddRequest::export_all(&cfg))?;
    each(MemoryAddResponse::export_all(&cfg))?;
    each(MemoryClassifyRequest::export_all(&cfg))?;
    each(MemoryRevisionRequest::export_all(&cfg))?;
    each(MemoryIndexStatus::export_all(&cfg))?;
    each(RebuildIndexResponse::export_all(&cfg))?;
    each(MatterListResponse::export_all(&cfg))?;
    each(MonitorListResponse::export_all(&cfg))?;
    each(MatterDetail::export_all(&cfg))?;
    each(MatterAddRequest::export_all(&cfg))?;
    each(MatterActionRequest::export_all(&cfg))?;
    each(MatterChatResponse::export_all(&cfg))?;
    each(MatterRunsResponse::export_all(&cfg))?;
    each(ManualCronRunResponse::export_all(&cfg))?;
    each(ConfigCheckResponse::export_all(&cfg))?;
    each(ConfigReloadResponse::export_all(&cfg))?;
    each(ConfigModelsResponse::export_all(&cfg))?;
    each(ConfigEditResponse::export_all(&cfg))?;
    each(ConfigSettingsResponse::export_all(&cfg))?;
    each(ConfigEnvResponse::export_all(&cfg))?;
    each(ConfigVersionRequest::export_all(&cfg))?;
    each(DefaultModelRequest::export_all(&cfg))?;
    each(ProviderUpsertRequest::export_all(&cfg))?;
    each(EmbeddingUpsertRequest::export_all(&cfg))?;
    each(ModelUpsertRequest::export_all(&cfg))?;
    each(ProviderModelsRequest::export_all(&cfg))?;
    each(ProviderModelsResponse::export_all(&cfg))?;
    each(SettingRequest::export_all(&cfg))?;
    each(EnvSetRequest::export_all(&cfg))?;
    each(McpServersResponse::export_all(&cfg))?;
    each(McpReconnectRequest::export_all(&cfg))?;
    each(McpReconnectResponse::export_all(&cfg))?;
    each(EnabledRequest::export_all(&cfg))?;
    each(SkillsResponse::export_all(&cfg))?;

    let names: BTreeSet<String> = std::fs::read_dir(dir)
        .map_err(|error| error.to_string())?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".ts").map(str::to_string))
        .filter(|name| name != "index")
        .collect();
    let mut index =
        String::from("// 由 `cargo test -p komo-kernel --features ts` 生成，不要手改。\n");
    for name in names {
        index.push_str(&format!("export type {{ {name} }} from \"./{name}\";\n"));
    }
    std::fs::write(dir.join("index.ts"), index).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_dir_files(dir: &Path) -> std::collections::BTreeMap<String, String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| {
                        let entry = entry.ok()?;
                        let name = entry.file_name().into_string().ok()?;
                        Some((name, std::fs::read_to_string(entry.path()).ok()?))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 提交进仓库的生成物与现在的类型一致。类型改了：`KOMO_UPDATE_TS=1 cargo test -p komo-kernel --features ts`。
    #[test]
    fn committed_typescript_matches_the_protocol_types() {
        let committed =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/protocol/src/generated");
        let fresh = std::env::temp_dir().join(format!("komo-ts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fresh);
        std::fs::create_dir_all(&fresh).expect("临时目录");
        export_all(&fresh).expect("导出");

        let want = read_dir_files(&fresh);
        let _ = std::fs::remove_dir_all(&fresh);
        if std::env::var_os("KOMO_UPDATE_TS").is_some() {
            let _ = std::fs::remove_dir_all(&committed);
            std::fs::create_dir_all(&committed).expect("生成物目录");
            for (name, text) in &want {
                std::fs::write(committed.join(name), text).expect("写生成物");
            }
            return;
        }
        let have = read_dir_files(&committed);
        assert_eq!(
            have.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "生成物的文件清单变了：KOMO_UPDATE_TS=1 cargo test -p komo-kernel --features ts"
        );
        for (name, text) in &want {
            assert_eq!(
                have.get(name),
                Some(text),
                "{name} 过期了：KOMO_UPDATE_TS=1 cargo test -p komo-kernel --features ts"
            );
        }
    }
}
