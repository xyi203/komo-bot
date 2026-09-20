//! 统一错误体与状态码（§13.1）。
//!
//! 每一种失败只在这里映射一次：`ErrorCode` 是给客户端看的那个判别式（`komo-client`
//! 按它认回来），HTTP 状态码是给代理与 curl 看的。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use komo_kernel::protocol::config::KeyPath;
use komo_kernel::protocol::http::{ApiError, ErrorBody, ErrorCode};
use komo_kernel::traits::{GatewayError, LedgerError, RepoError, StoreError};

/// 一次失败的响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiFailure {
    pub status: StatusCode,
    pub error: ApiError,
    /// 覆盖响应体。**只在失败正文不是错误结构时用**——今天只有 `purge` 的
    /// [`PurgeBlocked`](komo_kernel::protocol::http::PurgeBlocked)：§8.10 要求它"列出要先
    /// 处置什么"，那是一份数据，不是一个错误消息。
    pub body: Option<serde_json::Value>,
}

impl ApiFailure {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiFailure {
            status: status_of(code),
            error: ApiError::new(code, message),
            body: None,
        }
    }

    pub fn not_found(what: impl std::fmt::Display) -> Self {
        ApiFailure::new(ErrorCode::NotFound, format!("找不到 {what}"))
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        ApiFailure::new(ErrorCode::InvalidRequest, message)
    }

    /// 这个会话不接受新输入（§8.10）：409，而且**说清楚是哪种状态**——`closing`（还在
    /// 服务、只是不收新活）、`deleted`（逻辑删除完成）、`purged`（内容已回收）三句话
    /// 对操作者意味着三件不同的事。
    pub fn session_not_open(state: komo_kernel::types::status::SessionState) -> Self {
        let what = match state {
            komo_kernel::types::status::SessionState::Active => "接受输入",
            komo_kernel::types::status::SessionState::Closing => {
                "正在关闭（`closing`）：未完成的 Run 照常跑完，但不再接受新输入"
            }
            komo_kernel::types::status::SessionState::Deleted => {
                "已逻辑删除（`deleted`）：要接着用它得先明确恢复，不是「重新开始」"
            }
            komo_kernel::types::status::SessionState::Purged => {
                "内容已回收（`purged`）：只剩墓碑行，不接受任何新内容"
            }
        };
        let mut failure = ApiFailure::new(
            ErrorCode::Conflict,
            format!("这个会话当前状态是 {}（{what}）", state.as_str()),
        );
        failure.status = StatusCode::CONFLICT;
        failure
    }

    /// 结论与种类不符（§7.5：结论按种类分派，不混用）：`422` + `ErrorCode::InvalidRequest`。
    ///
    /// 状态码是 422 而不是 400：请求本身读得懂，是**它与此刻那条 Intervention 对不上**
    /// ——和配置校验不过同一类（`CONFIG_INVALID` 也是 422）。
    pub fn verdict_mismatch(message: impl Into<String>) -> Self {
        let mut failure = ApiFailure::new(ErrorCode::InvalidRequest, message);
        failure.status = StatusCode::UNPROCESSABLE_ENTITY;
        failure
    }

    /// `purge` 的引用检查没过（§8.10 第 3 条）：409，正文是 [`PurgeBlocked`]。
    ///
    /// [`PurgeBlocked`]: komo_kernel::protocol::http::PurgeBlocked
    pub fn purge_blocked(blocked: komo_kernel::protocol::http::PurgeBlocked) -> Self {
        let message = format!(
            "先处置这些再回收内容：{}",
            blocked
                .blockers
                .iter()
                .map(|blocker| format!("{}（{}）", blocker.what, blocker.detail))
                .collect::<Vec<_>>()
                .join("；")
        );
        let mut failure = ApiFailure::new(ErrorCode::Conflict, message);
        failure.status = StatusCode::CONFLICT;
        failure.body = serde_json::to_value(&blocked).ok();
        failure
    }

    /// 配置校验不过：带上键名定位，**不带值**（§3 第 3 步）。
    pub fn config_invalid(message: impl Into<String>, keys: Vec<KeyPath>) -> Self {
        let mut failure = ApiFailure::new(ErrorCode::ConfigInvalid, message);
        failure.error.keys = keys;
        failure
    }

    pub fn code(&self) -> ErrorCode {
        self.error.code
    }
}

/// 每个 `ErrorCode` 对应的状态码。
pub fn status_of(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorCode::RequestKeyConflict | ErrorCode::VersionConflict | ErrorCode::Conflict => {
            StatusCode::CONFLICT
        }
        ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        ErrorCode::Denied => StatusCode::FORBIDDEN,
        ErrorCode::ConfigInvalid => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::VectorUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Corrupt | ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        match self.body.clone() {
            // `PurgeBlocked` 这类：状态码与错误码仍然在，但正文是那份数据（§8.10）。
            Some(body) => (self.status, Json(body)).into_response(),
            None => (self.status, Json(ErrorBody { error: self.error })).into_response(),
        }
    }
}

impl From<LedgerError> for ApiFailure {
    fn from(error: LedgerError) -> Self {
        let code = match &error {
            // 同一请求键对应不同内容——**这是它对应的那个明确失败**（§8.5）。
            LedgerError::RequestKeyConflict { .. } => ErrorCode::RequestKeyConflict,
            LedgerError::NotFound { .. } => ErrorCode::NotFound,
            LedgerError::Conflict(_) | LedgerError::StaleGeneration { .. } => ErrorCode::Conflict,
            LedgerError::Corrupt(_) => ErrorCode::Corrupt,
            LedgerError::Persist(_) | LedgerError::Contended => ErrorCode::Internal,
        };
        ApiFailure::new(code, error.to_string())
    }
}

impl From<StoreError> for ApiFailure {
    fn from(error: StoreError) -> Self {
        let code = match &error {
            StoreError::NotFound { .. } => ErrorCode::NotFound,
            StoreError::VersionConflict { .. } => ErrorCode::VersionConflict,
            StoreError::GrantMismatch(_) => ErrorCode::Denied,
            StoreError::Corrupt(_) => ErrorCode::Corrupt,
            StoreError::Io(_) | StoreError::Contended | StoreError::Other(_) => ErrorCode::Internal,
        };
        ApiFailure::new(code, error.to_string())
    }
}

impl From<RepoError> for ApiFailure {
    fn from(error: RepoError) -> Self {
        let code = match &error {
            RepoError::NotFound { .. } => ErrorCode::NotFound,
            RepoError::VersionConflict { .. } => ErrorCode::VersionConflict,
            RepoError::GrantMismatch(_) => ErrorCode::Denied,
            RepoError::Contended | RepoError::Other(_) => ErrorCode::Internal,
        };
        ApiFailure::new(code, error.to_string())
    }
}

impl From<GatewayError> for ApiFailure {
    fn from(error: GatewayError) -> Self {
        match error {
            GatewayError::Unauthorized => {
                ApiFailure::new(ErrorCode::Unauthorized, "未认证".to_string())
            }
            GatewayError::NotFound { what } => ApiFailure::not_found(what),
            GatewayError::InvalidRequest(message) => ApiFailure::invalid(message),
            GatewayError::Ledger(error) => error.into(),
            GatewayError::Store(error) => error.into(),
            GatewayError::Repo(error) => error.into(),
            GatewayError::Internal(message) => ApiFailure::new(ErrorCode::Internal, message),
        }
    }
}

/// 处理器的返回类型。
pub type ApiResult<T> = Result<T, ApiFailure>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_key_conflict_is_a_409_with_its_own_code() {
        let failure: ApiFailure = LedgerError::RequestKeyConflict {
            key: "telegram:42".into(),
        }
        .into();
        assert_eq!(failure.status, StatusCode::CONFLICT);
        assert_eq!(failure.code(), ErrorCode::RequestKeyConflict);
    }

    #[test]
    fn a_missing_thing_is_a_404() {
        let failure: ApiFailure = GatewayError::NotFound {
            what: "run x".into(),
        }
        .into();
        assert_eq!(failure.status, StatusCode::NOT_FOUND);
        assert_eq!(failure.code(), ErrorCode::NotFound);
    }

    #[test]
    fn an_invalid_config_is_a_422_that_names_keys_and_never_values() {
        let failure = ApiFailure::config_invalid(
            "校验不过",
            vec![KeyPath::new("channels.telegram.allow_from")],
        );
        assert_eq!(failure.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(failure.error.keys.len(), 1);
    }
}
