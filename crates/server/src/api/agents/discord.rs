pub async fn get_discord_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let cfg = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_discord_config(&conn, &id).unwrap()
    };

    match cfg {
        Some(cfg) => {
            // Mask the token: show first 10 chars + "..."
            let token_masked = if cfg.bot_token.len() > 10 {
                format!("{}...", &cfg.bot_token[..10])
            } else {
                "***".to_string()
            };

            // 未登録（discord feature 無効 / マネージャ未生成）は false。
            let running = state.gateways.is_running(gateway_kinds::DISCORD, &id);

            Json(serde_json::json!({
                "configured": true,
                "enabled": cfg.enabled,
                "token_masked": token_masked,
                "owner_discord_id": cfg.owner_discord_id,
                "running": running,
            }))
        }
        None => Json(serde_json::json!({
            "configured": false,
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct PatchDiscordConfigRequest {
    pub bot_token: Option<String>,
    pub owner_discord_id: Option<String>,
}

pub async fn patch_discord_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PatchDiscordConfigRequest>,
) -> Json<serde_json::Value> {
    // 既存の設定を取得
    let existing = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_discord_config(&conn, &id).unwrap()
    };

    let Some(_existing) = existing else {
        return Json(serde_json::json!({
            "ok": false,
            "error": "No Discord config found. Use PUT to create one.",
        }));
    };

    // 部分更新
    let updated = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::patch_agent_discord_config(
            &conn,
            &id,
            req.bot_token.as_deref(),
            // PUT と同じく入口で正規化する（理由は update_discord_config のコメント参照）。
            req.owner_discord_id.as_deref().map(str::trim),
        )
        .unwrap()
    };

    if !updated {
        return Json(serde_json::json!({
            "ok": false,
            "error": "Update failed.",
        }));
    };

    // 更新後の設定を返す
    let cfg = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_discord_config(&conn, &id)
            .unwrap()
            .unwrap()
    };

    let token_masked = if cfg.bot_token.len() > 10 {
        format!("{}...", &cfg.bot_token[..10])
    } else {
        "***".to_string()
    };

    // Restart the gateway with new config if enabled and token is present.
    if let Some(gw) = state.gateways.get(gateway_kinds::DISCORD) {
        // Stop the current gateway first (no-op if not running).
        gw.stop(&id).await;
        // 起動条件（enabled かつトークンが空白でない）の判定は `start` の中にある
        // （#191 段階2 PR3 で `gateway_will_start` ごと実装側へ持ち上げた）。
        // 条件を満たさずに見送られたときは以前と同じく**黙って何もしない**ので、
        // `StartDeclined` は error ログに出さない（本当の起動失敗だけ残す）。
        if let Err(e) = gw.start(&id).await {
            if !opencrab_actions::is_start_declined(&e) {
                tracing::error!(agent_id = %id, error = %e, "Failed to restart Discord gateway after patch");
            }
        }
    }

    Json(serde_json::json!({
        "ok": true,
        "configured": true,
        "enabled": cfg.enabled,
        "token_masked": token_masked,
        "owner_discord_id": cfg.owner_discord_id,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateDiscordConfigRequest {
    pub bot_token: String,
    pub owner_discord_id: Option<String>,
}

pub async fn update_discord_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateDiscordConfigRequest>,
) -> Json<serde_json::Value> {
    // 入口で正規化する。owner の判定は `api::is_owner_id`（trim 済み比較）を通る経路と、
    // 下位 crate の生比較のまま残っている経路（form/modal、ボタン操作）が混在するため、
    // 前後空白付きの値を保存すると「DM は通るのに owner 専用 UI だけ無言で拒否される」
    // 半端な状態になる。判定述語を下位 crate へ移す整理は #174。
    let owner_discord_id = req.owner_discord_id.unwrap_or_default().trim().to_string();

    // Save to DB.
    {
        let conn = state.db.lock().unwrap();
        let cfg = opencrab_db::queries::AgentDiscordConfigRow {
            agent_id: id.clone(),
            bot_token: req.bot_token,
            owner_discord_id,
            enabled: true,
        };
        opencrab_db::queries::upsert_agent_discord_config(&conn, &cfg).unwrap();
    }

    // Start the gateway (only when a Discord gateway is registered).
    // 資格情報は `start` が**この直前に書いた行**を DB から読み直す（契約が引数を取らない
    // 理由は `AgentGatewayLifecycle` の doc 参照）。正規化済み owner を保存しているので、
    // 読み直しても渡していた値と同じになる。
    if let Some(gw) = state.gateways.get(gateway_kinds::DISCORD) {
        match gw.start(&id).await {
            Ok(()) => {
                return Json(serde_json::json!({
                    "ok": true,
                    "message": "Discord bot started.",
                }));
            }
            Err(e) => {
                tracing::error!(agent_id = %id, error = %e, "Failed to start per-agent Discord gateway");
                return Json(serde_json::json!({
                    "ok": false,
                    "error": e.to_string(),
                }));
            }
        }
    }

    // Config saved but gateway not started (discord feature disabled or manager not registered).
    Json(serde_json::json!({
        "ok": true,
        "message": "Config saved. Gateway not started (discord feature not active).",
    }))
}

pub async fn start_discord_gateway(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let cfg = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_discord_config(&conn, &id).unwrap()
    };

    if cfg.is_none() {
        return Json(serde_json::json!({ "ok": false, "error": "No Discord config found." }));
    }

    // Set enabled=1 in DB.
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_discord_config_enabled(&conn, &id, true).unwrap();
    }

    // 資格情報は `start` が DB から読み直す。enabled は**この時点で既に 1** なので、
    // `start` 側のガード（enabled かつトークンあり）が新たに弾くのは
    // 「空白だけのトークン」だけ（それは以前も接続に失敗していた）。
    if let Some(gw) = state.gateways.get(gateway_kinds::DISCORD) {
        match gw.start(&id).await {
            Ok(()) => return Json(serde_json::json!({ "ok": true })),
            Err(e) => {
                tracing::error!(agent_id = %id, error = %e, "Failed to start Discord gateway");
                return Json(serde_json::json!({ "ok": false, "error": e.to_string() }));
            }
        }
    }

    Json(serde_json::json!({ "ok": false, "error": "Discord feature not active." }))
}

pub async fn stop_discord_gateway(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    // Set enabled=0 in DB.
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_discord_config_enabled(&conn, &id, false).unwrap();
    }

    if let Some(gw) = state.gateways.get(gateway_kinds::DISCORD) {
        gw.stop(&id).await;
    }

    Json(serde_json::json!({ "ok": true }))
}

pub async fn delete_discord_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    // Stop the gateway.
    if let Some(gw) = state.gateways.get(gateway_kinds::DISCORD) {
        gw.stop(&id).await;
    }

    // Delete from DB.
    let deleted = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::delete_agent_discord_config(&conn, &id).unwrap()
    };

    Json(serde_json::json!({"deleted": deleted}))
}

// ============================================
// Memory Index API
// ============================================

