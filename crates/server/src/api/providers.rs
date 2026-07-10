//! ダッシュボードのプロバイダー設定 API。
//!
//! LLM: TOML（[llm.providers.*]）を土台に DB オーバーライドをマージし、
//! 保存のたびにルーターを再構築してホットスワップする（再起動不要）。
//! API キーは応答で必ずマスクし、平文は返さない。
//!
//! Voice: DB オーバーライドは VoiceConfig の完全置換。voice ランタイムが
//! 稼働中（discord 起動済み + enabled）なら STT/TTS を即時差し替える。
//! enabled の切り替え自体は起動時配線のため再起動が必要。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::json;

use crate::AppState;

/// ダッシュボードに列挙する既知のプロバイダー種別。
/// TOML/DB に無くても一覧に出し、UI から新規設定できるようにする。
const KNOWN_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "openrouter",
    "ollama",
    "llamacpp",
    "codex",
    "chatgpt",
];

/// API キーのマスク表示。末尾 4 文字だけ見せる（短いキーは伏せ字のみ）。
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() >= 8 {
        format!(
            "••••{}",
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    } else {
        "••••".to_string()
    }
}

fn provider_entry(
    name: &str,
    state: &AppState,
    overrides: &[opencrab_db::queries::LlmProviderOverrideRow],
    active_names: &[&str],
) -> serde_json::Value {
    let toml_cfg = state.llm_config.providers.get(name);
    let ov = overrides.iter().find(|r| r.provider == name);

    // 実効 API キーの出所: DB > TOML(env 展開済み) > なし
    let (api_key_source, api_key_masked) = match ov.and_then(|o| o.api_key.as_deref()) {
        Some(k) if !k.is_empty() => ("db", mask_key(k)),
        _ => match toml_cfg.map(|c| c.api_key.as_str()) {
            Some(k) if !k.is_empty() => ("toml", mask_key(k)),
            _ => ("none", String::new()),
        },
    };
    let base_url = ov
        .and_then(|o| o.base_url.clone())
        .or_else(|| toml_cfg.map(|c| c.base_url.clone()))
        .unwrap_or_default();
    let default_model = ov
        .and_then(|o| o.default_model.clone())
        .or_else(|| toml_cfg.map(|c| c.default_model.clone()))
        .unwrap_or_default();
    // 推論（thinking）強度: DB > TOML > 空（モデル既定）
    let reasoning_effort = ov
        .and_then(|o| o.reasoning_effort.clone())
        .or_else(|| toml_cfg.map(|c| c.reasoning_effort.clone()))
        .unwrap_or_default();

    json!({
        "name": name,
        // 現在のルーターに登録されているか（= 実際に使える状態か）
        "active": active_names.contains(&name),
        "in_toml": toml_cfg.is_some(),
        "has_override": ov.is_some(),
        "enabled_override": ov.and_then(|o| o.enabled),
        "api_key_source": api_key_source,
        "api_key_masked": api_key_masked,
        "base_url": base_url,
        "default_model": default_model,
        "reasoning_effort": reasoning_effort,
    })
}

/// GET /api/llm/providers — プロバイダー一覧（実効設定 + オーバーライド状態）
pub async fn list_providers(State(state): State<AppState>) -> Json<serde_json::Value> {
    let overrides = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_llm_provider_overrides(&conn).unwrap_or_default()
    };
    let router = state.llm_router.get();
    let active = router.provider_names();

    // 既知プロバイダー + TOML/DB にだけ存在する名前もすべて列挙
    let mut names: Vec<String> = KNOWN_PROVIDERS.iter().map(|s| s.to_string()).collect();
    for n in state.llm_config.providers.keys() {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    for r in &overrides {
        if !names.contains(&r.provider) {
            names.push(r.provider.clone());
        }
    }

    let providers: Vec<serde_json::Value> = names
        .iter()
        .map(|n| provider_entry(n, &state, &overrides, &active))
        .collect();

    Json(json!({
        "providers": providers,
        "default_model": state.default_model,
    }))
}

/// 三値セマンティクスの解釈: キー欠落 = 変更しない / `null` = 解除 / 値 = 上書き。
///
/// serde の `Option<T>` は JSON の `null` を `None` に潰してしまい「欠落」と
/// 区別できない（`Some(Value::Null)` にはならない）。そのため PUT ボディは
/// `serde_json::Map` で受け、**キーの有無**で三値を判定する。
///
/// `map.get(key)` が返すのは:
/// - `None`           → キー欠落（変更しない）
/// - `Some(Null)`     → 解除
/// - `Some(Bool/Str)` → 上書き
fn tri_bool(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<Option<bool>>, String> {
    match map.get(key) {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(serde_json::Value::Bool(b)) => Ok(Some(Some(*b))),
        Some(other) => Err(format!("{key}: expected bool or null, got: {other}")),
    }
}

fn tri_string(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<Option<String>>, String> {
    match map.get(key) {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(serde_json::Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(other) => Err(format!("{key}: expected string or null, got: {other}")),
    }
}

/// PUT /api/llm/providers/{name} — オーバーライド保存 + ルーター再構築
///
/// ボディはキー有無で三値を判定するため `serde_json::Map` で受ける（上記参照）。
pub async fn update_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Map<String, serde_json::Value>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || name.is_empty()
        || name.len() > 64
    {
        return Err((StatusCode::BAD_REQUEST, "invalid provider name".into()));
    }

    {
        let conn = state.db.lock().unwrap();
        // 既存行をベースに部分更新（欠落フィールドは維持）
        let mut row = opencrab_db::queries::get_llm_provider_override(&conn, &name)
            .map_err(internal)?
            .unwrap_or(opencrab_db::queries::LlmProviderOverrideRow {
                provider: name.clone(),
                ..Default::default()
            });
        let bad = |e: String| (StatusCode::BAD_REQUEST, e);
        if let Some(v) = tri_bool(&body, "enabled").map_err(bad)? {
            row.enabled = v;
        }
        if let Some(v) = tri_string(&body, "api_key").map_err(bad)? {
            // 空文字は「解除」として None に正規化
            row.api_key = v.filter(|s| !s.is_empty());
        }
        if let Some(v) = tri_string(&body, "base_url").map_err(bad)? {
            row.base_url = v;
        }
        if let Some(v) = tri_string(&body, "default_model").map_err(bad)? {
            row.default_model = v;
        }
        if let Some(v) = tri_string(&body, "reasoning_effort").map_err(bad)? {
            // 空文字は「解除」として None に正規化
            row.reasoning_effort = v.filter(|s| !s.is_empty());
        }
        // 全フィールドが None ならオーバーライド行ごと削除
        if row.enabled.is_none()
            && row.api_key.is_none()
            && row.base_url.is_none()
            && row.default_model.is_none()
            && row.reasoning_effort.is_none()
        {
            opencrab_db::queries::delete_llm_provider_override(&conn, &name).map_err(internal)?;
        } else {
            opencrab_db::queries::upsert_llm_provider_override(&conn, &row).map_err(internal)?;
        }
    }

    reload_router(&state)?;

    // 更新後の状態を返す
    let overrides = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_llm_provider_overrides(&conn).unwrap_or_default()
    };
    let router = state.llm_router.get();
    let active = router.provider_names();
    Ok(Json(json!({
        "provider": provider_entry(&name, &state, &overrides, &active),
        "reloaded": true,
    })))
}

/// DELETE /api/llm/providers/{name}/override — TOML 設定に戻す
pub async fn delete_provider_override(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::delete_llm_provider_override(&conn, &name).map_err(internal)?;
    }
    reload_router(&state)?;
    Ok(Json(json!({"deleted": true, "reloaded": true})))
}

/// POST /api/llm/providers/reload — TOML + DB から再構築（手動リロード）
pub async fn reload_providers(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    reload_router(&state)?;
    let router = state.llm_router.get();
    Ok(Json(json!({
        "reloaded": true,
        "active_providers": router.provider_names(),
    })))
}

/// TOML + DB オーバーライドからルーターを再構築してスワップする。
fn reload_router(state: &AppState) -> Result<(), (StatusCode, String)> {
    let overrides = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_llm_provider_overrides(&conn).map_err(internal)?
    };
    let merged = crate::config::apply_llm_overrides(&state.llm_config, &overrides);
    let router = crate::config::build_llm_router(&merged).map_err(internal)?;
    let names: Vec<String> = router
        .provider_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    state.llm_router.swap(router);
    tracing::info!(providers = ?names, "LLM router hot-reloaded from dashboard settings");
    Ok(())
}

// ============================================
// Voice (VC) 設定
// ============================================

/// GET /api/voice/config — 実効 voice 設定と状態
pub async fn get_voice_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let override_json = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_voice_config_override(&conn).unwrap_or(None)
    };
    let (config, source) = match &override_json {
        Some(json_str) => match serde_json::from_str::<opencrab_voice::VoiceConfig>(json_str) {
            Ok(c) => (c, "db"),
            Err(_) => (state.voice_config.as_ref().clone(), "toml"),
        },
        None => (state.voice_config.as_ref().clone(), "toml"),
    };
    let runtime_active = state.voice_runtime.lock().unwrap().is_some();
    Json(json!({
        "config": config,
        "source": source,
        // ランタイム稼働中なら STT/TTS 変更は即時反映。enabled の切替は再起動が必要。
        "runtime_active": runtime_active,
    }))
}

/// PUT /api/voice/config — 保存（完全置換）+ 可能ならホットスワップ
pub async fn update_voice_config(
    State(state): State<AppState>,
    Json(config): Json<opencrab_voice::VoiceConfig>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // 保存**前**にプロバイダを検証する（enabled のときのみ）。壊れた設定を
    // DB に残したまま 400 を返すと、GET が db 由来として壊れた値を表示し、
    // 次回起動で voice が黙って無効化される（レビュー指摘）。検証を通った
    // Arc をそのまま apply に再利用する。
    let built = if config.enabled {
        match (
            opencrab_voice::build_stt(&config.stt),
            opencrab_voice::build_tts(&config.tts),
        ) {
            (Ok(stt), Ok(tts)) => Some((stt, tts)),
            (stt, tts) => {
                let mut errs = Vec::new();
                if let Err(e) = stt {
                    errs.push(format!("STT: {e}"));
                }
                if let Err(e) = tts {
                    errs.push(format!("TTS: {e}"));
                }
                return Err((StatusCode::BAD_REQUEST, errs.join(" / ")));
            }
        }
    } else {
        None
    };

    // 検証通過後に永続化
    let json_str = serde_json::to_string(&config).map_err(internal)?;
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_voice_config_override(&conn, &json_str).map_err(internal)?;
    }

    // 稼働中ランタイムがあれば検証済みプロバイダを即時差し替え
    let mut applied_live = false;
    if let Some((stt, tts)) = built {
        // guard は clone してすぐ手放す（apply は別の内部ロックを取るため
        // ここで voice_runtime mutex を保持したまま await/apply しない）
        let runtime = state.voice_runtime.lock().unwrap().clone();
        if let Some(rt) = runtime {
            rt.apply_settings(stt, tts, config.tts.clone(), config.stt.language.clone());
            applied_live = true;
        }
    }

    Ok(Json(json!({
        "saved": true,
        "applied_live": applied_live,
        // ランタイム未稼働（voice 無効で起動 or discord 無効）の変更は再起動で反映
        "restart_required": !applied_live,
    })))
}

/// DELETE /api/voice/config — オーバーライド削除（TOML に戻す。反映は再起動）
pub async fn delete_voice_config(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::delete_voice_config_override(&conn).map_err(internal)?;
    Ok(Json(json!({"deleted": true, "restart_required": true})))
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
