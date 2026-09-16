//! 客户端错误：网络、解码，以及 Gateway 回的统一错误体（§13.1）。

use komo_kernel::protocol::config::KeyPath;
use komo_kernel::protocol::http::{ApiError, ErrorBody, ErrorCode};

pub type ClientResult<T> = Result<T, ClientError>;

/// 一次调用为什么没有拿到结果。
///
/// [`ClientError::Api`] 是**服务端明确说了话**的那一类：状态码加一份 [`ErrorBody`]，
/// 于是 [`ClientError::code`] 能答出 [`ErrorCode`]。服务端答了非 2xx 却不是那个形状，
/// 落到 [`ClientError::Http`]——把它伪造成某个 `ErrorCode` 会让调用方按一个没人说过的
/// 理由分支。
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 统一错误体。
    #[error("{status} {}：{}", .error.code_str(), .error.message)]
    Api { status: u16, error: ApiError },
    /// 非 2xx，但响应体不是 [`ErrorBody`]。
    #[error("HTTP {status}：{body}")]
    Http { status: u16, body: String },
    /// 连不上、超时、连接中断。
    #[error("传输失败：{0}")]
    Transport(String),
    /// 2xx 但响应体解不出目标类型。
    #[error("响应解析失败：{0}")]
    Decode(String),
    /// 请求 URL 拼不出来（base_url 不合法）。
    #[error("地址不合法：{0}")]
    BadUrl(String),
}

impl ClientError {
    /// 服务端说的那个错误码。只有服务端真的说了才有值。
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            ClientError::Api { error, .. } => Some(error.code),
            _ => None,
        }
    }

    /// 与这次错误相关的配置键（`config check` 用它定位到键）。
    pub fn keys(&self) -> &[KeyPath] {
        match self {
            ClientError::Api { error, .. } => &error.keys,
            _ => &[],
        }
    }

    pub fn is(&self, code: ErrorCode) -> bool {
        self.code() == Some(code)
    }

    /// 从一个非 2xx 响应体构造：能读成 [`ErrorBody`] 就带上码，否则保留原文。
    pub fn from_body(status: u16, body: &str) -> ClientError {
        match serde_json::from_str::<ErrorBody>(body) {
            Ok(parsed) => ClientError::Api {
                status,
                error: parsed.error,
            },
            Err(_) => ClientError::Http {
                status,
                body: body.trim().to_string(),
            },
        }
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_decode() {
            ClientError::Decode(error.to_string())
        } else {
            ClientError::Transport(error.to_string())
        }
    }
}

/// `ErrorCode` 没有 `Display`，而错误信息要能印出它。
trait CodeStr {
    fn code_str(&self) -> &'static str;
}

impl CodeStr for ApiError {
    fn code_str(&self) -> &'static str {
        code_str(self.code)
    }
}

/// 线格式上的名字，和 `#[serde(rename_all = "snake_case")]` 一致。
pub fn code_str(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::NotFound => "not_found",
        ErrorCode::Unauthorized => "unauthorized",
        ErrorCode::RequestKeyConflict => "request_key_conflict",
        ErrorCode::VersionConflict => "version_conflict",
        ErrorCode::InvalidRequest => "invalid_request",
        ErrorCode::Denied => "denied",
        ErrorCode::Conflict => "conflict",
        ErrorCode::ConfigInvalid => "config_invalid",
        ErrorCode::VectorUnavailable => "vector_unavailable",
        ErrorCode::Corrupt => "corrupt",
        ErrorCode::Internal => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_error_body_keeps_its_code() {
        let error = ClientError::from_body(
            409,
            r#"{"error":{"code":"request_key_conflict","message":"同一请求键对应不同内容"}}"#,
        );
        assert_eq!(error.code(), Some(ErrorCode::RequestKeyConflict));
        assert!(error.is(ErrorCode::RequestKeyConflict));
    }

    #[test]
    fn a_body_that_is_not_the_error_shape_does_not_invent_a_code() {
        let error = ClientError::from_body(502, "<html>bad gateway</html>");
        assert_eq!(error.code(), None);
        assert!(matches!(error, ClientError::Http { status: 502, .. }));
    }

    #[test]
    fn config_errors_carry_the_keys_they_are_about() {
        let error = ClientError::from_body(
            400,
            r#"{"error":{"code":"config_invalid","message":"模型没有配置","keys":["model.base_url"]}}"#,
        );
        assert_eq!(error.keys().len(), 1);
        assert_eq!(error.keys()[0].as_str(), "model.base_url");
    }

    #[test]
    fn every_code_round_trips_through_its_wire_name() {
        for code in [
            ErrorCode::NotFound,
            ErrorCode::Unauthorized,
            ErrorCode::RequestKeyConflict,
            ErrorCode::VersionConflict,
            ErrorCode::InvalidRequest,
            ErrorCode::Denied,
            ErrorCode::Conflict,
            ErrorCode::ConfigInvalid,
            ErrorCode::VectorUnavailable,
            ErrorCode::Corrupt,
            ErrorCode::Internal,
        ] {
            let body = format!(
                r#"{{"error":{{"code":"{}","message":"x"}}}}"#,
                code_str(code)
            );
            assert_eq!(ClientError::from_body(400, &body).code(), Some(code));
        }
    }
}
