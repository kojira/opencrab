//! per-agent Nostr sub-gateway の設定 API。
//!
//! Discord の per-agent 設定 API と同型: DB に設定を保存し、マネージャで
//! 起動/停止する。秘密鍵は応答でマスクする（平文を返さない）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use opencrab_actions::gateway_kinds;
use opencrab_db::queries::AgentNostrConfigRow;
use opencrab_nostr::{config_from_row, validate_vanity_prefix};

use crate::AppState;

/// nsec は秘密鍵なので**完全マスク**（末尾も見せない）。設定有無は has_secret_key で示す。
fn mask_secret(key: &str) -> String {
    if key.is_empty() {
        String::new()
    } else {
        "••••••••".to_string()
    }
}

/// GET /api/agents/{id}/nostr — 設定を返す（秘密鍵はマスク）。
pub async fn get_nostr_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let row = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_config(&conn, &id).unwrap_or(None)
    };
    // 未登録（マネージャ未生成）は false。
    let running = state.gateways.is_running(gateway_kinds::NOSTR, &id);

    // オーナー識別子は公開情報（pubkey）なのでマスクしない。未設定は空文字。
    let owner_pubkey = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_owner_pubkey(&conn, &id).unwrap_or_default()
    };

    match row {
        Some(cfg) => {
            let parsed = config_from_row(&cfg);
            Json(json!({
                "configured": true,
                "enabled": cfg.enabled,
                "running": running,
                "has_secret_key": !cfg.secret_key.is_empty(),
                "secret_key_masked": mask_secret(&cfg.secret_key),
                "owner_pubkey": owner_pubkey,
                "relays": parsed.effective_relays(),
                "filter": {
                    "authors": parsed.filter.authors,
                    "keywords": parsed.filter.keywords,
                    "kinds": parsed.filter.kinds,
                },
            }))
        }
        None => Json(json!({
            "configured": false,
            "enabled": false,
            "running": running,
            "has_secret_key": false,
            "secret_key_masked": "",
            "owner_pubkey": "",
            "relays": opencrab_nostr::DEFAULT_RELAYS,
            "filter": {"authors": [], "keywords": [], "kinds": []},
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct PutNostrBody {
    /// nsec。空/未指定なら既存を保持（更新でクリアしない）。
    #[serde(default)]
    pub secret_key: Option<String>,
    #[serde(default)]
    pub relays: Vec<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<u32>,
    /// 有効化して即起動するか。
    #[serde(default)]
    pub enabled: bool,
    /// Nostr 経路のオーナー識別子（npub / hex）。未指定なら現状維持、`""` で未設定に戻す。
    ///
    /// `owner_discord_id` を per-agent Discord 設定 API で設定できるのと同じ位置づけ
    /// （#319）。ダッシュボード / REST からも設定できる。
    #[serde(default)]
    pub owner_pubkey: Option<String>,
}

/// PUT /api/agents/{id}/nostr — 設定を保存し、enabled なら起動する。
pub async fn update_nostr_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PutNostrBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    apply_nostr_settings(
        &state,
        &id,
        &body.relays,
        &body.authors,
        &body.keywords,
        &body.kinds,
        body.enabled,
        body.secret_key.as_deref(),
        body.owner_pubkey.as_deref(),
    )
    .await?;
    Ok(Json(json!({"updated": true, "enabled": body.enabled})))
}

/// オーナー識別子の入力を保存形（64 桁小文字 hex）へ正規化する。
///
/// - `None`（未指定）→ `None`（現状維持）
/// - 空文字 → `Some("")`（未設定へ戻す）
/// - npub / hex → `Some(hex)`
/// - それ以外 → 400
///
/// **黙って落とさない**のが要点。正規化できない値をそのまま保存すると、設定できた
/// ように見えて永久に誰とも一致しない行になる（＝オーナーが居ないことに気づけない）。
pub(crate) fn normalize_owner_pubkey_input(
    raw: Option<&str>,
) -> Result<Option<String>, (StatusCode, String)> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Some(String::new()));
    }
    opencrab_nostr::normalize_pubkey(trimmed)
        .map(Some)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "owner_pubkey は npub1... か 64 桁の hex で指定してください".to_string(),
            )
        })
}

/// Nostr 設定を保存し、マネージャに反映（enabled なら起動、else 停止）する共通処理。
/// REST（`update_nostr_config`）と エージェントツール（`configure_nostr`）の両方が使う。
///
/// `secret_key_override` が空でなければそれを、無ければ既存の秘密鍵を保持する
/// （更新で誤って鍵をクリアしない）。既存も無ければ 400（先に鍵生成が必要）。
/// 起動失敗時に「enabled だが未稼働」の不整合を残さないよう、まず enabled=false で
/// 保存し、起動成功後にのみ enabled=true にする。
// 引数が多いのは「省略されたフィールドは現状維持」という部分更新の形をそのまま
// 引数で表しているため（`manager::handle_event` と同じ扱い）。構造体へまとめると
// REST とツールの 2 つの呼び出し口で組み立て方が分かれ、片方だけ更新漏れが起きる。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_nostr_settings(
    state: &AppState,
    agent_id: &str,
    relays: &[String],
    authors: &[String],
    keywords: &[String],
    kinds: &[u32],
    enabled: bool,
    secret_key_override: Option<&str>,
    owner_pubkey_override: Option<&str>,
) -> Result<(), (StatusCode, String)> {
    // 不正なオーナー識別子は**何も保存する前に**弾く（他の設定だけ通って
    // オーナーが黙って未設定のまま、を作らない）。
    let owner_pubkey = normalize_owner_pubkey_input(owner_pubkey_override)?;
    let existing = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_config(&conn, agent_id).unwrap_or(None)
    };
    let secret_key = secret_key_override
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().map(|e| e.secret_key.clone()))
        .unwrap_or_default();

    if secret_key.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "secret_key（nsec）が必要です。先に鍵を生成してください".to_string(),
        ));
    }

    // 【author/keyword が空でも拒否しない】（#271/#278）
    //
    // 以前は「author も keyword も無い＝全ノート洪水」として弾いていた。旧 nostaro の
    // `watch --json` が mention-only を無視して kind:1 を全件購読していたので当時は正しい。
    // 新 nostaro では `--json` でも mention-only が既定で効き、opencrab は
    // `--no-mention-only` を渡さないので、**空フィルタの購読は「自分宛の p タグのみ」＝
    // 最も狭い**（詳細は `docs/nostaro-interface.md`）。ここで弾くと「自分宛だけ受信する」
    // という一番普通の設定がダッシュボードから作れなくなる。

    let row = AgentNostrConfigRow {
        agent_id: agent_id.to_string(),
        secret_key,
        relays_json: serde_json::to_string(relays).unwrap_or_else(|_| "[]".to_string()),
        filter_json: serde_json::to_string(&json!({
            "authors": authors,
            "keywords": keywords,
            "kinds": kinds,
        }))
        .unwrap_or_else(|_| "{}".to_string()),
        enabled: false,
    };
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_nostr_config(&conn, &row)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // オーナー識別子は行の他の列とは独立に持つ（`upsert` は触らない）ので、
        // 明示指定があったときだけ書く。同じロックの中で行の作成に続けて書き、
        // 「行はできたがオーナーは入っていない」中間状態を外から見せない。
        if let Some(ref owner) = owner_pubkey {
            opencrab_db::queries::set_agent_nostr_owner_pubkey(&conn, agent_id, owner)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
    }

    // マネージャ反映。**「起動が成功してから enabled=true」の順序はここ（ハンドラ側）の
    // 方針として残す**（契約が持つのは起動と停止だけ）。`start` は上で upsert した行を
    // DB から読み直すが、その時点の行は enabled=false なので、Nostr 側のガードは
    // enabled を見ない（理由は `NostrGatewayManager` のトレイト実装の doc）。
    if let Some(gw) = state.gateways.get(gateway_kinds::NOSTR) {
        if enabled {
            gw.start(agent_id)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_agent_nostr_config_enabled(&conn, agent_id, true).ok();
        } else {
            gw.stop(agent_id).await;
        }
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct GenerateNostrBody {
    /// vanity prefix（npub の `npub1` 以降）。空なら通常のランダム鍵。
    #[serde(default)]
    pub prefix: String,
    /// 既存の秘密鍵を上書きするか。既定 false（既存があれば 409 で拒否する）。
    #[serde(default)]
    pub overwrite: bool,
}

/// POST /api/agents/{id}/nostr/generate — nostaro の vanity で新規鍵を生成し保存する。
///
/// **operator 向け**（LLM ツールではない）。生成鍵はエージェントの Nostr アイデンティティ
/// になる。既存鍵は誤って潰さないよう、上書きは `overwrite=true` を要求する。生成後は
/// enabled=false（鍵を差し替えたら停止し、フィルタ確認後に operator が再有効化する）。
/// nsec は応答で返さない（既存の完全マスク方針に合わせ、平文を外へ出さない）。
pub async fn generate_nostr_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GenerateNostrBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // prefix を先に検証して不正なら 400（無効 prefix で nostaro を spawn しない）。
    let prefix = body.prefix.trim();
    validate_vanity_prefix(prefix).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let existing = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_config(&conn, &id).unwrap_or(None)
    };
    // 既存鍵を誤って潰さない（アイデンティティ喪失を防ぐ）。
    if let Some(e) = existing.as_ref() {
        if !e.secret_key.is_empty() && !body.overwrite {
            return Err((
                StatusCode::CONFLICT,
                "既に秘密鍵が設定されています。上書きするには overwrite=true を指定してください"
                    .to_string(),
            ));
        }
    }

    // 鍵生成（nostaro vanity）は **transport 固有の操作**で、ライフサイクル契約
    // （起動 / 停止 / 生存確認）には無い。capability の受け口から引く（#191 段階2 PR4）。
    // **受け口が無い（未登録 / 提供しない）構成は 503**。名指しフィールドが `None`
    // だったときと同じ判定・同じ文言で、判定するこの位置（既存鍵の 409 の後、生成の
    // 手前）も変えない。
    let Some(provisioning) = state
        .gateways
        .get(gateway_kinds::NOSTR)
        .and_then(|gw| gw.key_provisioning())
    else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Nostr マネージャが無効です".to_string(),
        ));
    };

    // 生成（nostaro vanity）。同時実行は NostaroCli 内のゲートで 1 に直列化する
    // （進行中なら待つ。生成は ≤3 文字で通常即時）。失敗（未インストール/timeout 等）は
    // 500。エラー文言に秘密鍵が載らないことは parse 側で担保済み（失敗経路に鍵は無い）。
    let generated = provisioning
        .generate_key(prefix)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 鍵を差し替えるので、稼働中なら止める（新アイデンティティで黙って走らせない）。
    // 停止は共通操作で、上と同じ登録簿から引く。
    if let Some(gw) = state.gateways.get(gateway_kinds::NOSTR) {
        gw.stop(&id).await;
    }

    // 生成中（最大 timeout ぶん）に別リクエストが鍵を書き込む TOCTOU を避けるため、
    // 「既存鍵の再確認 → relays/filter 保持 → upsert」を**同一ロック内**で原子的に行う。
    {
        let conn = state.db.lock().unwrap();
        let current = opencrab_db::queries::get_agent_nostr_config(&conn, &id).unwrap_or(None);
        if let Some(c) = current.as_ref() {
            if !c.secret_key.is_empty() && !body.overwrite {
                return Err((
                    StatusCode::CONFLICT,
                    "既に秘密鍵が設定されています。上書きするには overwrite=true を指定してください"
                        .to_string(),
                ));
            }
        }
        let (relays_json, filter_json) = current
            .as_ref()
            .map(|c| (c.relays_json.clone(), c.filter_json.clone()))
            .unwrap_or_else(|| ("[]".to_string(), "{}".to_string()));
        let row = AgentNostrConfigRow {
            agent_id: id.clone(),
            secret_key: generated.secret,
            relays_json,
            filter_json,
            enabled: false,
        };
        opencrab_db::queries::upsert_agent_nostr_config(&conn, &row)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    Ok(Json(json!({
        "generated": true,
        "npub": generated.public_id,
        "pubkey": generated.public_key_hex,
    })))
}

/// POST /api/agents/{id}/nostr/start
pub async fn start_nostr_gateway(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let row = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_config(&conn, &id).unwrap_or(None)
    };
    if row.is_none() {
        return Err((StatusCode::NOT_FOUND, "Nostr 設定がありません".to_string()));
    }
    // 起動が成功してから enabled=true にする（失敗時に「enabled だが未稼働」の
    // 不整合を残さない）。この順序はハンドラ側の方針として残す。
    if let Some(gw) = state.gateways.get(gateway_kinds::NOSTR) {
        gw.start(&id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_config_enabled(&conn, &id, true).ok();
    }
    Ok(Json(json!({"started": true})))
}

/// POST /api/agents/{id}/nostr/stop
pub async fn stop_nostr_gateway(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_config_enabled(&conn, &id, false).ok();
    }
    if let Some(gw) = state.gateways.get(gateway_kinds::NOSTR) {
        gw.stop(&id).await;
    }
    Json(json!({"stopped": true}))
}

/// DELETE /api/agents/{id}/nostr
pub async fn delete_nostr_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    if let Some(gw) = state.gateways.get(gateway_kinds::NOSTR) {
        gw.stop(&id).await;
    }
    let deleted = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::delete_agent_nostr_config(&conn, &id).unwrap_or(false)
    };
    Json(json!({"deleted": deleted}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ダミー鍵（実在の pubkey は書かない）。
    const DUMMY_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    /// 未指定は現状維持、空文字は未設定へ戻す。
    #[test]
    fn owner_pubkey_input_absent_keeps_and_empty_clears() {
        assert_eq!(normalize_owner_pubkey_input(None).unwrap(), None);
        assert_eq!(
            normalize_owner_pubkey_input(Some("")).unwrap(),
            Some(String::new())
        );
        assert_eq!(
            normalize_owner_pubkey_input(Some("   ")).unwrap(),
            Some(String::new())
        );
    }

    /// **本丸**: npub でも hex でも受け付け、保存形（hex）へ揃う。
    #[test]
    fn owner_pubkey_input_normalizes_npub_and_hex_to_the_same_value() {
        let npub = opencrab_nostr::to_npub(DUMMY_HEX).unwrap();
        assert_eq!(
            normalize_owner_pubkey_input(Some(&npub)).unwrap(),
            Some(DUMMY_HEX.to_string()),
            "npub が保存形の hex に揃わない"
        );
        assert_eq!(
            normalize_owner_pubkey_input(Some(DUMMY_HEX)).unwrap(),
            Some(DUMMY_HEX.to_string())
        );
        // 大文字 hex や前後の空白も同じ値へ。
        assert_eq!(
            normalize_owner_pubkey_input(Some(&format!("  {}\n", DUMMY_HEX.to_ascii_uppercase())))
                .unwrap(),
            Some(DUMMY_HEX.to_string())
        );
    }

    /// 正規化できない値は 400。**黙って保存しない**
    /// （設定できたように見えて永久に誰とも一致しない行を作らせない）。
    #[test]
    fn malformed_owner_pubkey_input_is_rejected() {
        for bad in ["abcd", "npub1broken", "nsec1whatever"] {
            let (code, _msg) =
                normalize_owner_pubkey_input(Some(bad)).expect_err("不正値を通した: {bad}");
            assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");
        }
    }
}
