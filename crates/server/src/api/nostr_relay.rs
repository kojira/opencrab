//! Nostr 受信 → Discord 転記先のエージェント単位設定 API（issue #252 段階 B）。
//!
//! エージェントが Nostr で受け取った**自分宛の受信**（メンション/リプライ/DM）を、
//! 運用者が指定した 1 つの Discord チャンネル webhook へ転記する設定を、ダッシュボードから
//! 読み書きする。段階 A（DB 表 `agent_nostr_relay_config` + `webhook_target` の
//! 検証/秘匿）の上に乗る薄い REST 層。
//!
//! 秘匿方針: webhook URL のトークンは秘密なので、GET 応答では **生 URL を返さず**
//! [`redact_webhook_url`] で末尾を伏字にした値だけを返す（段階 C の own ツールと同じ）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use opencrab_actions::{redact_webhook_url, validate_webhook_url};
use opencrab_db::queries::AgentNostrRelayConfigRow;

use crate::AppState;

/// GET /api/agents/{id}/nostr-relay — 現在の転記設定を返す。
///
/// `webhook_url` は伏字（トークン末尾をマスク）で返す。行が無ければ既定（無効・未設定）。
pub async fn get_nostr_relay_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let row = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_relay_config(&conn, &id).unwrap_or(None)
    };
    match row {
        Some(cfg) => {
            let url = cfg.webhook_url.unwrap_or_default();
            let has_webhook = !url.trim().is_empty();
            Json(json!({
                "configured": true,
                "enabled": cfg.enabled,
                "has_webhook": has_webhook,
                // 生 URL は返さない（トークンを伏字にした表示専用値だけを返す）。
                "webhook_url_masked": if has_webhook { redact_webhook_url(url.trim()) } else { String::new() },
            }))
        }
        None => Json(json!({
            "configured": false,
            "enabled": false,
            "has_webhook": false,
            "webhook_url_masked": "",
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct PutNostrRelayBody {
    /// 転記を有効にするか。
    #[serde(default)]
    pub enabled: bool,
    /// 転記先 Discord チャンネルの webhook URL。空 / null で転記先を消去する。
    #[serde(default)]
    pub webhook_url: Option<String>,
}

/// PUT /api/agents/{id}/nostr-relay — 転記設定を保存する。
///
/// `webhook_url` は空 / null なら転記先を消去（`None` で保存）。非空なら
/// [`validate_webhook_url`] で検証し、**不正なら丸めず 400 で拒否**する（Discord の
/// webhook ホスト・`/api/webhooks/<id>/<token>` 形式のみ許可）。設定は fail-closed のため、
/// 有効でも webhook が無ければ転記は起きない（段階 A の `resolve_nostr_relay_webhook`）。
pub async fn update_nostr_relay_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PutNostrRelayBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let trimmed = body.webhook_url.as_deref().map(str::trim).unwrap_or("");
    let webhook_url = if trimmed.is_empty() {
        None
    } else {
        // 不正な URL は保存前に 400 で弾く（生 URL は応答に載せない — reason に URL は含まれない契約）。
        validate_webhook_url(trimmed).map_err(|reason| {
            (
                StatusCode::BAD_REQUEST,
                format!("webhook_url が不正です: {reason}"),
            )
        })?;
        Some(trimmed.to_string())
    };

    let row = AgentNostrRelayConfigRow {
        agent_id: id.clone(),
        enabled: body.enabled,
        webhook_url,
    };
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_nostr_relay_config(&conn, &row)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let masked = row
        .webhook_url
        .as_deref()
        .map(redact_webhook_url)
        .unwrap_or_default();
    Ok(Json(json!({
        "updated": true,
        "enabled": row.enabled,
        "has_webhook": row.webhook_url.is_some(),
        "webhook_url_masked": masked,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcdefTOKEN";

    fn put_body(enabled: bool, webhook_url: Option<&str>) -> PutNostrRelayBody {
        PutNostrRelayBody {
            enabled,
            webhook_url: webhook_url.map(str::to_string),
        }
    }

    /// 更新 → 取得のラウンドトリップ: 保存した enabled が読み出せる。
    #[tokio::test]
    async fn update_then_get_roundtrip() {
        let state = crate::test_app_state();
        let _ = update_nostr_relay_config(
            State(state.clone()),
            Path("a1".to_string()),
            Json(put_body(true, Some(VALID_URL))),
        )
        .await
        .expect("update should succeed");

        let got = get_nostr_relay_config(State(state.clone()), Path("a1".to_string()))
            .await
            .0;
        assert_eq!(got["configured"], json!(true));
        assert_eq!(got["enabled"], json!(true));
        assert_eq!(got["has_webhook"], json!(true));
    }

    /// GET は生 URL を返さない（トークンが伏字になっている）。
    #[tokio::test]
    async fn get_never_returns_raw_webhook_url() {
        let state = crate::test_app_state();
        let _ = update_nostr_relay_config(
            State(state.clone()),
            Path("a1".to_string()),
            Json(put_body(true, Some(VALID_URL))),
        )
        .await
        .expect("update should succeed");

        let got = get_nostr_relay_config(State(state.clone()), Path("a1".to_string()))
            .await
            .0;
        let masked = got["webhook_url_masked"].as_str().unwrap();
        // トークン（末尾セグメント）が応答のどこにも出ない。
        assert!(
            !masked.contains("abcdefTOKEN"),
            "生トークンが伏字応答に漏れている: {masked}"
        );
        assert!(
            !got.to_string().contains("abcdefTOKEN"),
            "生トークンが GET 応答本文に漏れている"
        );
        assert!(masked.contains("[redacted]"), "伏字マーカが無い: {masked}");
    }

    /// 更新応答も生 URL を返さない（伏字のみ）。
    #[tokio::test]
    async fn update_response_never_returns_raw_webhook_url() {
        let state = crate::test_app_state();
        let res = update_nostr_relay_config(
            State(state.clone()),
            Path("a1".to_string()),
            Json(put_body(true, Some(VALID_URL))),
        )
        .await
        .expect("update should succeed")
        .0;
        assert!(
            !res.to_string().contains("abcdefTOKEN"),
            "生トークンが更新応答に漏れている"
        );
    }

    /// 不正な webhook URL は丸めず 400 で拒否し、DB に何も書かない。
    #[tokio::test]
    async fn invalid_webhook_url_is_rejected_with_400() {
        let state = crate::test_app_state();
        let err = update_nostr_relay_config(
            State(state.clone()),
            Path("a1".to_string()),
            Json(put_body(
                true,
                Some("https://evil.example.com/api/webhooks/1/tok"),
            )),
        )
        .await
        .expect_err("invalid url must be rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        // reason に生 URL（ホスト以降の秘密トークン）が載らない。
        assert!(
            !err.1.contains("tok"),
            "エラー文言に生トークンが載っている: {}",
            err.1
        );

        // 拒否時は行が作られない（fail-closed）。
        let got = get_nostr_relay_config(State(state.clone()), Path("a1".to_string()))
            .await
            .0;
        assert_eq!(got["configured"], json!(false));
    }

    /// 空文字 / null で保存すると転記先が消える（enabled は保持されるが webhook は None）。
    #[tokio::test]
    async fn empty_webhook_url_clears_destination() {
        let state = crate::test_app_state();
        // まず有効な URL を設定。
        let _ = update_nostr_relay_config(
            State(state.clone()),
            Path("a1".to_string()),
            Json(put_body(true, Some(VALID_URL))),
        )
        .await
        .expect("initial set");

        // 空文字で上書き → 消去。
        let _ = update_nostr_relay_config(
            State(state.clone()),
            Path("a1".to_string()),
            Json(put_body(true, Some("   "))),
        )
        .await
        .expect("clear with blank");
        let got = get_nostr_relay_config(State(state.clone()), Path("a1".to_string()))
            .await
            .0;
        assert_eq!(got["has_webhook"], json!(false));
        assert_eq!(got["webhook_url_masked"], json!(""));

        // null（フィールド省略相当）でも消去。
        let _ = update_nostr_relay_config(
            State(state.clone()),
            Path("a1".to_string()),
            Json(put_body(false, None)),
        )
        .await
        .expect("clear with null");
        let got = get_nostr_relay_config(State(state.clone()), Path("a1".to_string()))
            .await
            .0;
        assert_eq!(got["has_webhook"], json!(false));
        assert_eq!(got["enabled"], json!(false));
    }

    /// 未設定エージェントの GET は既定（無効・未設定）を返す。
    #[tokio::test]
    async fn unset_agent_returns_defaults() {
        let state = crate::test_app_state();
        let got = get_nostr_relay_config(State(state.clone()), Path("nobody".to_string()))
            .await
            .0;
        assert_eq!(got["configured"], json!(false));
        assert_eq!(got["enabled"], json!(false));
        assert_eq!(got["has_webhook"], json!(false));
    }
}
