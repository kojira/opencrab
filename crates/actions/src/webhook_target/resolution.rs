/// subtask webhook の解決元（優先順位）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookSource {
    Explicit,
    ToolDefault,
    AgentDefault,
    GlobalDefault,
    EnvConfig,
}

impl WebhookSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            WebhookSource::Explicit => "explicit",
            WebhookSource::ToolDefault => "tool_default",
            WebhookSource::AgentDefault => "agent_default",
            WebhookSource::GlobalDefault => "global_default",
            WebhookSource::EnvConfig => "env_config",
        }
    }
}

/// subtask webhook の解決結果。
pub enum WebhookResolution {
    /// 検証済みの webhook。ここへ配送する。
    Use {
        config: WebhookConfig,
        source: WebhookSource,
    },
    /// 当選した scope で enabled=false。webhook 無効・fallthrough しない。
    Disabled { source: WebhookSource },
    /// どこにも設定が無い。
    None,
    /// 検証失敗 → spawn_subtask を失敗させる。
    Error {
        code: String,
        message: String,
        source: WebhookSource,
    },
}

/// events_json (Option<String>) から events を解析する。
fn parse_events_json(events_json: &Option<String>) -> Option<Vec<String>> {
    let raw = events_json.as_ref()?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let arr = value.as_array()?;
    Some(
        arr.iter()
            .filter_map(|e| e.as_str().map(|s| s.to_string()))
            .collect(),
    )
}

/// DB 行を WebhookResolution へ変換する（enabled/url 検証含む）。
fn resolve_db_row(row: AgentWebhookConfigRowLite, source: WebhookSource) -> WebhookResolution {
    if !row.enabled {
        return WebhookResolution::Disabled { source };
    }
    if let Err(reason) = validate_webhook_url(&row.url) {
        return WebhookResolution::Error {
            code: "invalid_default_webhook".to_string(),
            message: reason,
            source,
        };
    }
    let events = parse_events_json(&row.events_json);
    WebhookResolution::Use {
        config: WebhookConfig {
            url: row.url,
            events,
        },
        source,
    }
}

/// resolve で必要な DB 行の最小フィールド。
struct AgentWebhookConfigRowLite {
    url: String,
    events_json: Option<String>,
    enabled: bool,
}

/// 1 つの scope について指定 kind 群を順に試し、最初に見つかった行を返す。
fn fetch_scope_row_kinds(
    conn: &rusqlite::Connection,
    scope: &str,
    agent_id: &str,
    tool_name: &str,
    kinds: &[&str],
) -> Option<AgentWebhookConfigRowLite> {
    for kind in kinds {
        if let Ok(Some(r)) =
            opencrab_db::queries::get_agent_webhook_config(conn, scope, agent_id, tool_name, kind)
        {
            return Some(AgentWebhookConfigRowLite {
                url: r.url,
                events_json: r.events_json,
                enabled: r.enabled,
            });
        }
    }
    None
}

/// 1 つの scope について subtask lifecycle の宛先行を取得する。
///
/// 優先順位は `subtask > lifecycle > activity`。subtask 専用に設定された明示的な
/// デフォルト（subtask/lifecycle kind）を、汎用 activity デフォルトより優先する。
/// activity family は subtask ライフサイクルも包含するため、subtask 専用行が無い
/// ときのフォールバックとして最後に見る。
fn fetch_scope_row(
    conn: &rusqlite::Connection,
    scope: &str,
    agent_id: &str,
    tool_name: &str,
) -> Option<AgentWebhookConfigRowLite> {
    fetch_scope_row_kinds(
        conn,
        scope,
        agent_id,
        tool_name,
        &["subtask", "lifecycle", "activity"],
    )
}

/// subtask webhook を固定順序で解決する。
///
/// 優先順位: explicit > tool default > agent default > global default > env config。
/// あるレベルで設定が見つかったら、それより下へは fall through しない
/// （error/disabled も同様に止まる）。
pub fn resolve_subtask_webhook(
    conn: &rusqlite::Connection,
    agent_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    env_config_default: Option<&WebhookConfig>,
) -> WebhookResolution {
    // 1. EXPLICIT
    // webhook キーがあり、url が非空（trim 後）のときだけ明示指定として扱う。
    // url が空文字 / 空白のみのときは「明示指定なし」とみなし、下位のデフォルト解決へ
    // フォールバックさせる（明示的に空 url を渡しても通知が無効化されない）。これは DB の
    // enabled=false による明示無効化（auditable disable）とは別物で、後者はその scope で
    // 配送を止め fall through しない。
    if let Some(wh) = args.get("webhook") {
        if !wh.is_null() {
            let url = wh
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !url.trim().is_empty() {
                if let Err(reason) = validate_webhook_url(&url) {
                    return WebhookResolution::Error {
                        code: "invalid_webhook_url".to_string(),
                        message: reason,
                        source: WebhookSource::Explicit,
                    };
                }
                let events = wh.get("events").and_then(|v| v.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|e| e.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                });
                return WebhookResolution::Use {
                    config: WebhookConfig {
                        url: url.trim().to_string(),
                        events,
                    },
                    source: WebhookSource::Explicit,
                };
            }
            // url 空 / 空白のみ → 明示指定なし扱い。下の DB / env デフォルトへ続行する。
        }
    }

    // 2. DB defaults: tool > agent > global。最初に見つかった行で確定。
    if let Some(row) = fetch_scope_row(conn, "tool", agent_id, "spawn_subtask") {
        return resolve_db_row(row, WebhookSource::ToolDefault);
    }
    if let Some(row) = fetch_scope_row(conn, "agent", agent_id, "") {
        return resolve_db_row(row, WebhookSource::AgentDefault);
    }
    if let Some(row) = fetch_scope_row(conn, "global", "*", "") {
        return resolve_db_row(row, WebhookSource::GlobalDefault);
    }

    // 3. env/config 互換フォールバック。DB 行が皆無のときのみ。
    let _ = tool_name;
    match env_config_default {
        Some(cfg) => WebhookResolution::Use {
            config: cfg.clone(),
            source: WebhookSource::EnvConfig,
        },
        None => WebhookResolution::None,
    }
}

/// 一般ツール/コマンド活動（activity family）の宛先を固定順序で解決する。
///
/// 優先順位: tool-specific(activity) > agent(activity) > global(activity)。
/// 明示 per-call webhook も env/config fallback も用いない（design 2.2: env/config は
/// subtask ファミリ限定）。activity kind の DB 行のみを見る。
/// disabled / 不正 URL は下位へ fall through しない（no-silent-fallback）。
pub fn resolve_activity_webhook(
    conn: &rusqlite::Connection,
    agent_id: &str,
    tool_name: &str,
) -> WebhookResolution {
    if !tool_name.is_empty() {
        if let Some(row) = fetch_scope_row_kinds(conn, "tool", agent_id, tool_name, &["activity"]) {
            return resolve_db_row(row, WebhookSource::ToolDefault);
        }
    }
    if let Some(row) = fetch_scope_row_kinds(conn, "agent", agent_id, "", &["activity"]) {
        return resolve_db_row(row, WebhookSource::AgentDefault);
    }
    if let Some(row) = fetch_scope_row_kinds(conn, "global", "*", "", &["activity"]) {
        return resolve_db_row(row, WebhookSource::GlobalDefault);
    }
    WebhookResolution::None
}

/// agent に適用され得る有効な activity デフォルトが 1 つでも存在するか。
///
/// `resolve_activity_webhook` と同じ scope 集合（tool / agent / global の activity 行）を
/// 見る。`list_agent_webhook_config` は `(agent_id = ? OR agent_id = '*') AND enabled = 1`
/// で引くため、agent 自身の tool/agent scope 行と global(`*`) 行を enabled のみ含む。
/// env/config fallback は使わない（activity kind の DB 行のみ）。
/// 配送 sink を立てる価値があるか（best-effort）の単一判定点。
pub fn has_activity_default(conn: &rusqlite::Connection, agent_id: &str) -> bool {
    opencrab_db::queries::list_agent_webhook_config(conn, Some(agent_id), false)
        .map(|rows| rows.iter().any(|r| r.kind == "activity"))
        .unwrap_or(false)
}

/// webhook 配送が最終的に失敗したとき、親セッションログに 1 件記録する。
///
/// raw url は決して渡さない（redacted_url のみ）。parent_session_id が空なら何もしない。
pub fn record_webhook_delivery_failure(
    conn: &rusqlite::Connection,
    agent_id: &str,
    parent_session_id: &str,
    subtask_id: &str,
    sub_session_id: &str,
    redacted_url: &str,
    error: &str,
) {
    if parent_session_id.is_empty() {
        return;
    }
    let content = json!({
        "type": "subtask_progress",
        "subtask_id": subtask_id,
        "session_id": sub_session_id,
        "webhook_status": "delivery_failed",
        "webhook_redacted_url": redacted_url,
        "webhook_error": error,
    })
    .to_string();
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id: parent_session_id.to_string(),
        log_type: "system".to_string(),
        content,
        speaker_id: None,
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    opencrab_db::queries::insert_session_log_best_effort(conn, &log);
}

/// Nostr 受信を Discord へ転記する宛先を **fail-closed** に解決する（issue #252 段階 A）。
///
/// エージェント単位設定（`agent_nostr_relay_config`）を読み、有効かつ URL が
/// Discord webhook として妥当なときだけ配送先 [`WebhookConfig`] を返す。以下は
/// すべて「転記しない（`None`）」に倒す:
///
/// - 行が無い（未設定）
/// - 読み出しに失敗した（DB が壊れている）
/// - `enabled = 0`（明示的に無効）
/// - `webhook_url` が NULL / 空
/// - `webhook_url` が Discord webhook として不正
///
/// 応答生成の判定ではなく**受信ループから同期的に**呼ばれる（軽い PK 読み 1 回）。
/// 返す `events` は `None`（全イベント相当）: 転記は種別で間引かない。
pub fn resolve_nostr_relay_webhook(
    conn: &rusqlite::Connection,
    agent_id: &str,
) -> Option<WebhookConfig> {
    let row = match opencrab_db::queries::get_agent_nostr_relay_config(conn, agent_id) {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(e) => {
            // 読めない = 壊れている。転記の方向へは倒さない。
            tracing::warn!(agent_id, "agent_nostr_relay_config の読み出しに失敗: {e}");
            return None;
        }
    };
    if !row.enabled {
        return None;
    }
    let url = row
        .webhook_url
        .map(|u| u.trim().to_string())
        .unwrap_or_default();
    if url.is_empty() {
        return None;
    }
    if let Err(reason) = validate_webhook_url(&url) {
        // 生 URL は載せない（reason は raw url を含まない契約）。
        tracing::warn!(
            agent_id,
            "Nostr 転記先 webhook が不正なので転記しない: {reason}"
        );
        return None;
    }
    Some(WebhookConfig { url, events: None })
}

