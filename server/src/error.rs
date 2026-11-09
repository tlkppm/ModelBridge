//! Defines the server-wide error type and error conversions.
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderFailure {
    pub message: String,
    pub status: Option<u16>,
    pub error_type: Option<String>,
    pub code: Option<String>,
    pub param: Option<String>,
    pub retryable: bool,
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<String> for ProviderFailure {
    fn from(message: String) -> Self {
        Self::plain(message)
    }
}

impl From<&str> for ProviderFailure {
    fn from(message: &str) -> Self {
        Self::plain(message.to_owned())
    }
}

impl ProviderFailure {
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            error_type: None,
            code: None,
            param: None,
            retryable: true,
        }
    }

    pub fn http(label: &str, status: u16, body: &str) -> Self {
        let fields = error_fields(body);
        let mut failure = Self {
            message: format!("{label} {status}: {body}"),
            status: Some(status),
            error_type: fields.error_type,
            code: fields.code,
            param: fields.param,
            retryable: true,
        };
        failure.retryable = failure.allows_retry();
        failure
    }

    pub fn event(label: &str, message: impl Into<String>, value: &serde_json::Value) -> Self {
        let fields = error_fields_value(value);
        let mut failure = Self {
            message: format!("{label} error: {}", message.into()),
            status: None,
            error_type: fields.error_type,
            code: fields.code,
            param: fields.param,
            retryable: true,
        };
        failure.retryable = failure.allows_retry();
        failure
    }

    pub fn allows_retry(&self) -> bool {
        if self.code.as_deref() == Some("invalid_value")
            || self.error_type.as_deref() == Some("image_generation_user_error")
            || self.error_type.as_deref() == Some("invalid_request_error")
        {
            return false;
        }
        !matches!(self.status, Some(status) if (400..500).contains(&status) && !matches!(status, 408 | 425 | 429))
    }
}

struct ErrorFields {
    error_type: Option<String>,
    code: Option<String>,
    param: Option<String>,
}

fn error_fields(body: &str) -> ErrorFields {
    serde_json::from_str::<serde_json::Value>(body)
        .map(|value| error_fields_value(&value))
        .unwrap_or(ErrorFields {
            error_type: None,
            code: None,
            param: None,
        })
}

fn error_fields_value(value: &serde_json::Value) -> ErrorFields {
    let error = value
        .get("error")
        .filter(|error| error.is_object())
        .or_else(|| {
            value
                .pointer("/response/error")
                .filter(|error| error.is_object())
        })
        .unwrap_or(value);
    ErrorFields {
        error_type: text_field(error, "type").or_else(|| text_field(value, "error_type")),
        code: text_field(error, "code").or_else(|| text_field(value, "code")),
        param: text_field(error, "param").or_else(|| text_field(value, "param")),
    }
}

fn text_field(value: &serde_json::Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("provider error: {0}")]
    Provider(ProviderFailure),
    #[error("store error: {0}")]
    Store(String),
    #[error("run was cancelled")]
    Cancelled,
    #[error("run not found: {0}")]
    RunNotFound(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(
        "database migration stage '{stage}' timed out after {timeout_seconds} seconds; close other Cursor BYOK processes and try again"
    )]
    MigrationTimeout { stage: String, timeout_seconds: u64 },
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("protobuf decode error: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protobuf encode error: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn provider(message: String) -> Self {
        Self::Provider(ProviderFailure::plain(message))
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Config(_) | Self::Protocol(_) | Self::Decode(_) | Self::Json(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::RunNotFound(_) => StatusCode::NOT_FOUND,
            Self::Provider(_) | Self::Http(_) => StatusCode::BAD_GATEWAY,
            Self::Cancelled => StatusCode::CONFLICT,
            Self::Store(_)
            | Self::Database(_)
            | Self::Migration(_)
            | Self::MigrationTimeout { .. }
            | Self::Encode(_)
            | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // 所有回给 UI 的错误统一落日志,否则失败原因只出现在前端提示里。
        tracing::warn!(%status, error = %self, "request failed");
        let code = match status {
            StatusCode::BAD_REQUEST => "invalid_argument",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "aborted",
            StatusCode::BAD_GATEWAY => "unavailable",
            _ => "internal",
        };
        (
            status,
            Json(serde_json::json!({ "code": code, "message": self.to_string() })),
        )
            .into_response()
    }
}
