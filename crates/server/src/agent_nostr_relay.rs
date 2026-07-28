//! エージェントが**自分自身の** Nostr 受信 → Discord 転記先設定を読み書きする
//! ツール（issue #252 段階 C）。
//!
//! - `get_my_nostr_relay`: 現在の有効/無効と、転記先が設定済みか（伏字 URL）を返す。
//! - `set_my_nostr_relay`: 有効/無効と転記先 webhook URL を更新する。
//!
//! 永続化する行は段階 A（#253）の `agent_nostr_relay_config`
//! （`crates/db/src/queries/agent_nostr_relay_config.rs`）。宛先解決と実配送は
//! 段階 A が `handle_event` へ配線済みで、ここは**設定の入出力だけ**を担う。
//!
//! # 「自分のだけ」をどう保証しているか
//!
//! **引数に `agent_id` が無い。** 対象は常に `ctx.agent_id`（bridge が実行境界で組む
//! 呼び出し文脈で、ツール引数 JSON からは触れない）。さらに `agent_id`（および
//! 紛らわしい別名 `target_agent_id` / `agent`）が引数に現れたら**明示エラーで拒否**する。
//! 無視して黙って自分の設定を書き換えると、エージェントは「他のエージェントの設定を
//! 変えた」と思い込んだままになる。#247 / #251 の own ツールと同じ作法。
//!
//! # 権限
//!
//! **オーナー限定にしない**（自分の設定を自分で触れることがこの機能の目的）。ただし
//! 素の `Agent`（= 未信頼の外部ユーザー由来のターン）からは見えないよう
//! `TRUSTED_ONLY_ACTIONS` に入れる。エージェントが自分の意思で触るターン（ハートビート
//! tick / ダッシュボード / オーナーとの会話）は全て `Owner` なので妨げられない。逆に
//! ここを開けると、外部ユーザーが Nostr の会話ターンで「転記先をここに変えて」と
//! 言うだけで、自分宛受信を任意の Discord チャンネルへ流させられてしまう。ハンドラ内でも
//! 同じ検査をする（多層防御 / `heartbeat_instructions` と同じ流儀）。
//!
//! # 秘匿値の扱い
//!
//! `webhook_url` は秘匿値なので**生では返さない**。読み出し・更新後の応答はどちらも
//! `redact_webhook_url` で伏字化した `redacted_url` と、設定済みか否かの
//! `webhook_configured` だけを返す。監査ログにも生 URL は載せない（有効/無効と
//! 「URL 設定あり/なし」まで）。

use serde_json::json;

use opencrab_actions::webhook_target::{redact_webhook_url, validate_webhook_url};
use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::AppState;

fn err(msg: impl Into<String>) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg.into()),
    }
}

/// 呼び出し元権限の検査（多層防御）。bridge の `TRUSTED_ONLY_ACTIONS` と同じ範囲。
fn ensure_trusted(ctx: &GatewayCallContext) -> Option<GatewayActionResult> {
    if matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser
    ) {
        return None;
    }
    Some(err(
        "このアクションは信頼済みの呼び出し元のみ実行できます".to_string()
    ))
}

/// 他エージェントを指そうとする引数を拒否する。
///
/// このツールは `ctx.agent_id` しか見ないので、指定しても**効かない**。効かない指定を
/// 黙って捨てると「他のエージェントの設定を変えた」という誤解が残るため、明示的に落とす。
fn reject_foreign_target(args: &serde_json::Value) -> Option<GatewayActionResult> {
    for key in ["agent_id", "target_agent_id", "agent"] {
        if args.get(key).is_some() {
            return Some(err(format!(
                "{key}は指定できません（このツールは呼び出し元エージェント自身の設定だけを扱います）"
            )));
        }
    }
    None
}

/// 現在の設定を秘匿値を伏字化した 1 つの JSON にまとめる。読み出しも更新後も同じ形。
fn state_payload(conn: &rusqlite::Connection, agent_id: &str) -> serde_json::Value {
    let row = opencrab_db::queries::get_agent_nostr_relay_config(conn, agent_id)
        .ok()
        .flatten();
    let enabled = row.as_ref().map(|r| r.enabled).unwrap_or(false);
    // 空文字/空白のみの URL は「未設定」と同一視する（段階 A の fail-closed と揃える）。
    let url = row
        .and_then(|r| r.webhook_url)
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());
    json!({
        "agent_id": agent_id,
        "enabled": enabled,
        "webhook_configured": url.is_some(),
        // 生 URL は決して返さない。設定済みなら伏字（`.../[redacted]`）、未設定なら null。
        "redacted_url": url.as_deref().map(redact_webhook_url),
    })
}

/// 自分の Nostr 転記設定を読み出す。
pub(crate) fn get_my_nostr_relay(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if let Some(denied) = ensure_trusted(ctx) {
        return denied;
    }
    if let Some(denied) = reject_foreign_target(args) {
        return denied;
    }

    let conn = state.db.lock().unwrap();
    GatewayActionResult {
        success: true,
        data: Some(state_payload(&conn, &ctx.agent_id)),
        error: None,
    }
}

/// `webhook_url` 引数の 3 状態（省略 = 保持 / null・空 = 消去 / 文字列 = 設定）。
enum UrlArg {
    Keep,
    Clear,
    Set(String),
}

/// 自分の Nostr 転記設定を更新する。
///
/// URL は段階 A の `validate_webhook_url`（Discord webhook のホスト許可リスト）で検証し、
/// 不正なら**丸めずエラーで拒否**する。エラーには理由を載せるので、同じターンで正しい
/// URL に直して呼び直せる（このツールを inline 分類にしている理由でもある）。
pub(crate) fn set_my_nostr_relay(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if let Some(denied) = ensure_trusted(ctx) {
        return denied;
    }
    if let Some(denied) = reject_foreign_target(args) {
        return denied;
    }

    let enabled_arg = match args.get("enabled") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Bool(b)) => Some(*b),
        Some(_) => return err("enabledは真偽値で指定してください".to_string()),
    };

    // webhook_url は「省略（保持）」「明示 null / 空文字（消去）」「文字列（検証して設定）」
    // を区別する。生 URL の妥当性検証は段階 A の口を再利用する（許可リストを緩めない）。
    let url_arg = match args.get("webhook_url") {
        None => UrlArg::Keep,
        Some(serde_json::Value::Null) => UrlArg::Clear,
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                UrlArg::Clear
            } else if let Err(reason) = validate_webhook_url(trimmed) {
                // reason は raw url を含まない契約。
                return err(format!("webhook_urlが不正です: {reason}"));
            } else {
                UrlArg::Set(trimmed.to_string())
            }
        }
        Some(_) => return err("webhook_urlは文字列で指定してください".to_string()),
    };

    if enabled_arg.is_none() && matches!(url_arg, UrlArg::Keep) {
        return err("enabled か webhook_url のどちらかが必要です".to_string());
    }

    let conn = state.db.lock().unwrap();
    let existing = opencrab_db::queries::get_agent_nostr_relay_config(&conn, &ctx.agent_id)
        .ok()
        .flatten();
    // 行が無いときの土台は**無効 / 未設定**（fail-closed。設定を作っただけで転記が
    // 始まらない / 段階 A と同じ既定）。
    let old_enabled = existing.as_ref().map(|r| r.enabled).unwrap_or(false);
    let old_url = existing.as_ref().and_then(|r| r.webhook_url.clone());

    let new_enabled = enabled_arg.unwrap_or(old_enabled);
    let new_url = match url_arg {
        UrlArg::Keep => old_url.clone(),
        UrlArg::Clear => None,
        UrlArg::Set(u) => Some(u),
    };

    let row = opencrab_db::queries::AgentNostrRelayConfigRow {
        agent_id: ctx.agent_id.clone(),
        enabled: new_enabled,
        webhook_url: new_url.clone(),
    };
    if let Err(e) = opencrab_db::queries::upsert_agent_nostr_relay_config(&conn, &row) {
        return err(format!("Nostr 転記設定の保存に失敗: {e}"));
    }

    // 監査ログ。**生 URL は載せない**（有効/無効と「URL 設定あり/なし」まで）。
    tracing::info!(
        target: "nostr_relay_audit",
        agent_id = %ctx.agent_id,
        caller = %ctx.caller.label(),
        old_enabled = old_enabled,
        new_enabled = new_enabled,
        old_has_url = old_url.as_deref().map(|u| !u.trim().is_empty()).unwrap_or(false),
        new_has_url = new_url.as_deref().map(|u| !u.trim().is_empty()).unwrap_or(false),
        "エージェント自身が Nostr 転記設定を更新した"
    );

    let mut payload = state_payload(&conn, &ctx.agent_id);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("success".to_string(), json!(true));
    }
    GatewayActionResult {
        success: true,
        data: Some(payload),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_actions::SystemGatewayActions;
    use opencrab_gateway::GatewayActions;

    const WH_VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcSECRETtok";
    const WH_SECRET: &str = "abcSECRETtok";

    /// **transport 固有 gateway 無し**（`inner = None`）で合成 gateway を組む。
    /// これは web / REST / Nostr / heartbeat の経路そのもの。
    fn make_actions() -> (SystemGatewayActions, opencrab_db::Db) {
        let state = crate::test_app_state();
        let db = state.db.clone();
        (SystemGatewayActions::new(state, None, None, None), db)
    }

    fn ctx(caller: GatewayCaller) -> GatewayCallContext {
        GatewayCallContext::new(caller, "test-agent")
    }

    /// 応答 JSON に raw トークンが 1 度も現れないこと（秘匿処理の不変条件）。
    fn json_has_no_raw_token(v: &serde_json::Value) -> bool {
        !v.to_string().contains(WH_SECRET)
    }

    fn stored(db: &opencrab_db::Db) -> Option<opencrab_db::queries::AgentNostrRelayConfigRow> {
        let conn = db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_relay_config(&conn, "test-agent").unwrap()
    }

    /// own 定義に 1 件ずつ露出し、**対象エージェント ID プロパティを持たない**こと。
    /// これが生えると「他人を指す経路」ができるので、テストで固定する（#252 段階 C）。
    #[test]
    fn tools_are_own_only_in_definitions() {
        let (actions, _db) = make_actions();
        let defs = actions.definitions();
        for name in ["get_my_nostr_relay", "set_my_nostr_relay"] {
            assert_eq!(
                defs.iter().filter(|d| d.name == name).count(),
                1,
                "{name} は own 定義にちょうど 1 件必要（#252 段階 C）"
            );
            let props = defs
                .iter()
                .find(|d| d.name == name)
                .unwrap()
                .parameters
                .get("properties")
                .and_then(|p| p.as_object())
                .cloned()
                .unwrap_or_default();
            for forbidden in ["agent_id", "target_agent_id", "agent"] {
                assert!(
                    !props.contains_key(forbidden),
                    "{name} に {forbidden} を生やしてはならない（対象は常に呼び出し元自身）"
                );
            }
        }
        let set = defs
            .iter()
            .find(|d| d.name == "set_my_nostr_relay")
            .unwrap();
        let props = set.parameters["properties"].as_object().unwrap();
        for key in ["enabled", "webhook_url"] {
            assert!(props.contains_key(key), "missing property: {key}");
        }
    }

    /// **既定は無効 / 未設定**（fail-closed）。設定したことが無いエージェントはそう返る。
    #[tokio::test]
    async fn get_defaults_to_disabled_and_unconfigured() {
        let (actions, _db) = make_actions();
        let r = actions
            .execute("get_my_nostr_relay", &json!({}), &ctx(GatewayCaller::Owner))
            .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["enabled"], false);
        assert_eq!(data["webhook_configured"], false);
        assert_eq!(data["redacted_url"], serde_json::Value::Null);
    }

    /// set → get のラウンドトリップ。自分の設定が読め、応答に raw トークンが漏れない。
    #[tokio::test]
    async fn set_then_get_roundtrip_redacted() {
        let (actions, db) = make_actions();
        let set = actions
            .execute(
                "set_my_nostr_relay",
                &json!({ "enabled": true, "webhook_url": WH_VALID_URL }),
                &ctx(GatewayCaller::Owner),
            )
            .await;
        assert!(set.success, "{:?}", set.error);
        let set_data = set.data.unwrap();
        assert_eq!(set_data["success"], true);
        assert_eq!(set_data["enabled"], true);
        assert_eq!(set_data["webhook_configured"], true);
        assert!(
            json_has_no_raw_token(&set_data),
            "set 応答に raw トークンが漏れた"
        );
        assert!(set_data["redacted_url"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));

        // DB には生 URL が保存される（配送はこれを使う）。
        let row = stored(&db).unwrap();
        assert!(row.enabled);
        assert_eq!(row.webhook_url.as_deref(), Some(WH_VALID_URL));

        // get でも同じ伏字表現で読み戻せる。
        let get = actions
            .execute("get_my_nostr_relay", &json!({}), &ctx(GatewayCaller::Owner))
            .await;
        assert!(get.success);
        let get_data = get.data.unwrap();
        assert_eq!(get_data["enabled"], true);
        assert_eq!(get_data["webhook_configured"], true);
        assert!(
            json_has_no_raw_token(&get_data),
            "get 応答に raw トークンが漏れた"
        );
    }

    /// 不正な URL の set は**拒否**され、1 行も保存されない（丸めない）。
    #[tokio::test]
    async fn set_rejects_invalid_url_without_writing() {
        let (actions, db) = make_actions();
        for bad in [
            "https://evil.example.com/api/webhooks/1/tok",
            "http://discord.com/api/webhooks/1/tok",
            "https://discord.com/not-a-webhook",
        ] {
            let r = actions
                .execute(
                    "set_my_nostr_relay",
                    &json!({ "enabled": true, "webhook_url": bad }),
                    &ctx(GatewayCaller::Owner),
                )
                .await;
            assert!(!r.success, "{bad} が通ってしまう");
            assert!(
                r.error.unwrap().contains("webhook_urlが不正です"),
                "{bad}: 文言が想定と違う"
            );
        }
        assert!(stored(&db).is_none(), "検証に落ちた設定が保存されている");
    }

    /// **own-only の固定**: `agent_id` 等を渡すと両ツールとも明示エラーで拒否され、
    /// 自分の設定も書き換わらない。対象は常に `ctx.agent_id`。
    #[tokio::test]
    async fn foreign_target_keys_are_rejected() {
        let (actions, db) = make_actions();
        for key in ["agent_id", "target_agent_id", "agent"] {
            // get
            let g = actions
                .execute(
                    "get_my_nostr_relay",
                    &json!({ key: "victim" }),
                    &ctx(GatewayCaller::Owner),
                )
                .await;
            assert!(!g.success, "get: {key} が拒否されていない");
            assert!(g.error.unwrap().contains(key));

            // set（拒否されるので保存も起きない）
            let s = actions
                .execute(
                    "set_my_nostr_relay",
                    &json!({ key: "victim", "enabled": true, "webhook_url": WH_VALID_URL }),
                    &ctx(GatewayCaller::Owner),
                )
                .await;
            assert!(!s.success, "set: {key} が拒否されていない");
            assert!(s.error.unwrap().contains(key));
        }
        assert!(
            stored(&db).is_none(),
            "foreign target を拒否したのに自分の行が作られている"
        );
    }

    /// 権限ゲート（多層防御のハンドラ側）: 素の `Agent`（未信頼）は不可、`Owner`
    /// （heartbeat / ダッシュボード / オーナー会話）は可。
    #[tokio::test]
    async fn permission_agent_denied_owner_allowed() {
        let (actions, _db) = make_actions();
        for name in ["get_my_nostr_relay", "set_my_nostr_relay"] {
            let denied = actions
                .execute(
                    name,
                    &json!({ "enabled": true, "webhook_url": WH_VALID_URL }),
                    &ctx(GatewayCaller::Agent),
                )
                .await;
            assert!(!denied.success, "{name}: caller=Agent が実行できてしまう");
            assert!(denied.error.unwrap().contains("信頼済みの呼び出し元のみ"));
        }
        // Owner は set/get とも成功する。
        assert!(
            actions
                .execute(
                    "set_my_nostr_relay",
                    &json!({ "enabled": true, "webhook_url": WH_VALID_URL }),
                    &ctx(GatewayCaller::Owner),
                )
                .await
                .success
        );
        assert!(
            actions
                .execute("get_my_nostr_relay", &json!({}), &ctx(GatewayCaller::Owner))
                .await
                .success
        );
    }

    /// 権限ゲートは bridge の集合でも固定する（可視性 == 実行強制 / #45）。
    #[test]
    fn tools_are_trusted_only_and_not_owner_only() {
        for name in ["get_my_nostr_relay", "set_my_nostr_relay"] {
            assert!(
                opencrab_actions::TRUSTED_ONLY_ACTIONS.contains(&name),
                "{name} は TRUSTED_ONLY_ACTIONS に必要（未信頼 Agent から塞ぐ）"
            );
            assert!(
                !opencrab_actions::OWNER_ONLY_ACTIONS.contains(&name),
                "{name} を owner 限定にしてはならない（自己設定が目的）"
            );
        }
    }

    /// 引数がどちらも無ければ**エラー**（誤って空更新で確定させない）。
    #[tokio::test]
    async fn set_requires_at_least_one_arg() {
        let (actions, _db) = make_actions();
        let r = actions
            .execute("set_my_nostr_relay", &json!({}), &ctx(GatewayCaller::Owner))
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("enabled か webhook_url"));
    }

    /// 空文字 / null で転記先を消去できる（enabled は保持）。
    #[tokio::test]
    async fn empty_url_clears_target() {
        let (actions, db) = make_actions();
        actions
            .execute(
                "set_my_nostr_relay",
                &json!({ "enabled": true, "webhook_url": WH_VALID_URL }),
                &ctx(GatewayCaller::Owner),
            )
            .await;
        let r = actions
            .execute(
                "set_my_nostr_relay",
                &json!({ "webhook_url": "" }),
                &ctx(GatewayCaller::Owner),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        // enabled は保持され、転記先だけ消える。
        assert_eq!(data["enabled"], true);
        assert_eq!(data["webhook_configured"], false);
        let row = stored(&db).unwrap();
        assert!(row.enabled);
        assert!(row.webhook_url.as_deref().unwrap_or("").trim().is_empty());
    }
}
