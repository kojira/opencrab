//! 通知先（webhook）の設定・解決・秘匿・検証（gateway 非依存）。
//!
//! `crates/discord/src/gateway_actions/webhook.rs` から **純関数群だけ**を降ろしたもの
//! （#157 S4）。ここには
//!
//! - 通知先の設定型（[`WebhookConfig`]）とその出所（[`WebhookSource`]）・解決結果
//!   （[`WebhookResolution`]）
//! - 優先順位付きの解決（[`resolve_subtask_webhook`] / [`resolve_activity_webhook`] /
//!   [`has_activity_default`]）
//! - URL 検証（[`validate_webhook_url`]）と秘匿化（[`redact_webhook_url`] /
//!   [`redact_secrets`]）
//! - 送信先の文字数上限で分ける処理（[`chunk_text`] / [`build_part_messages`]）
//! - 配送失敗の記録（[`record_webhook_delivery_failure`]。raw url は受け取らない）
//!
//! だけが入る。**実際の HTTP 送信（transport）と Discord 固有の整形は含めない**
//! （それらは discord crate 側に残す）。
//!
//! 依存は `serde_json` / `rusqlite` / `opencrab_db` のみで、gateway crate には依存しない。

use serde_json::json;

/// spawn 時に渡される webhook 設定（最小形）。
#[derive(Clone, Debug, PartialEq)]
pub struct WebhookConfig {
    pub url: String,
    /// 送信対象イベント名。None の場合は全イベントを送る。
    pub events: Option<Vec<String>>,
}

impl WebhookConfig {
    /// spawn_subtask の引数から webhook 設定を取り出す。
    ///
    /// 期待する最小 JSON 形:
    /// ```json
    /// { "webhook": { "url": "https://...", "events": ["started", "completed"] } }
    /// ```
    /// `events` は省略可能。`url` が無い / 空 / 空白のみなら「明示指定なし」として
    /// None を返す（呼び出し側はデフォルトへフォールバックできる）。
    pub fn from_args(args: &serde_json::Value) -> Option<WebhookConfig> {
        let wh = args.get("webhook")?;
        let url = wh.get("url").and_then(|v| v.as_str())?.to_string();
        if url.trim().is_empty() {
            return None;
        }
        let events = wh.get("events").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        });
        Some(WebhookConfig { url, events })
    }

    pub fn from_parts(url: String, events: Option<Vec<String>>) -> Option<WebhookConfig> {
        if url.trim().is_empty() {
            return None;
        }
        Some(WebhookConfig { url, events })
    }

    /// 指定イベントを送るべきか。events 未指定なら常に true。
    pub fn wants(&self, event: &str) -> bool {
        match &self.events {
            Some(list) => {
                // 比較は canonical な status 名で行う。depth0 sink は
                // `tool_call_started`/`tool_call_completed`/... を、subtask path は
                // `subtask.started`/`started`/... を渡してくるため、両辺を正規化して
                // 同じ語彙（started/completed/failed/rejected/...）で突き合わせる。
                let want = normalize_event_name(event);
                if list.iter().any(|e| normalize_event_name(e) == want) {
                    return true;
                }
                // Backward compatibility for callers that created lifecycle streams before
                // progress existed: started/completed streams should include tool progress too.
                want == "progress" && list.iter().any(|e| normalize_event_name(e) == "started")
            }
            None => true,
        }
    }
}

/// イベント名を canonical な status 名へ正規化する。
/// `subtask.` 接頭辞（subtask lifecycle）と `tool_call_` 接頭辞（depth0 tool sink）を剥がし、
/// `started`/`completed`/`failed`/`rejected`/`timed_out`/`progress` 等の素の status に揃える。
fn normalize_event_name(event: &str) -> &str {
    event
        .strip_prefix("subtask.")
        .or_else(|| event.strip_prefix("tool_call_"))
        .unwrap_or(event)
}

/// raw text を char 単位で limit 以下の chunk に分割する（UTF-8 境界を壊さない）。
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() || limit == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(limit)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

/// raw text を `part X/N` 付きメッセージ列に整形する。
///
/// この framing はピアレビュー等で「part X/N の生データを読め」という
/// プロンプト規約とセットのプロトコルなので、変更時は全利用箇所と
/// system prompt（server/process.rs）を同時に更新すること。
pub fn build_part_messages(content: &str, limit: usize) -> Vec<String> {
    let chunks = chunk_text(content, limit);
    let part_count = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| format!("part {}/{}\n{}", i + 1, part_count, c))
        .collect()
}

/// Nostr 受信転記の Discord webhook POST body を組む（issue #252 段階 A）。
///
/// **必ず `allowed_mentions: { "parse": [] }` を乗せる**。Discord webhook は
/// `allowed_mentions` 省略時に content 内のメンション（`@everyone` / `@here` /
/// `<@userid>` / `<@&roleid>`）を全解決して通知を飛ばす。転記対象は第三者が送れる
/// 「エージェント宛の受信イベント」なので、抑止しないと第三者が `@everyone` を
/// 含めるだけで転記先サーバ全員へ通知が飛ぶ（mention 暴発）。空の parse 配列で
/// 全種別の解決を Discord 側で止める。
pub fn build_relay_webhook_body(chunk: &str) -> serde_json::Value {
    json!({
        "content": chunk,
        "allowed_mentions": { "parse": [] },
    })
}

/// webhook URL のトークン（末尾セグメント）をマスクして返す。ログ・応答用。
pub fn redact_webhook_url(url: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/[redacted]"),
        None => "[redacted]".to_string(),
    }
}

// ---- Secret redaction (retained utility) ----
//
// 本設計（docs/design-webhook-output-lossless.md §2 P4）により、covered 経路
// （work-channel 出力: command/stdout/stderr/args/result）からは redaction を完全に外した。
// 以下の関数群はもはや配送経路では呼ばれないが、covered 経路外（別タスク・§8）で再利用しうる
// 汎用ユーティリティとして残す。

const REDACTED: &str = "[REDACTED]";
const SECRET_PREFIXES: [&str; 4] = ["sk-", "ghp_", "xoxb-", "AKIA"];
const KV_MARKERS: [&str; 5] = ["TOKEN", "SECRET", "PASSWORD", "KEY", "API"];

/// 既知のシークレットパターンを [REDACTED] に置換する汎用ユーティリティ。
/// 取りこぼし対策として保守的に倒す（長い base64/hex 連や Bearer トークンも redact）。
/// 冪等: 既に redact 済みの文字列を再度通しても安全。
/// 注: covered 経路（webhook 出力）では **呼ばない**（§2 P4）。
pub fn redact_secrets(input: &str) -> String {
    input
        .split('\n')
        .map(redact_secrets_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_secrets_line(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for tok in line.split_whitespace() {
        if redact_next {
            out.push(REDACTED.to_string());
            redact_next = false;
            continue;
        }
        if tok.eq_ignore_ascii_case("bearer") {
            out.push(tok.to_string());
            redact_next = true;
            continue;
        }
        let (rendered, want_next) = redact_secret_token(tok);
        out.push(rendered);
        redact_next = want_next;
    }
    out.join(" ")
}

/// 1 トークンを検査し、(置換後文字列, 次トークンも redact すべきか) を返す。
fn redact_secret_token(tok: &str) -> (String, bool) {
    let core = tok.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | ',' | ';' | '(' | ')' | '`' | '[' | ']' | '{' | '}'
        )
    });
    if core.is_empty() {
        return (tok.to_string(), false);
    }
    // Discord webhook URL（ホスト不問）
    if core.contains("/api/webhooks/") {
        return (REDACTED.to_string(), false);
    }
    // KEY=VALUE / KEY:VALUE （キーに TOKEN/SECRET/PASSWORD/KEY/API を含む）
    if let Some(idx) = core.find(|c: char| c == '=' || c == ':') {
        let (k, rest) = core.split_at(idx);
        let delim = &core[idx..idx + 1];
        let value = &rest[1..];
        let key_up = k.trim_matches('"').to_ascii_uppercase();
        if KV_MARKERS.iter().any(|m| key_up.contains(m)) {
            if value.trim().is_empty() {
                // 値は次トークン側にある（例: `"token": "abc"`）
                return (tok.to_string(), true);
            }
            return (format!("{k}{delim}{REDACTED}"), false);
        }
    }
    // 既知プレフィックス
    for p in SECRET_PREFIXES {
        if core.starts_with(p) && core.len() > p.len() + 3 {
            return (REDACTED.to_string(), false);
        }
    }
    // 長い base64 / hex 連
    if core.len() >= 32
        && core
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'))
    {
        return (REDACTED.to_string(), false);
    }
    (tok.to_string(), false)
}

/// Discord webhook URL を検証する。空・パース不可・Discord webhook でない場合は Err(理由)。
///
/// 理由文字列に raw URL は含めない。
pub fn validate_webhook_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("url is empty".to_string());
    }
    let rest = match url.strip_prefix("https://") {
        Some(r) => r,
        None => return Err("url must start with https://".to_string()),
    };
    // host = "https://" と最初の '/' の間の部分。
    let (host, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => return Err("url has no path".to_string()),
    };
    const ALLOWED_HOSTS: [&str; 4] = [
        "discord.com",
        "discordapp.com",
        "ptb.discord.com",
        "canary.discord.com",
    ];
    if !ALLOWED_HOSTS.contains(&host) {
        return Err("host is not a Discord webhook host".to_string());
    }
    let webhook_path = match path.strip_prefix("/api/webhooks/") {
        Some(p) => p,
        None => return Err("path must start with /api/webhooks/".to_string()),
    };
    // id / token の 2 つ以上の非空セグメントが必要。
    let segments: Vec<&str> = webhook_path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return Err("path is missing webhook id or token".to_string());
    }
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_chunk_text_empty() {
        assert!(chunk_text("", 10).is_empty());
        assert!(chunk_text("abc", 0).is_empty());
    }

    #[test]
    fn test_chunk_text_shorter_than_limit() {
        let chunks = chunk_text("hello", 10);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn test_chunk_text_splits_in_order() {
        let chunks = chunk_text("abcdefg", 3);
        assert_eq!(chunks, vec!["abc", "def", "g"]);
        // reconstruction preserves order/content
        assert_eq!(chunks.concat(), "abcdefg");
    }

    #[test]
    fn test_chunk_text_respects_utf8_boundaries() {
        // multibyte chars must not be split mid-byte
        let chunks = chunk_text("あいうえお", 2);
        assert_eq!(chunks, vec!["あい", "うえ", "お"]);
        assert_eq!(chunks.concat(), "あいうえお");
    }

    #[test]
    fn test_build_relay_webhook_body_has_content() {
        let body = build_relay_webhook_body("hello");
        assert_eq!(body["content"], json!("hello"));
    }

    #[test]
    fn test_build_relay_webhook_body_suppresses_all_mentions() {
        // allowed_mentions.parse は必ず空配列で乗る（mention 暴発抑止の固定）。
        let body = build_relay_webhook_body("plain text");
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        // 空配列であること（省略でも非空でもない）を厳密に確認。
        let parse = body["allowed_mentions"]["parse"]
            .as_array()
            .expect("parse must be an array");
        assert!(parse.is_empty(), "parse must be empty to suppress mentions");
    }

    #[test]
    fn test_build_relay_webhook_body_suppresses_everyone_input() {
        // 第三者が @everyone 等を含むリプライを送っても、body は content をそのまま
        // 載せつつ allowed_mentions.parse: [] で全解決を止める。
        let hostile = "@everyone @here <@123> <@&456> pwn";
        let body = build_relay_webhook_body(hostile);
        assert_eq!(body["content"], json!(hostile));
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }

    #[test]
    fn test_webhook_config_from_args() {
        let cfg = WebhookConfig::from_args(&json!({
            "webhook": { "url": "https://discord.com/api/webhooks/x", "events": ["started", "completed"] }
        }))
        .unwrap();
        assert_eq!(cfg.url, "https://discord.com/api/webhooks/x");
        assert_eq!(
            cfg.events,
            Some(vec!["started".to_string(), "completed".to_string()])
        );
    }

    #[test]
    fn test_webhook_config_from_args_no_events() {
        let cfg = WebhookConfig::from_args(&json!({
            "webhook": { "url": "https://x" }
        }))
        .unwrap();
        assert_eq!(cfg.events, None);
    }

    #[test]
    fn test_webhook_config_from_args_missing_or_empty() {
        assert!(WebhookConfig::from_args(&json!({})).is_none());
        assert!(WebhookConfig::from_args(&json!({ "webhook": {} })).is_none());
        assert!(WebhookConfig::from_args(&json!({ "webhook": { "url": "" } })).is_none());
        // 空白のみの url も「指定なし」として None（フォールバック可能）。
        assert!(WebhookConfig::from_args(&json!({ "webhook": { "url": "   " } })).is_none());
    }

    #[test]
    fn test_webhook_config_from_parts_missing_or_empty() {
        assert!(WebhookConfig::from_parts("".to_string(), None).is_none());
        assert!(WebhookConfig::from_parts("   ".to_string(), None).is_none());

        let cfg = WebhookConfig::from_parts(
            "https://discord.com/api/webhooks/x".to_string(),
            Some(vec!["started".to_string()]),
        )
        .unwrap();
        assert_eq!(cfg.url, "https://discord.com/api/webhooks/x");
        assert_eq!(cfg.events, Some(vec!["started".to_string()]));
    }

    #[test]
    fn test_webhook_config_wants() {
        let all = WebhookConfig {
            url: "u".to_string(),
            events: None,
        };
        assert!(all.wants("started"));
        assert!(all.wants("progress"));
        assert!(all.wants("aborted"));

        let filtered = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["completed".to_string()]),
        };
        assert!(filtered.wants("completed"));
        assert!(!filtered.wants("started"));
        assert!(!filtered.wants("progress"));

        let lifecycle = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["started".to_string(), "completed".to_string()]),
        };
        assert!(lifecycle.wants("progress"));

        let fully_qualified = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["subtask.started".to_string()]),
        };
        assert!(fully_qualified.wants("started"));

        // Regression: depth0 sink emits `tool_call_*`; the stored allow-list uses the
        // canonical status vocabulary. Both sides must normalize to the same token so
        // activity events are not silently dropped before HTTP delivery.
        let activity_legacy = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec![
                "started".to_string(),
                "progress".to_string(),
                "completed".to_string(),
                "failed".to_string(),
                "timed_out".to_string(),
            ]),
        };
        assert!(activity_legacy.wants("tool_call_started"));
        assert!(activity_legacy.wants("tool_call_completed"));
        assert!(activity_legacy.wants("tool_call_failed"));
        // `rejected` is a tool-only status absent from this legacy list, so it stays
        // filtered here; an all-events (None) config delivers it.
        assert!(!activity_legacy.wants("tool_call_rejected"));

        let activity_explicit = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["rejected".to_string(), "tool_call_failed".to_string()]),
        };
        assert!(activity_explicit.wants("tool_call_rejected"));
        assert!(activity_explicit.wants("tool_call_failed"));
        assert!(!activity_explicit.wants("tool_call_started"));
    }

    // ---- webhook URL validation ----

    const VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcdefSECRETtoken";
    const SECRET_TOKEN: &str = "abcdefSECRETtoken";

    #[test]
    fn test_validate_webhook_url_valid() {
        assert!(validate_webhook_url(VALID_URL).is_ok());
        assert!(validate_webhook_url("https://canary.discord.com/api/webhooks/1/tok").is_ok());
        assert!(validate_webhook_url("https://discordapp.com/api/webhooks/1/tok").is_ok());
        assert!(validate_webhook_url("https://ptb.discord.com/api/webhooks/1/tok").is_ok());
    }

    #[test]
    fn test_validate_webhook_url_invalid() {
        assert!(validate_webhook_url("").is_err());
        assert!(validate_webhook_url("   ").is_err());
        assert!(validate_webhook_url("http://discord.com/api/webhooks/1/tok").is_err());
        assert!(validate_webhook_url("https://evil.com/api/webhooks/1/tok").is_err());
        // missing token segment
        assert!(validate_webhook_url("https://discord.com/api/webhooks/123").is_err());
        // wrong path
        assert!(validate_webhook_url("https://discord.com/channels/1/2").is_err());
        // no path
        assert!(validate_webhook_url("https://discord.com").is_err());
        // reason must not leak the raw url
        let reason = validate_webhook_url("https://evil.com/api/webhooks/1/secrettok").unwrap_err();
        assert!(!reason.contains("secrettok"));
    }

    // ---- redaction ----

    #[test]
    fn test_redact_webhook_url_hides_token() {
        let redacted = redact_webhook_url(VALID_URL);
        assert!(!redacted.contains(SECRET_TOKEN), "token leaked: {redacted}");
        assert!(redacted.contains("[redacted]"));
        assert!(redacted.contains("123456789"));
    }

    // ---- resolution ----

    fn insert_row(
        conn: &rusqlite::Connection,
        scope: &str,
        agent_id: &str,
        tool_name: &str,
        kind: &str,
        url: &str,
        enabled: bool,
    ) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: scope.to_string(),
            agent_id: agent_id.to_string(),
            tool_name: tool_name.to_string(),
            kind: kind.to_string(),
            url: url.to_string(),
            events_json: None,
            enabled,
            name: None,
            created_by: Some("owner".to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };
        opencrab_db::queries::upsert_agent_webhook_config(conn, &row).unwrap();
    }

    fn use_source(r: &WebhookResolution) -> WebhookSource {
        match r {
            WebhookResolution::Use { source, .. } => *source,
            _ => panic!("expected Use"),
        }
    }

    #[test]
    fn test_webhook_resolution_explicit_beats_db() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": VALID_URL } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::Explicit);
    }

    #[test]
    fn test_webhook_resolution_tool_beats_agent_beats_global() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "global", "*", "", "subtask", VALID_URL, true);
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        insert_row(
            &conn,
            "tool",
            "a1",
            "spawn_subtask",
            "subtask",
            VALID_URL,
            true,
        );
        let args = json!({});
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::ToolDefault);

        // remove tool -> agent wins
        let conn2 = opencrab_db::init_memory().unwrap();
        insert_row(&conn2, "global", "*", "", "subtask", VALID_URL, true);
        insert_row(&conn2, "agent", "a1", "", "subtask", VALID_URL, true);
        let r2 = resolve_subtask_webhook(&conn2, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r2), WebhookSource::AgentDefault);

        // only global
        let conn3 = opencrab_db::init_memory().unwrap();
        insert_row(&conn3, "global", "*", "", "subtask", VALID_URL, true);
        let r3 = resolve_subtask_webhook(&conn3, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r3), WebhookSource::GlobalDefault);
    }

    #[test]
    fn test_webhook_resolution_db_beats_env_config() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let env = WebhookConfig {
            url: "https://discord.com/api/webhooks/9/envtok".to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_webhook_resolution_env_only_when_no_db_row() {
        let conn = opencrab_db::init_memory().unwrap();
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        assert_eq!(use_source(&r), WebhookSource::EnvConfig);
    }

    #[test]
    fn test_webhook_resolution_none() {
        let conn = opencrab_db::init_memory().unwrap();
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert!(matches!(r, WebhookResolution::None));
    }

    #[test]
    fn test_webhook_resolution_invalid_explicit() {
        let conn = opencrab_db::init_memory().unwrap();
        let args = json!({ "webhook": { "url": "http://evil.com/x" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        match r {
            WebhookResolution::Error { code, source, .. } => {
                assert_eq!(code, "invalid_webhook_url");
                assert_eq!(source, WebhookSource::Explicit);
            }
            _ => panic!("expected Error"),
        }
    }

    // ---- empty / whitespace explicit url falls back to default (not an error) ----

    #[test]
    fn test_webhook_resolution_empty_explicit_url_falls_back_to_db_default() {
        // 明示 webhook の url が空文字なら「指定なし」扱いとし、DB の agent デフォルトへ
        // フォールバックする（Error にして配送をブロックしない）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_webhook_resolution_whitespace_explicit_url_falls_back_to_db_default() {
        // 空白のみの url も「指定なし」扱い。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "   " } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_webhook_resolution_empty_explicit_url_falls_back_to_env_config() {
        // DB 行が無くても、空 url は env/config デフォルトへフォールバックする。
        let conn = opencrab_db::init_memory().unwrap();
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let args = json!({ "webhook": { "url": "" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, Some(&env));
        assert_eq!(use_source(&r), WebhookSource::EnvConfig);
    }

    #[test]
    fn test_webhook_resolution_empty_explicit_url_with_no_default_is_none() {
        // 空 url + デフォルト無し → None（Error ではない）。
        let conn = opencrab_db::init_memory().unwrap();
        let args = json!({ "webhook": { "url": "   " } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert!(matches!(r, WebhookResolution::None));
    }

    #[test]
    fn test_webhook_resolution_empty_explicit_url_keeps_events_ignored_on_fallback() {
        // 空 url のとき explicit events は使われず、フォールバック先（DB）の設定が勝つ。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "", "events": ["completed"] } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        match r {
            WebhookResolution::Use { config, source } => {
                assert_eq!(source, WebhookSource::AgentDefault);
                assert_eq!(config.url, VALID_URL);
                // DB 行は events_json=None なので全イベント送信。
                assert_eq!(config.events, None);
            }
            _ => panic!("expected Use from DB default"),
        }
    }

    #[test]
    fn test_webhook_resolution_nonempty_invalid_explicit_still_errors_over_default() {
        // 非空の不正 url はデフォルトがあっても fall through せず Error（strict 維持）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "http://evil.com/x/secrettok" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        match r {
            WebhookResolution::Error {
                code,
                message,
                source,
            } => {
                assert_eq!(code, "invalid_webhook_url");
                assert_eq!(source, WebhookSource::Explicit);
                // 診断メッセージに raw url/token は漏れない。
                assert!(!message.contains("secrettok"), "token leaked: {message}");
            }
            _ => panic!("expected Error, got fallthrough"),
        }
    }

    #[test]
    fn test_webhook_resolution_invalid_db_default_no_fallthrough() {
        let conn = opencrab_db::init_memory().unwrap();
        // tool default invalid, agent default valid -> must NOT fall through.
        insert_row(
            &conn,
            "tool",
            "a1",
            "spawn_subtask",
            "subtask",
            "http://bad",
            true,
        );
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        match r {
            WebhookResolution::Error { code, source, .. } => {
                assert_eq!(code, "invalid_default_webhook");
                assert_eq!(source, WebhookSource::ToolDefault);
            }
            _ => panic!("expected Error, got fallthrough"),
        }
    }

    #[test]
    fn test_webhook_resolution_disabled_no_fallthrough() {
        let conn = opencrab_db::init_memory().unwrap();
        // tool disabled, agent valid -> Disabled, no fallthrough.
        insert_row(
            &conn,
            "tool",
            "a1",
            "spawn_subtask",
            "subtask",
            VALID_URL,
            false,
        );
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        match r {
            WebhookResolution::Disabled { source } => {
                assert_eq!(source, WebhookSource::ToolDefault);
            }
            _ => panic!("expected Disabled, got fallthrough"),
        }
    }

    #[test]
    fn test_webhook_resolution_lifecycle_alias() {
        let conn = opencrab_db::init_memory().unwrap();
        // only a 'lifecycle' row at agent scope -> resolves like subtask.
        insert_row(&conn, "agent", "a1", "", "lifecycle", VALID_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);

        // lifecycle at tool scope still beats agent-scope subtask row.
        let conn2 = opencrab_db::init_memory().unwrap();
        insert_row(&conn2, "agent", "a1", "", "subtask", VALID_URL, true);
        insert_row(
            &conn2,
            "tool",
            "a1",
            "spawn_subtask",
            "lifecycle",
            VALID_URL,
            true,
        );
        let r2 = resolve_subtask_webhook(&conn2, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r2), WebhookSource::ToolDefault);
    }

    // ---- secret redaction ----

    #[test]
    fn test_redact_secrets_scrubs_known_patterns() {
        let input =
            "key sk-ABCDEFGHIJKLMNOP and ghp_0123456789abcdefghij and AKIAABCDEFGHIJKLMNOP \
                     Authorization: Bearer myreallylongtoken123456 \
                     API_KEY=supersecretvalue \
                     hook https://discord.com/api/webhooks/123/abcdefSECRETtoken \
                     hex 0123456789abcdef0123456789abcdef0123";
        let out = redact_secrets(input);
        assert!(!out.contains("sk-ABCDEFGHIJKLMNOP"), "sk leaked: {out}");
        assert!(
            !out.contains("ghp_0123456789abcdefghij"),
            "ghp leaked: {out}"
        );
        assert!(!out.contains("AKIAABCDEFGHIJKLMNOP"), "akia leaked: {out}");
        assert!(
            !out.contains("myreallylongtoken123456"),
            "bearer leaked: {out}"
        );
        assert!(!out.contains("supersecretvalue"), "kv leaked: {out}");
        assert!(
            !out.contains("abcdefSECRETtoken"),
            "webhook token leaked: {out}"
        );
        assert!(out.contains("[REDACTED]"));
        // benign words preserved
        assert!(out.contains("key"));
        assert!(out.contains("Authorization:"));
    }

    #[test]
    fn test_redact_secrets_kv_value_in_next_token() {
        let out = redact_secrets("\"token\": \"abcdefghijklmnopqrstuvwx\"");
        assert!(
            !out.contains("abcdefghijklmnopqrstuvwx"),
            "value leaked: {out}"
        );
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn test_redact_secrets_idempotent_and_keeps_plain_text() {
        let plain = "hello world exit=0 done";
        assert_eq!(redact_secrets(plain), plain);
        let once = redact_secrets("API_KEY=supersecretvalue");
        let twice = redact_secrets(&once);
        assert_eq!(once, twice);
    }

    // ---- activity-family resolution ----

    #[test]
    fn test_resolve_activity_tool_beats_agent_beats_global() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "global", "*", "", "activity", VALID_URL, true);
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        insert_row(
            &conn,
            "tool",
            "a1",
            "execute_shell",
            "activity",
            VALID_URL,
            true,
        );
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        assert_eq!(use_source(&r), WebhookSource::ToolDefault);

        let conn2 = opencrab_db::init_memory().unwrap();
        insert_row(&conn2, "global", "*", "", "activity", VALID_URL, true);
        insert_row(&conn2, "agent", "a1", "", "activity", VALID_URL, true);
        let r2 = resolve_activity_webhook(&conn2, "a1", "execute_shell");
        assert_eq!(use_source(&r2), WebhookSource::AgentDefault);

        let conn3 = opencrab_db::init_memory().unwrap();
        insert_row(&conn3, "global", "*", "", "activity", VALID_URL, true);
        let r3 = resolve_activity_webhook(&conn3, "a1", "execute_shell");
        assert_eq!(use_source(&r3), WebhookSource::GlobalDefault);
    }

    #[test]
    fn test_resolve_activity_ignores_subtask_kind_and_has_no_env() {
        let conn = opencrab_db::init_memory().unwrap();
        // only a subtask-kind agent row exists -> activity resolution must NOT use it.
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        assert!(
            matches!(r, WebhookResolution::None),
            "subtask kind must not serve activity"
        );
    }

    #[test]
    fn test_resolve_activity_disabled_no_fallthrough() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(
            &conn,
            "tool",
            "a1",
            "execute_shell",
            "activity",
            VALID_URL,
            false,
        );
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        assert!(matches!(
            r,
            WebhookResolution::Disabled {
                source: WebhookSource::ToolDefault
            }
        ));
    }

    #[test]
    fn test_resolve_activity_invalid_db_default_errors() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", "http://bad", true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        match r {
            WebhookResolution::Error { code, source, .. } => {
                assert_eq!(code, "invalid_default_webhook");
                assert_eq!(source, WebhookSource::AgentDefault);
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_resolve_activity_also_serves_subtask_lifecycle() {
        // An agent 'activity' default should also be picked up by resolve_subtask_webhook
        // (activity family includes subtask lifecycle).
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_resolve_subtask_prefers_explicit_subtask_over_activity_same_scope() {
        // L3: 同一 scope に subtask 専用行と汎用 activity 行が両方あるとき、subtask 通知は
        // 明示的な subtask 専用デフォルトへ送る（activity に奪われない）。
        const SUBTASK_URL: &str = "https://discord.com/api/webhooks/111/subtasktoken";
        const ACTIVITY_URL: &str = "https://discord.com/api/webhooks/222/activitytoken";
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", ACTIVITY_URL, true);
        insert_row(&conn, "agent", "a1", "", "subtask", SUBTASK_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        match r {
            WebhookResolution::Use { config, source } => {
                assert_eq!(source, WebhookSource::AgentDefault);
                assert_eq!(
                    config.url, SUBTASK_URL,
                    "subtask-specific default must win over generic activity"
                );
            }
            _ => panic!("expected Use"),
        }
    }

    #[test]
    fn test_resolve_subtask_falls_back_to_activity_when_no_subtask_row() {
        // subtask 専用行が無ければ activity 行へフォールバックする（family 包含）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    // ---- delivery failure recording ----

    #[test]
    fn test_record_webhook_delivery_failure_writes_redacted_log() {
        let conn = opencrab_db::init_memory().unwrap();

        let redacted = redact_webhook_url(VALID_URL);
        record_webhook_delivery_failure(
            &conn,
            "a1",
            "parent-sess",
            "st1",
            "subtask-st1",
            &redacted,
            "http 500",
        );

        let logs =
            opencrab_db::queries::list_session_logs_by_session(&conn, "parent-sess").unwrap();
        let found = logs
            .iter()
            .find(|l| l.content.contains("delivery_failed"))
            .expect("delivery_failed log should exist");
        assert!(found.content.contains("[redacted]"));
        assert!(
            !found.content.contains(SECRET_TOKEN),
            "raw token leaked into log: {}",
            found.content
        );

        // empty parent_session_id -> no-op
        record_webhook_delivery_failure(&conn, "a1", "", "st1", "s", &redacted, "x");
    }

    // ---- Nostr 受信 → Discord 転記先の解決（#252 段階 A） ----

    fn set_relay(conn: &rusqlite::Connection, agent_id: &str, enabled: bool, url: Option<&str>) {
        opencrab_db::queries::upsert_agent_nostr_relay_config(
            conn,
            &opencrab_db::queries::AgentNostrRelayConfigRow {
                agent_id: agent_id.to_string(),
                enabled,
                webhook_url: url.map(|s| s.to_string()),
            },
        )
        .unwrap();
    }

    /// fail-closed: 未設定 / 無効 / URL 欠落・不正 はすべて「転記しない（None）」。
    #[test]
    fn test_resolve_nostr_relay_is_fail_closed() {
        let conn = opencrab_db::init_memory().unwrap();

        // 1. 行が無い → None。
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());

        // 2. enabled=false（URL はあっても無効なら転記しない）。
        set_relay(&conn, "a1", false, Some(VALID_URL));
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());

        // 3. enabled だが URL が NULL。
        set_relay(&conn, "a1", true, None);
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());

        // 4. enabled だが URL が空白のみ。
        set_relay(&conn, "a1", true, Some("   "));
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());

        // 5. enabled だが Discord webhook として不正な URL。
        set_relay(&conn, "a1", true, Some("http://evil.com/x/tok"));
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());
    }

    /// 有効かつ URL が妥当なら、その宛先を全イベント（events=None）で返す。
    #[test]
    fn test_resolve_nostr_relay_returns_target_when_enabled_and_valid() {
        let conn = opencrab_db::init_memory().unwrap();
        set_relay(&conn, "a1", true, Some(VALID_URL));
        let cfg = resolve_nostr_relay_webhook(&conn, "a1").expect("有効なら宛先を返す");
        assert_eq!(cfg.url, VALID_URL);
        assert_eq!(cfg.events, None, "転記は種別で間引かない");

        // 前後空白は trim される。
        set_relay(
            &conn,
            "a2",
            true,
            Some("  https://discord.com/api/webhooks/9/tok9  "),
        );
        let cfg = resolve_nostr_relay_webhook(&conn, "a2").unwrap();
        assert_eq!(cfg.url, "https://discord.com/api/webhooks/9/tok9");

        // per-agent: 別エージェントは設定を共有しない。
        assert!(resolve_nostr_relay_webhook(&conn, "a3").is_none());
    }
}
