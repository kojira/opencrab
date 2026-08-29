//! V3 §5.4 の 25 code。これ以外を追加しない。

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// 401 の body。byte-exact。
pub const UNAUTHORIZED_BODY: &[u8] = br#"{"error":{"code":"unauthorized","detail":null}}"#;

/// Content-Type for admin JSON。
pub const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";

/// V3 の stable error 語彙。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Unauthorized,
    BadRequest,
    SubjectUnknown,
    InstanceUnknown,
    BindingUnknown,
    InstanceConflict,
    RevisionConflict,
    InstanceDisabled,
    BindingClosed,
    BindingConflict,
    AddressInUse,
    InstanceActive,
    InstanceNotReady,
    StoreError,
    TooLarge,
    ProtocolOrder,
    ProtocolUnsupported,
    RevisionMismatch,
    ConfigDigestMismatch,
    ResponseInvalid,
    UnknownMessage,
    BindFailed,
    NotConnected,
    ExternalRejected,
    Disconnect,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::BadRequest => "bad_request",
            Self::SubjectUnknown => "subject_unknown",
            Self::InstanceUnknown => "instance_unknown",
            Self::BindingUnknown => "binding_unknown",
            Self::InstanceConflict => "instance_conflict",
            Self::RevisionConflict => "revision_conflict",
            Self::InstanceDisabled => "instance_disabled",
            Self::BindingClosed => "binding_closed",
            Self::BindingConflict => "binding_conflict",
            Self::AddressInUse => "address_in_use",
            Self::InstanceActive => "instance_active",
            Self::InstanceNotReady => "instance_not_ready",
            Self::StoreError => "store_error",
            Self::TooLarge => "too_large",
            Self::ProtocolOrder => "protocol_order",
            Self::ProtocolUnsupported => "protocol_unsupported",
            Self::RevisionMismatch => "revision_mismatch",
            Self::ConfigDigestMismatch => "config_digest_mismatch",
            Self::ResponseInvalid => "response_invalid",
            Self::UnknownMessage => "unknown_message",
            Self::BindFailed => "bind_failed",
            Self::NotConnected => "not_connected",
            Self::ExternalRejected => "external_rejected",
            Self::Disconnect => "disconnect",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "unauthorized" => Self::Unauthorized,
            "bad_request" => Self::BadRequest,
            "subject_unknown" => Self::SubjectUnknown,
            "instance_unknown" => Self::InstanceUnknown,
            "binding_unknown" => Self::BindingUnknown,
            "instance_conflict" => Self::InstanceConflict,
            "revision_conflict" => Self::RevisionConflict,
            "instance_disabled" => Self::InstanceDisabled,
            "binding_closed" => Self::BindingClosed,
            "binding_conflict" => Self::BindingConflict,
            "address_in_use" => Self::AddressInUse,
            "instance_active" => Self::InstanceActive,
            "instance_not_ready" => Self::InstanceNotReady,
            "store_error" => Self::StoreError,
            "too_large" => Self::TooLarge,
            "protocol_order" => Self::ProtocolOrder,
            "protocol_unsupported" => Self::ProtocolUnsupported,
            "revision_mismatch" => Self::RevisionMismatch,
            "config_digest_mismatch" => Self::ConfigDigestMismatch,
            "response_invalid" => Self::ResponseInvalid,
            "unknown_message" => Self::UnknownMessage,
            "bind_failed" => Self::BindFailed,
            "not_connected" => Self::NotConnected,
            "external_rejected" => Self::ExternalRejected,
            "disconnect" => Self::Disconnect,
            _ => return None,
        })
    }

    pub const fn http_status(self) -> Option<StatusCode> {
        Some(match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::SubjectUnknown | Self::InstanceUnknown | Self::BindingUnknown => {
                StatusCode::NOT_FOUND
            }
            Self::InstanceConflict
            | Self::RevisionConflict
            | Self::InstanceDisabled
            | Self::BindingClosed
            | Self::BindingConflict
            | Self::AddressInUse
            | Self::InstanceActive
            | Self::InstanceNotReady => StatusCode::CONFLICT,
            Self::StoreError => StatusCode::INTERNAL_SERVER_ERROR,
            Self::TooLarge
            | Self::ProtocolOrder
            | Self::ProtocolUnsupported
            | Self::RevisionMismatch
            | Self::ConfigDigestMismatch
            | Self::ResponseInvalid
            | Self::UnknownMessage
            | Self::BindFailed
            | Self::NotConnected
            | Self::ExternalRejected
            | Self::Disconnect => return None,
        })
    }
}

/// admin / 内部の失敗。detail に path・SQL・token を入れない。
#[derive(Debug)]
pub struct GateError {
    pub code: ErrorCode,
    pub detail: Option<String>,
}

impl GateError {
    pub fn new(code: ErrorCode) -> Self {
        Self { code, detail: None }
    }

    pub fn with_detail(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: Some(detail.into()),
        }
    }

    pub fn store() -> Self {
        Self::new(ErrorCode::StoreError)
    }

    pub fn to_json_bytes(&self) -> Vec<u8> {
        if self.code == ErrorCode::Unauthorized {
            return UNAUTHORIZED_BODY.to_vec();
        }
        let detail = match &self.detail {
            Some(d) => serde_json::Value::String(d.clone()),
            None => serde_json::Value::Null,
        };
        serde_json::json!({
            "error": {
                "code": self.code.as_str(),
                "detail": detail,
            }
        })
        .to_string()
        .into_bytes()
    }
}

impl IntoResponse for GateError {
    fn into_response(self) -> Response {
        let status = self
            .code
            .http_status()
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = self.to_json_bytes();
        let mut res = Response::new(axum::body::Body::from(body));
        *res.status_mut() = status;
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(JSON_CONTENT_TYPE),
        );
        res
    }
}
