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
    "cursor",
    "acp",
    "chatgpt",
];

/// 保存時の自動起動確認（health_check）と、失敗時の自動ロールバックの対象。
///
/// health_check がローカルの `<binary> --version` で完結し高速・確定的な
/// codex/cursor のみ。acp は health_check が実際に ACP エージェントを起こす
/// （npx/uvx のコールドスタートで DL が走る等、外部ネットワーク依存で低速・非確定）
/// ため、これを自動ロールバックに使うと正常な設定でも誤って差し戻す。よって acp は
/// 自動対象から外し、明示的な接続テスト（/test エンドポイント・ダッシュボードのボタン）
/// でのみ本物のハンドシェイク確認を行う（#127 レビュー指摘）。
const AUTO_HEALTHTEST_PROVIDERS: &[&str] = &["codex", "cursor"];

fn is_auto_healthtest_provider(name: &str) -> bool {
    AUTO_HEALTHTEST_PROVIDERS.contains(&name)
}

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
    // 起動系（subprocess プロバイダ）: DB > TOML
    let binary_path = ov
        .and_then(|o| o.binary_path.clone())
        .or_else(|| toml_cfg.map(|c| c.binary_path.clone()))
        .unwrap_or_default();
    let args: Vec<String> = ov
        .and_then(|o| o.args_json.as_deref())
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .or_else(|| toml_cfg.map(|c| c.args.clone()))
        .unwrap_or_default();
    let working_dir = ov
        .and_then(|o| o.working_dir.clone())
        .or_else(|| toml_cfg.map(|c| c.working_dir.clone()))
        .unwrap_or_default();
    let timeout_secs = ov
        .and_then(|o| o.timeout_secs)
        .map(|t| t as u64)
        .or_else(|| toml_cfg.map(|c| c.timeout_secs))
        .unwrap_or(0);

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
        "binary_path": binary_path,
        "args": args,
        "working_dir": working_dir,
        "timeout_secs": timeout_secs,
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

/// プロバイダ名のバリデーション（英数字・`_`・`-` のみ、1〜64 文字）。
pub(crate) fn valid_provider_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 三値ボディ（`serde_json::Map`）を既存オーバーライド行にマージして新しい行を作る。
///
/// 戻り値 `None` は「全フィールドが未設定 → オーバーライド行を削除すべき」を意味する。
/// ダッシュボード PUT とエージェントツールの双方がこの一箇所を共有し、三値
/// セマンティクス（キー欠落=不変 / null=解除 / 値=上書き）を一貫させる。
pub(crate) fn build_override_row(
    conn: &rusqlite::Connection,
    name: &str,
    body: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<opencrab_db::queries::LlmProviderOverrideRow>, (StatusCode, String)> {
    // 既存行をベースに部分更新（欠落フィールドは維持）
    let mut row = opencrab_db::queries::get_llm_provider_override(conn, name)
        .map_err(internal)?
        .unwrap_or(opencrab_db::queries::LlmProviderOverrideRow {
            provider: name.to_string(),
            ..Default::default()
        });
    let bad = |e: String| (StatusCode::BAD_REQUEST, e);
    if let Some(v) = tri_bool(body, "enabled").map_err(bad)? {
        row.enabled = v;
    }
    if let Some(v) = tri_string(body, "api_key").map_err(bad)? {
        // 空文字は「解除」として None に正規化
        row.api_key = v.filter(|s| !s.is_empty());
    }
    if let Some(v) = tri_string(body, "base_url").map_err(bad)? {
        row.base_url = v;
    }
    if let Some(v) = tri_string(body, "default_model").map_err(bad)? {
        row.default_model = v;
    }
    if let Some(v) = tri_string(body, "reasoning_effort").map_err(bad)? {
        // 空文字は「解除」として None に正規化
        row.reasoning_effort = v.filter(|s| !s.is_empty());
    }
    // 起動系（subprocess プロバイダ codex/cursor/acp 向け）。空文字は解除。
    if let Some(v) = tri_string(body, "binary_path").map_err(bad)? {
        row.binary_path = v.filter(|s| !s.is_empty());
    }
    if let Some(v) = tri_string(body, "working_dir").map_err(bad)? {
        row.working_dir = v.filter(|s| !s.is_empty());
    }
    // args は文字列配列 or null（解除）。
    match body.get("args") {
        None => {}
        Some(serde_json::Value::Null) => row.args_json = None,
        Some(serde_json::Value::Array(a)) => {
            let args: Vec<String> = a
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            row.args_json = Some(serde_json::to_string(&args).unwrap_or_else(|_| "[]".into()));
        }
        Some(other) => return Err(bad(format!("args: expected array or null, got: {other}"))),
    }
    // timeout_secs は数値 or null（解除）。
    match body.get("timeout_secs") {
        None => {}
        Some(serde_json::Value::Null) => row.timeout_secs = None,
        Some(v) if v.is_u64() || v.is_i64() => row.timeout_secs = v.as_i64(),
        Some(other) => {
            return Err(bad(format!(
                "timeout_secs: expected int or null, got: {other}"
            )))
        }
    }
    // 全フィールドが None ならオーバーライド行ごと削除
    let all_none = row.enabled.is_none()
        && row.api_key.is_none()
        && row.base_url.is_none()
        && row.default_model.is_none()
        && row.reasoning_effort.is_none()
        && row.binary_path.is_none()
        && row.args_json.is_none()
        && row.working_dir.is_none()
        && row.timeout_secs.is_none();
    Ok((!all_none).then_some(row))
}

/// PUT /api/llm/providers/{name} — オーバーライド保存 + ルーター再構築
///
/// ボディはキー有無で三値を判定するため `serde_json::Map` で受ける（上記参照）。
pub async fn update_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Map<String, serde_json::Value>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !valid_provider_name(&name) {
        return Err((StatusCode::BAD_REQUEST, "invalid provider name".into()));
    }

    {
        let conn = state.db.lock().unwrap();
        match build_override_row(&conn, &name, &body)? {
            None => opencrab_db::queries::delete_llm_provider_override(&conn, &name)
                .map_err(internal)?,
            Some(row) => {
                opencrab_db::queries::upsert_llm_provider_override(&conn, &row).map_err(internal)?
            }
        }
    }

    reload_router(&state)?;

    // 保存後に起動確認（health_check）して結果を返す（ダッシュボードは自動差し戻しせず、
    // 繋がったか可視化するだけ。自動ロールバックはエージェント経由の変更で行う）。
    // 自動対象は health_check が高速・確定的な codex/cursor のみ。API キー型（openai 等）や
    // acp（ネットワーク依存の本物ハンドシェイク）は保存のたびに発火・ブロック・誤判定するのを
    // 避けて自動テストしない（明示的な接続テストは /test エンドポイント経由で行える）。
    let test = if is_auto_healthtest_provider(&name) {
        Some(test_provider(&state, &name).await)
    } else {
        None
    };

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
        "test_ok": test,
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

/// GET /api/llm/codex/diagnostics — サーバープロセスが実際に使う codex の
/// バイナリパスとバージョンを返す。シェルの codex と別物（古い）だと
/// 新しいモデル（gpt-5.6 系）が弾かれるため、その切り分け用。
pub async fn codex_diagnostics(State(state): State<AppState>) -> Json<serde_json::Value> {
    // config の binary_path（空なら PATH 上の "codex"）。サーバーの環境で解決される。
    let configured = state
        .llm_config
        .providers
        .get("codex")
        .map(|c| c.binary_path.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "codex".to_string());

    let resolved_path = resolve_binary_path(&configured).await;
    let (version, error) = match run_codex_version(&configured).await {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };

    Json(json!({
        // config で指定したパス（"codex" は PATH 検索）
        "configured_path": configured,
        // サーバー環境で実際に解決される絶対パス（which）
        "resolved_path": resolved_path,
        // `<codex> --version` の出力（例: "codex-cli 0.144.1"）
        "version": version,
        "error": error,
    }))
}

/// `<cmd> --version` を実行して出力を返す。cmd は config 由来（リクエスト
/// 入力ではない）なのでコマンドインジェクションの懸念はない。
async fn run_codex_version(cmd: &str) -> Result<String, String> {
    let out = tokio::process::Command::new(cmd)
        .arg("--version")
        .output()
        .await
        .map_err(|e| format!("codex を実行できませんでした（{cmd}）: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "`{cmd} --version` が失敗しました（{}）: {}",
            out.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// GET /api/llm/cursor/diagnostics — サーバープロセスが実際に使う cursor CLI の
/// バイナリパスとバージョンを返す。コマンド名がインストールでゆれる
/// （`cursor-agent` / `agent` / `cursor`）ため、config の binary_path が
/// サーバー環境で解決できているかの切り分け用。
pub async fn cursor_diagnostics(State(state): State<AppState>) -> Json<serde_json::Value> {
    // config の binary_path（空なら既定の "cursor-agent"）。
    let configured = state
        .llm_config
        .providers
        .get("cursor")
        .map(|c| c.binary_path.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cursor-agent".to_string());

    let resolved_path = resolve_binary_path(&configured).await;
    let (version, error) = match run_cursor_version(&configured).await {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };

    Json(json!({
        // config で指定したパス（"cursor-agent" は PATH 検索）
        "configured_path": configured,
        // サーバー環境で実際に解決される絶対パス（which）
        "resolved_path": resolved_path,
        // `<cursor> --version` の出力
        "version": version,
        "error": error,
    }))
}

/// `<cmd> --version` を実行して出力を返す（cursor CLI 用）。cmd は config 由来
/// （リクエスト入力ではない）なのでコマンドインジェクションの懸念はない。
async fn run_cursor_version(cmd: &str) -> Result<String, String> {
    let out = tokio::process::Command::new(cmd)
        .arg("--version")
        .output()
        .await
        .map_err(|e| format!("cursor を実行できませんでした（{cmd}）: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "`{cmd} --version` が失敗しました（{}）: {}",
            out.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// GET /api/llm/acp/diagnostics — サーバープロセスが実際に使う ACP エージェントの
/// 起動バイナリ・引数・解決パスを返す。ACP は `binary_path`（例 npx）+ `args`
/// （例 `-y @zed-industries/claude-code-acp`）で起動し、args がエージェント本体を
/// 担うため、`<binary> --version` だけでは起動可否が分からない。ここでは PATH/バイナリ
/// 解決の切り分け情報を返し、実際に ACP を話せるかは接続テスト（/test）で確認する。
pub async fn acp_diagnostics(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cfg = state.llm_config.providers.get("acp");
    let configured = cfg
        .map(|c| c.binary_path.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let args: Vec<String> = cfg.map(|c| c.args.clone()).unwrap_or_default();

    if configured.is_empty() {
        return Json(json!({
            "configured_path": "",
            "args": args,
            "resolved_path": null,
            "version": null,
            "error": "binary_path が未設定です（例: npx / gemini）。ダッシュボードで設定してください。",
        }));
    }

    let resolved_path = resolve_binary_path(&configured).await;
    // 起動バイナリ自体の --version（npx ラッパ等では ACP 本体のバージョンではない点に注意）。
    let (version, error) = match run_version(&configured).await {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };

    Json(json!({
        // config で指定した起動バイナリ（PATH 検索されうる）
        "configured_path": configured,
        // 起動引数（ACP 本体の指定を含む）
        "args": args,
        // サーバー環境で実際に解決される絶対パス（which）
        "resolved_path": resolved_path,
        // `<binary> --version` の出力（npx 等では起動バイナリ自身のバージョン）
        "version": version,
        "error": error,
    }))
}

/// `<cmd> --version` を実行して出力を返す（汎用）。cmd は config 由来
/// （リクエスト入力ではない）なのでコマンドインジェクションの懸念はない。
async fn run_version(cmd: &str) -> Result<String, String> {
    let out = tokio::process::Command::new(cmd)
        .arg("--version")
        .output()
        .await
        .map_err(|e| format!("実行できませんでした（{cmd}）: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "`{cmd} --version` が失敗しました（{}）: {}",
            out.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// サーバー環境で cmd が解決される絶対パスを返す（`which`）。
/// 既に絶対/相対パス指定ならそのまま返す。
async fn resolve_binary_path(cmd: &str) -> Option<String> {
    if cmd.contains('/') {
        return Some(cmd.to_string());
    }
    let out = tokio::process::Command::new("which")
        .arg(cmd)
        .output()
        .await
        .ok()?;
    if out.status.success() {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() {
            return Some(p);
        }
    }
    None
}

/// TOML + DB オーバーライドからルーターを再構築してスワップする。
/// プロバイダの起動確認（health_check）。subprocess プロバイダ（codex/cursor/acp）は
/// `<binary> --version` 等でバイナリの起動可否を見る。ルータに無ければ false。
pub(crate) async fn test_provider(state: &AppState, provider: &str) -> bool {
    let router = state.llm_router.get();
    let Some(p) = router.get_provider(provider).cloned() else {
        return false;
    };
    p.health_check().await.unwrap_or(false)
}

/// POST /api/llm/providers/{name}/test — 現在の設定で起動確認する。
pub async fn test_provider_endpoint(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<serde_json::Value> {
    let ok = test_provider(&state, &name).await;
    Json(json!({ "provider": name, "ok": ok }))
}

pub(crate) fn reload_router(state: &AppState) -> Result<(), (StatusCode, String)> {
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

/// `apply_provider_override_with_rollback` の結果。
pub(crate) struct ApplyOutcome {
    /// 最終状態に新しい設定が適用されているか（false = ロールバック済み）。
    pub applied: bool,
    /// subprocess プロバイダで起動確認（health_check）を実行した結果。
    /// None = テスト対象外（非 subprocess）。
    pub test_ok: Option<bool>,
    /// 起動確認に失敗し、元の設定へ戻したか。
    pub rolled_back: bool,
}

/// オーバーライドを適用 → ルーター再構築 → （subprocess なら）起動確認し、失敗したら
/// 元のオーバーライド状態へ**自動ロールバック**して再構築する。
///
/// エージェント（owner）がツールから設定を配線する経路で使う。壊れた設定で
/// プロバイダが起動不能になっても、直前の状態に自動復帰させ、その事実を呼び出し元に返す。
/// api_key はこの経路では受け付けない（秘密情報を LLM 経路に載せない）。
pub(crate) async fn apply_provider_override_with_rollback(
    state: &AppState,
    name: &str,
    body: &serde_json::Map<String, serde_json::Value>,
) -> Result<ApplyOutcome, (StatusCode, String)> {
    if !valid_provider_name(name) {
        return Err((StatusCode::BAD_REQUEST, "invalid provider name".into()));
    }
    // 1. 変更前のオーバーライド行を退避（ロールバック用）。
    let previous = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_llm_provider_override(&conn, name).map_err(internal)?
    };
    // オーバーライド行を `desired`（Some=upsert / None=削除）へ書き込むヘルパ。
    let write_override = |desired: Option<&opencrab_db::queries::LlmProviderOverrideRow>| {
        let conn = state.db.lock().unwrap();
        match desired {
            Some(row) => opencrab_db::queries::upsert_llm_provider_override(&conn, row),
            None => opencrab_db::queries::delete_llm_provider_override(&conn, name),
        }
    };
    // previous の状態へ戻すヘルパ（ロールバック）。
    let restore_previous = || write_override(previous.as_ref());

    // 2. 新しい行を構築して書き込む（全 None なら削除）。
    let desired = {
        let conn = state.db.lock().unwrap();
        build_override_row(&conn, name, body)?
    };
    write_override(desired.as_ref()).map_err(internal)?;

    // 3. ルーター再構築。build 自体が失敗する壊れた設定（例: 不正な引数）も「起動失敗」
    //    として扱い、DB を previous へ戻してから通知する（壊れた行を残さない）。
    if reload_router(state).is_err() {
        restore_previous().map_err(internal)?;
        // 復元後の再構築は previous（元は正常だった設定）なので成功するはず。失敗しても
        // best-effort でログのみ（router は swap 前で旧状態が生きている）。
        let _ = reload_router(state);
        tracing::warn!(
            provider = %name,
            "provider configuration failed to build router; rolled back to previous settings"
        );
        return Ok(ApplyOutcome {
            applied: false,
            test_ok: Some(false),
            rolled_back: true,
        });
    }

    // 4. 自動起動確認・自動ロールバックの対象は health_check が高速・確定的な codex/cursor のみ。
    //    API キー型や acp（ネットワーク依存の本物ハンドシェイク。npx/uvx のコールドスタートや
    //    一過性のネットワーク失敗で誤判定しうる）は対象外とし、正常な設定を誤って差し戻さない。
    //    acp の起動確認は明示的な /test（ダッシュボードのボタン）で行う（#127 レビュー指摘）。
    if !is_auto_healthtest_provider(name) {
        return Ok(ApplyOutcome {
            applied: true,
            test_ok: None,
            rolled_back: false,
        });
    }

    // 5. 対象でも、再構築後にルーターへ登録されていない場合（enabled=false での
    //    無効化やオーバーライド削除で TOML でも無効等）は、起動確認の対象が存在しない。
    //    これを health_check 失敗と混同するとロールバックが暴発し「無効化できない」
    //    バグになるため、意図した非登録は適用成功として扱う。
    if state.llm_router.get().get_provider(name).is_none() {
        return Ok(ApplyOutcome {
            applied: true,
            test_ok: None,
            rolled_back: false,
        });
    }

    // 6. 登録済み subprocess は起動確認。成功ならそのまま。
    if test_provider(state, name).await {
        return Ok(ApplyOutcome {
            applied: true,
            test_ok: Some(true),
            rolled_back: false,
        });
    }

    // 7. 失敗 → 元の状態へロールバックして再構築。DB は previous に戻す（永続状態は正）。
    //    復元後の再構築は元は正常だった設定なので成功するはずだが、失敗しても best-effort
    //    でログのみ（ここで Err を返すと DB=previous と実行中 router が食い違う報告になる）。
    restore_previous().map_err(internal)?;
    if reload_router(state).is_err() {
        tracing::error!(
            provider = %name,
            "rolled back DB to previous settings but router rebuild failed; restart may be needed"
        );
    }
    tracing::warn!(
        provider = %name,
        "provider configuration failed health_check; rolled back to previous settings"
    );
    Ok(ApplyOutcome {
        applied: false,
        test_ok: Some(false),
        rolled_back: true,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn valid_provider_name_rules() {
        assert!(valid_provider_name("acp"));
        assert!(valid_provider_name("openai-4o_mini"));
        assert!(!valid_provider_name(""));
        assert!(!valid_provider_name("bad name")); // space
        assert!(!valid_provider_name("bad/name")); // slash
        assert!(!valid_provider_name(&"x".repeat(65))); // too long
    }

    #[test]
    fn build_override_row_sets_launch_fields() {
        let db = opencrab_db::Db::memory().unwrap();
        let conn = db.lock().unwrap();
        let b = body(json!({
            "binary_path": "/usr/bin/acp",
            "args": ["--foo", "bar"],
            "timeout_secs": 90,
            "enabled": true,
        }));
        let row = build_override_row(&conn, "acp", &b).unwrap().unwrap();
        assert_eq!(row.provider, "acp");
        assert_eq!(row.binary_path.as_deref(), Some("/usr/bin/acp"));
        assert_eq!(row.args_json.as_deref(), Some(r#"["--foo","bar"]"#));
        assert_eq!(row.timeout_secs, Some(90));
        assert_eq!(row.enabled, Some(true));
    }

    #[test]
    fn build_override_row_all_none_means_delete() {
        let db = opencrab_db::Db::memory().unwrap();
        let conn = db.lock().unwrap();
        // 空文字/null は解除。全て解除なら None（＝行削除）。
        let b = body(json!({
            "binary_path": "",
            "working_dir": "",
            "args": null,
            "timeout_secs": null,
        }));
        assert!(build_override_row(&conn, "acp", &b).unwrap().is_none());
    }

    #[test]
    fn build_override_row_merges_onto_existing() {
        let db = opencrab_db::Db::memory().unwrap();
        let conn = db.lock().unwrap();
        // まず binary_path を保存。
        let first = build_override_row(&conn, "acp", &body(json!({"binary_path": "/a"})))
            .unwrap()
            .unwrap();
        opencrab_db::queries::upsert_llm_provider_override(&conn, &first).unwrap();
        // 次に timeout だけ変更 → binary_path は保持される（部分更新）。
        let merged = build_override_row(&conn, "acp", &body(json!({"timeout_secs": 30})))
            .unwrap()
            .unwrap();
        assert_eq!(merged.binary_path.as_deref(), Some("/a"));
        assert_eq!(merged.timeout_secs, Some(30));
    }

    #[test]
    fn build_override_row_rejects_bad_types() {
        let db = opencrab_db::Db::memory().unwrap();
        let conn = db.lock().unwrap();
        assert!(build_override_row(&conn, "acp", &body(json!({"timeout_secs": "nope"}))).is_err());
        assert!(build_override_row(&conn, "acp", &body(json!({"args": "nope"}))).is_err());
    }

    /// 自動 health_check/ロールバックの対象は codex/cursor のみ。acp は
    /// ネットワーク依存の本物ハンドシェイクのため自動対象から外す（#127 レビュー指摘）。
    #[test]
    fn auto_healthtest_excludes_acp_and_api_providers() {
        assert!(is_auto_healthtest_provider("codex"));
        assert!(is_auto_healthtest_provider("cursor"));
        assert!(!is_auto_healthtest_provider("acp"));
        assert!(!is_auto_healthtest_provider("openai"));
    }

    /// レビュー Finding 1 の回帰ガード: subprocess プロバイダを enabled=false で
    /// 無効化するとルーターから消える（get_provider が None）。
    /// `apply_provider_override_with_rollback` はこの「意図した非登録」を
    /// health_check 失敗と混同してロールバックしてはならない（＝適用成功で返す）。
    #[test]
    fn disabling_subprocess_provider_removes_it_from_router() {
        use crate::config::{apply_llm_overrides, build_llm_router, LlmConfig, ProviderConfig};
        let mut cfg = LlmConfig::default();
        cfg.providers.insert(
            "acp".to_string(),
            ProviderConfig {
                binary_path: "/bin/true".to_string(),
                ..Default::default()
            },
        );
        // default_provider は定義済みセクションを指す必要がある（#660）。
        // このテストの主題は override による登録/除外なので、既定を唯一の provider に合わせる。
        cfg.default_provider = "acp".to_string();
        // enabled 無指定 → acp は登録される（build は I/O せず成功）。
        let router = build_llm_router(&cfg).unwrap();
        assert!(router.get_provider("acp").is_some());
        // enabled=false override を適用 → acp はルーターから消える。
        let overrides = vec![opencrab_db::queries::LlmProviderOverrideRow {
            provider: "acp".to_string(),
            enabled: Some(false),
            ..Default::default()
        }];
        let merged = apply_llm_overrides(&cfg, &overrides);
        let router2 = build_llm_router(&merged).unwrap();
        assert!(
            router2.get_provider("acp").is_none(),
            "disabled subprocess provider must be absent from the router"
        );
    }
}
