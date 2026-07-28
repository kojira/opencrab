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
    /// 転記を有効にするか。**省略時は現状維持**（`enabled` だけ / `webhook_url` だけの
    /// 部分更新を許す）。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 転記先 Discord チャンネルの webhook URL。**三状態**（段階 C の own ツールと同じ意味論）:
    /// - **フィールド不在**（`None`）→ 既存の転記先を**保持**（変更しない）。
    /// - **null / 空・空白**（`Some(None)` / `Some(Some(""))`）→ 転記先を**消去**。
    /// - **非空**（`Some(Some(url))`）→ 検証して**設定**。
    ///
    /// serde の double-option で「フィールド不在」と「明示 null」を区別する。素の
    /// `Option<Option<_>>` は `null` を outer `None` に潰してしまうため、フィールドが
    /// 存在したら必ず `Some(_)` を返す [`double_option`] を挟む。
    #[serde(default, deserialize_with = "double_option")]
    pub webhook_url: Option<Option<String>>,
}

/// フィールドが**存在**したら（値が `null` でも）`Some(_)` を返す。`#[serde(default)]` と
/// 併用することで「不在（`None`）」「明示 null（`Some(None)`）」「値あり（`Some(Some(_))`）」の
/// 3 状態を区別する。
fn double_option<'de, D>(de: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(de)?))
}

/// `webhook_url` 引数の 3 状態（省略 = 保持 / null・空 = 消去 / 文字列 = 設定）。
/// 段階 C の `UrlArg`（`crates/server/src/agent_nostr_relay.rs`）と同じ意味論。
enum UrlArg {
    Keep,
    Clear,
    Set(String),
}

/// PUT /api/agents/{id}/nostr-relay — 転記設定を**部分更新**する。
///
/// 「現在行を読む → 指定された分だけ差し替え → upsert」で行う。`enabled` だけを送れば
/// 転記先 URL は現状維持され（チェック切り替えで既存 webhook が無言で消えない）、
/// `webhook_url` を明示 null / 空で送ったときだけ消去する。非空の URL は
/// [`validate_webhook_url`]（Discord の webhook ホスト・`/api/webhooks/<id>/<token>` 形式のみ）
/// で検証し、**不正なら丸めず 400 で拒否**する。
///
/// 生 URL は取得できないので、更新後の応答は伏字化した `webhook_url_masked` だけを返す。
/// 結果状態が「有効かつ転記先が実質空」のとき、fail-closed で 1 件も飛ばないことに気づける
/// よう応答に `warning` を添える（段階 C と同一文言。正常時は付けない）。
pub async fn update_nostr_relay_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PutNostrRelayBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // webhook_url の三状態を決める（非空はここで検証し、不正なら保存前に 400）。
    let url_arg = match &body.webhook_url {
        None => UrlArg::Keep,
        Some(None) => UrlArg::Clear,
        Some(Some(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                UrlArg::Clear
            } else {
                // reason は raw url を含まない契約（生 URL は応答に載せない）。
                validate_webhook_url(trimmed).map_err(|reason| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("webhook_url が不正です: {reason}"),
                    )
                })?;
                UrlArg::Set(trimmed.to_string())
            }
        }
    };

    // 「現在行を読む → 指定分だけ差し替え → upsert」を同一ロック内で原子的に行う。
    let (new_enabled, new_url) = {
        let conn = state.db.lock().unwrap();
        let existing = opencrab_db::queries::get_agent_nostr_relay_config(&conn, &id)
            .ok()
            .flatten();
        // 行が無いときの土台は無効 / 未設定（fail-closed。段階 A と同じ既定）。
        let old_enabled = existing.as_ref().map(|r| r.enabled).unwrap_or(false);
        let old_url = existing.as_ref().and_then(|r| r.webhook_url.clone());

        let new_enabled = body.enabled.unwrap_or(old_enabled);
        let new_url = match url_arg {
            UrlArg::Keep => old_url,
            UrlArg::Clear => None,
            UrlArg::Set(u) => Some(u),
        };

        let row = AgentNostrRelayConfigRow {
            agent_id: id.clone(),
            enabled: new_enabled,
            webhook_url: new_url.clone(),
        };
        opencrab_db::queries::upsert_agent_nostr_relay_config(&conn, &row)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        (new_enabled, new_url)
    };

    // 空文字/空白のみの URL は「未設定」と同一視する（段階 A の fail-closed と揃える）。
    let effective_url = new_url.as_deref().map(str::trim).filter(|u| !u.is_empty());
    let has_webhook = effective_url.is_some();
    let masked = effective_url.map(redact_webhook_url).unwrap_or_default();

    let mut payload = json!({
        "updated": true,
        "enabled": new_enabled,
        "has_webhook": has_webhook,
        "webhook_url_masked": masked,
    });
    // foot-gun: 有効だが転記先が実質空 → 下流の resolve が fail-closed で 1 件も飛ばさない。
    // 拒否はせず（enabled と webhook_url を別リクエストで設定する順序を許す）、警告を添える。
    if new_enabled && !has_webhook {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "warning".to_string(),
                json!(
                    "有効化されていますが転記先(webhook_url)が未設定のため、現在は転記されません"
                ),
            );
        }
    }
    Ok(Json(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcdefTOKEN";

    /// テスト用 body。`enabled` / `webhook_url` とも 3 状態を素直に組めるヘルパ:
    /// - `enabled`: `None` = 省略（保持）/ `Some(b)` = 設定。
    /// - `webhook_url`: `None` = 省略（保持）/ `Some(None)` = null 消去 /
    ///   `Some(Some(s))` = 文字列（空なら消去）。
    fn body(enabled: Option<bool>, webhook_url: Option<Option<&str>>) -> PutNostrRelayBody {
        PutNostrRelayBody {
            enabled,
            webhook_url: webhook_url.map(|o| o.map(str::to_string)),
        }
    }

    async fn put(state: &AppState, id: &str, b: PutNostrRelayBody) -> serde_json::Value {
        update_nostr_relay_config(State(state.clone()), Path(id.to_string()), Json(b))
            .await
            .expect("update should succeed")
            .0
    }

    async fn get(state: &AppState, id: &str) -> serde_json::Value {
        get_nostr_relay_config(State(state.clone()), Path(id.to_string()))
            .await
            .0
    }

    /// serde の double-option が 3 状態（不在=保持 / null=消去 / 文字列=設定）に写ること。
    #[test]
    fn body_deserializes_three_states() {
        // フィールド不在 → 保持（outer None）。
        let keep: PutNostrRelayBody = serde_json::from_value(json!({ "enabled": true })).unwrap();
        assert_eq!(keep.enabled, Some(true));
        assert!(keep.webhook_url.is_none(), "不在は保持（None）であるべき");

        // 明示 null → 消去（Some(None)）。
        let clear: PutNostrRelayBody =
            serde_json::from_value(json!({ "webhook_url": null })).unwrap();
        assert_eq!(clear.webhook_url, Some(None));

        // 文字列 → 設定（Some(Some(..)))。
        let set: PutNostrRelayBody =
            serde_json::from_value(json!({ "webhook_url": VALID_URL })).unwrap();
        assert_eq!(set.webhook_url, Some(Some(VALID_URL.to_string())));

        // enabled 不在 → 保持（None）。
        let no_enabled: PutNostrRelayBody = serde_json::from_value(json!({})).unwrap();
        assert!(no_enabled.enabled.is_none());
    }

    /// 更新 → 取得のラウンドトリップ: 保存した enabled が読み出せる。
    #[tokio::test]
    async fn update_then_get_roundtrip() {
        let state = crate::test_app_state();
        put(&state, "a1", body(Some(true), Some(Some(VALID_URL)))).await;

        let got = get(&state, "a1").await;
        assert_eq!(got["configured"], json!(true));
        assert_eq!(got["enabled"], json!(true));
        assert_eq!(got["has_webhook"], json!(true));
    }

    /// GET は生 URL を返さない（トークンが伏字になっている）。
    #[tokio::test]
    async fn get_never_returns_raw_webhook_url() {
        let state = crate::test_app_state();
        put(&state, "a1", body(Some(true), Some(Some(VALID_URL)))).await;

        let got = get(&state, "a1").await;
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
        let res = put(&state, "a1", body(Some(true), Some(Some(VALID_URL)))).await;
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
            Json(body(
                Some(true),
                Some(Some("https://evil.example.com/api/webhooks/1/tok")),
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
        let got = get(&state, "a1").await;
        assert_eq!(got["configured"], json!(false));
    }

    /// **ブロッカー回帰**: webhook_url を省略した部分更新（enabled トグルだけ）では
    /// 既存 URL が保持され、無言で消えない。
    #[tokio::test]
    async fn omitting_webhook_url_keeps_existing_destination() {
        let state = crate::test_app_state();
        // 有効 + URL を設定。
        put(&state, "a1", body(Some(true), Some(Some(VALID_URL)))).await;

        // enabled だけ false に（webhook_url は省略 = 保持）。
        let res = put(&state, "a1", body(Some(false), None)).await;
        assert_eq!(res["enabled"], json!(false));
        assert_eq!(res["has_webhook"], json!(true), "URL が消えてはならない");

        let got = get(&state, "a1").await;
        assert_eq!(got["enabled"], json!(false));
        assert_eq!(got["has_webhook"], json!(true), "URL が保持されるべき");
        assert!(got["webhook_url_masked"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));

        // 再度 enabled だけ true に戻しても URL は残っている。
        let res = put(&state, "a1", body(Some(true), None)).await;
        assert_eq!(res["enabled"], json!(true));
        assert_eq!(res["has_webhook"], json!(true));
        assert!(
            res.get("warning").is_none(),
            "URL があるので warning は不要"
        );
    }

    /// 明示 null / 空文字は転記先を消去する（enabled は保持）。
    #[tokio::test]
    async fn explicit_null_or_empty_clears_destination() {
        let state = crate::test_app_state();
        put(&state, "a1", body(Some(true), Some(Some(VALID_URL)))).await;

        // 空白のみ → 消去（enabled は保持）。
        let res = put(&state, "a1", body(None, Some(Some("   ")))).await;
        assert_eq!(res["enabled"], json!(true), "enabled は保持されるべき");
        assert_eq!(res["has_webhook"], json!(false));
        assert_eq!(res["webhook_url_masked"], json!(""));

        // 再設定してから明示 null で消去。
        put(&state, "a1", body(None, Some(Some(VALID_URL)))).await;
        let res = put(&state, "a1", body(None, Some(None))).await;
        assert_eq!(res["has_webhook"], json!(false));
        let got = get(&state, "a1").await;
        assert_eq!(got["has_webhook"], json!(false));
    }

    /// **foot-gun 警告**: 有効化かつ転記先が実質空だと warning を付ける。転記先を
    /// 設定した正常な有効化・無効化には付けない（段階 C と同じ）。
    #[tokio::test]
    async fn enabling_without_target_warns_but_setting_target_does_not() {
        let state = crate::test_app_state();

        // 有効化だけ（転記先未設定）→ success かつ warning あり。
        let warn = put(&state, "a1", body(Some(true), None)).await;
        assert_eq!(warn["enabled"], json!(true));
        assert_eq!(warn["has_webhook"], json!(false));
        assert_eq!(
            warn["warning"],
            json!("有効化されていますが転記先(webhook_url)が未設定のため、現在は転記されません"),
            "有効化かつ転記先未設定なら warning が必要: {warn}"
        );

        // 有効化 + 有効な転記先 → warning は付かない。
        let ok = put(&state, "a1", body(Some(true), Some(Some(VALID_URL)))).await;
        assert_eq!(ok["has_webhook"], json!(true));
        assert!(
            ok.get("warning").is_none(),
            "正常な有効化には warning を付けない: {ok}"
        );

        // 無効化（転記先消去）でも warning は付かない（有効時だけの注意喚起）。
        let disabled = put(&state, "a1", body(Some(false), Some(None))).await;
        assert!(
            disabled.get("warning").is_none(),
            "無効化には warning を付けない: {disabled}"
        );
    }

    /// 未設定エージェントの GET は既定（無効・未設定）を返す。
    #[tokio::test]
    async fn unset_agent_returns_defaults() {
        let state = crate::test_app_state();
        let got = get(&state, "nobody").await;
        assert_eq!(got["configured"], json!(false));
        assert_eq!(got["enabled"], json!(false));
        assert_eq!(got["has_webhook"], json!(false));
    }
}
