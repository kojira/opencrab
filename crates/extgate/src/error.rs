//! V3 §5.4 の 25 code + DI 拡張 §10.4 の 4 code = 29 code。これ以外を追加しない。

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
    // DI 拡張 §10.4。いずれも generic code で platform 語彙を埋め込まない。
    OperationDeclarationInvalid,
    OperationDeclarationMismatch,
    OperationUnknown,
    OperationRejected,
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
            Self::OperationDeclarationInvalid => "operation_declaration_invalid",
            Self::OperationDeclarationMismatch => "operation_declaration_mismatch",
            Self::OperationUnknown => "operation_unknown",
            Self::OperationRejected => "operation_rejected",
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
            "operation_declaration_invalid" => Self::OperationDeclarationInvalid,
            "operation_declaration_mismatch" => Self::OperationDeclarationMismatch,
            "operation_unknown" => Self::OperationUnknown,
            "operation_rejected" => Self::OperationRejected,
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
            | Self::Disconnect
            | Self::OperationDeclarationInvalid
            | Self::OperationDeclarationMismatch
            | Self::OperationUnknown
            | Self::OperationRejected => return None,
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

    /// store 失敗を**サーバ側 ERROR ログ**に真因（rusqlite 等）付きで残し、`StoreError` を返す。
    /// 無音の store 失敗（fail-loud 違反・said が store_error で全滅しても core ログに何も出ない
    /// 事象）を塞ぐ。`detail` には SQL/path/token を載せない規約（上のコメント）なので、返す
    /// `detail` は失敗地点を示す**固定カテゴリ名 `context`** だけにする（真因は ERROR ログ側だけに出す）。
    pub fn store_logged(context: &'static str, cause: impl std::fmt::Display) -> Self {
        tracing::error!(context, cause = %cause, "extgate said-store failure");
        Self::with_detail(ErrorCode::StoreError, context)
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

#[cfg(test)]
mod tests {
    use super::*;

    // fail-loud: store 失敗は StoreError を返しつつ、detail に「失敗地点カテゴリ」を載せる
    // （真因の rusqlite エラーは ERROR ログ側だけ・SQL/path/token は detail に出さない）。
    #[test]
    fn store_logged_carries_category_detail_not_raw_cause() {
        let err = GateError::store_logged("said.session_log_insert", "UNIQUE constraint failed: x");
        assert_eq!(err.code, ErrorCode::StoreError);
        assert_eq!(err.detail.as_deref(), Some("said.session_log_insert"));
        // 生の rusqlite 文言（SQL 断片）は detail に漏らさない。
        let json = String::from_utf8(err.to_json_bytes()).unwrap();
        assert!(
            !json.contains("UNIQUE constraint"),
            "raw cause leaked: {json}"
        );
        assert!(json.contains("said.session_log_insert"), "{json}");
        assert!(json.contains("store_error"), "{json}");
    }

    // 素の store() は従来どおり detail=None（既存契約を壊さない）。
    #[test]
    fn plain_store_has_null_detail() {
        let err = GateError::store();
        assert_eq!(err.code, ErrorCode::StoreError);
        assert_eq!(err.detail, None);
    }
}
