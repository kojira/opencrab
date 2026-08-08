//! 外部イベント受信 webhook（`POST /api/hooks/{source}` / issue #454）。
//!
//! source ごとの共有 secret で HMAC-SHA256 を検証し、受理したイベントを `agent_inbox` へ
//! 積む（**処理はしない** — 消化は `intake_process` ループ）。
//!
//! ステータス契約:
//! - secret 未設定の source → **404**（存在を秘匿）
//! - 署名ヘッダ欠落 / 署名不正 → **401**（inbox 汚染なし）
//! - body が壊れている → **400**
//! - 受理 → **202**（ルート無しで積まれなくても受理は返す）
//!
//! secret / 署名値はログにもエラー応答にも出さない。

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderName, StatusCode},
};
use serde::Deserialize;

use crate::intake::{self, EnqueueOutcome, IntakeEvent};
use crate::AppState;

/// webhook body: `{"type": "<event type>", "data": {...}, "delivered_at": "..."}`。
#[derive(Deserialize)]
struct HookBody {
    r#type: String,
    #[serde(default)]
    data: serde_json::Value,
    /// 配送時刻。今は payload_json（raw body 全体）に含めて保存するだけで個別には使わない。
    #[serde(default)]
    #[allow(dead_code)]
    delivered_at: Option<String>,
}

/// `POST /api/hooks/{source}` ハンドラ。
///
/// `Bytes`（生 body）は署名検証の対象なので**最後の抽出子**に置く（axum の規約）。
pub async fn receive_hook(
    State(state): State<AppState>,
    Path(source): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    // 1. source の共有 secret。未設定（または空）なら存在を秘匿して 404。
    let Some(secret) = state.intake.secret_for(&source).map(str::to_owned) else {
        return StatusCode::NOT_FOUND;
    };

    // 2. 署名ヘッダ（source 固有 or 汎用）。無ければ 401。
    let Some(signature) = signature_header(&headers, &source) else {
        return StatusCode::UNAUTHORIZED;
    };

    // 3. HMAC-SHA256 を定数時間検証。失敗なら 401（受信箱に何も積まない）。
    if !intake::verify_signature(&secret, &body, &signature) {
        return StatusCode::UNAUTHORIZED;
    }

    // 4. body をパース。type 必須。壊れていれば 400。
    let Ok(parsed) = serde_json::from_slice::<HookBody>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    if parsed.r#type.trim().is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    // 5. dedup_key を作り、(source, event_type) をルーティングして積む。
    //    payload_json は署名対象そのもの（raw body）を保存する。
    let dedup_key = intake::webhook_dedup_key(&parsed.r#type, &parsed.data, &body);
    let event = IntakeEvent {
        event_type: parsed.r#type,
        dedup_key,
        payload_json: String::from_utf8_lossy(&body).into_owned(),
    };
    match intake::route_and_enqueue(&state, &source, &event) {
        Ok(EnqueueOutcome::Enqueued) => {
            tracing::debug!(source, event_type = %event.event_type, "intake: 受信箱に積んだ");
            StatusCode::ACCEPTED
        }
        Ok(EnqueueOutcome::Duplicate) => {
            tracing::debug!(source, event_type = %event.event_type, "intake: dedup（既存）");
            StatusCode::ACCEPTED
        }
        Ok(EnqueueOutcome::NoRoute) => {
            // 受理はするが配送先が無いので積まれない。設定漏れに気づけるよう 1 行残す。
            tracing::debug!(
                source,
                event_type = %event.event_type,
                "intake: ルート未設定のため受理のみ（積まない）"
            );
            StatusCode::ACCEPTED
        }
        Err(e) => {
            tracing::error!(source, error = %e, "intake: enqueue に失敗");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// 署名ヘッダの値を取り出す。`X-{Source}-Signature`（例 `X-Omoikane-Signature`）を優先し、
/// 無ければ汎用の `X-Hook-Signature`。ヘッダ名は HTTP 仕様で大文字小文字を区別しない。
fn signature_header(headers: &HeaderMap, source: &str) -> Option<String> {
    let specific = format!("x-{}-signature", source.to_lowercase());
    if let Ok(name) = HeaderName::from_bytes(specific.as_bytes()) {
        if let Some(v) = headers.get(&name).and_then(|v| v.to_str().ok()) {
            if !v.trim().is_empty() {
                return Some(v.to_string());
            }
        }
    }
    headers
        .get("x-hook-signature")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::config::{IntakeConfig, IntakeRoute};

    const SECRET: &str = "s3cr3t";

    fn state_with_route() -> AppState {
        let mut state = crate::test_app_state();
        let mut secrets = HashMap::new();
        secrets.insert("omoikane".to_string(), SECRET.to_string());
        state.intake = Arc::new(IntakeConfig {
            secrets,
            routes: vec![IntakeRoute {
                source: "omoikane".to_string(),
                event_type: "comment.created".to_string(),
                agent_id: "scout".to_string(),
            }],
            ..IntakeConfig::default()
        });
        state
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let hex: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256={hex}")
    }

    fn headers_with(sig: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(s) = sig {
            h.insert("x-omoikane-signature", HeaderValue::from_str(s).unwrap());
        }
        h
    }

    fn unprocessed(state: &AppState, agent_id: &str) -> i64 {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::count_unprocessed_inbox(&conn, agent_id).unwrap()
    }

    #[tokio::test]
    async fn unknown_source_returns_404() {
        let state = state_with_route();
        let body = Bytes::from_static(b"{\"type\":\"comment.created\",\"data\":{\"id\":1}}");
        let sig = sign(SECRET, &body);
        let code = receive_hook(
            State(state.clone()),
            Path("unknown".to_string()),
            headers_with(Some(&sig)),
            body,
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn missing_or_bad_signature_returns_401_without_polluting_inbox() {
        let state = state_with_route();
        let body = Bytes::from_static(b"{\"type\":\"comment.created\",\"data\":{\"id\":1}}");

        // 署名ヘッダ欠落 → 401。
        let code = receive_hook(
            State(state.clone()),
            Path("omoikane".to_string()),
            headers_with(None),
            body.clone(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);

        // 不正署名 → 401。
        let code = receive_hook(
            State(state.clone()),
            Path("omoikane".to_string()),
            headers_with(Some("sha256=deadbeef")),
            body.clone(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);

        // 別 secret で署名 → 401。
        let code = receive_hook(
            State(state.clone()),
            Path("omoikane".to_string()),
            headers_with(Some(&sign("wrong", &body))),
            body,
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);

        // inbox は汚染されない。
        assert_eq!(unprocessed(&state, "scout"), 0);
    }

    #[tokio::test]
    async fn valid_signature_enqueues_and_returns_202() {
        let state = state_with_route();
        let body = Bytes::from_static(b"{\"type\":\"comment.created\",\"data\":{\"id\":42}}");
        let sig = sign(SECRET, &body);
        let code = receive_hook(
            State(state.clone()),
            Path("omoikane".to_string()),
            headers_with(Some(&sig)),
            body.clone(),
        )
        .await;
        assert_eq!(code, StatusCode::ACCEPTED);
        assert_eq!(unprocessed(&state, "scout"), 1);

        // 同じイベントの再送は dedup で積み増さない（202 のまま）。
        let code = receive_hook(
            State(state.clone()),
            Path("omoikane".to_string()),
            headers_with(Some(&sig)),
            body,
        )
        .await;
        assert_eq!(code, StatusCode::ACCEPTED);
        assert_eq!(unprocessed(&state, "scout"), 1);
    }

    #[tokio::test]
    async fn valid_signature_no_route_is_accepted_but_not_enqueued() {
        let state = state_with_route();
        // route の無い event_type。
        let body = Bytes::from_static(b"{\"type\":\"chat.message\",\"data\":{\"id\":9}}");
        let sig = sign(SECRET, &body);
        let code = receive_hook(
            State(state.clone()),
            Path("omoikane".to_string()),
            headers_with(Some(&sig)),
            body,
        )
        .await;
        assert_eq!(code, StatusCode::ACCEPTED);
        assert_eq!(unprocessed(&state, "scout"), 0);
    }

    #[tokio::test]
    async fn valid_signature_but_broken_json_returns_400() {
        let state = state_with_route();
        let body = Bytes::from_static(b"not json");
        let sig = sign(SECRET, &body);
        let code = receive_hook(
            State(state.clone()),
            Path("omoikane".to_string()),
            headers_with(Some(&sig)),
            body,
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(unprocessed(&state, "scout"), 0);
    }
}
