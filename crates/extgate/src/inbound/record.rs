use opencrab_actions::TranscriptSource;
use opencrab_db::queries::{insert_session_log, SessionLogRow};
use rusqlite::{params, Connection, Transaction};

use crate::error::GateError;
use crate::protocol::{Said, SaidAttachment, SaidCaller};
use crate::registry::ExtgateState;
use opencrab_core::conversation::OWNER_SPEAKER_METADATA;

use super::session::OriginRow;

pub(super) fn existing_seq(
    tx: &Transaction<'_>,
    binding_id: &str,
    origin: &str,
) -> Result<Option<i64>, GateError> {
    match tx.query_row(
        "SELECT seq FROM external_origins WHERE binding_id = ?1 AND origin = ?2",
        params![binding_id, origin],
        |r| r.get(0),
    ) {
        Ok(seq) => Ok(Some(seq)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(_) => Err(GateError::store()),
    }
}

pub(super) fn next_seq(tx: &Transaction<'_>, binding_id: &str) -> Result<i64, GateError> {
    tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM external_origins WHERE binding_id = ?1",
        params![binding_id],
        |r| r.get(0),
    )
    .map_err(|e| GateError::store_logged("said.next_seq", e))
}

/// #933: (binding_id, origin) の external_origins.seq を read-only で引く（畳み込み高水位の記録用）。
/// read state の付与時に「畳み込んだ said の seq」を得て `mark_folded_seq` へ渡す。無ければ None。
pub(crate) fn seq_for_origin(state: &ExtgateState, binding_id: &str, origin: &str) -> Option<i64> {
    let conn = state.db.lock().ok()?;
    conn.query_row(
        "SELECT seq FROM external_origins WHERE binding_id = ?1 AND origin = ?2",
        params![binding_id, origin],
        |r| r.get::<_, i64>(0),
    )
    .ok()
}

/// The metadata every inbound speech row starts with. Shared with the provisional run's
/// not-yet-recorded user line (D-1085).
pub(crate) fn inbound_speech_metadata(
    session_key: &str,
    author_label: Option<&str>,
    caller: &SaidCaller,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "source": TranscriptSource::new("external", "external_response").inbound(),
        "user_name": author_label.unwrap_or(""),
        // D-1099: the gateway's opaque key of this session (older rows used another key name).
        "session_key": session_key,
    });
    // D-1056: オーナーの発言は会話履歴の話者表示（`uN|name|owner`）で示す。記録時点の caller で
    // 固定し、過去行の表示が後から変わらないようにする（プロンプトキャッシュの prefix 安定）。
    if matches!(caller, SaidCaller::Owner) {
        meta[OWNER_SPEAKER_METADATA] = serde_json::json!(true);
    }
    meta
}

/// Records the said as inbound speech and returns the row id.
pub(super) fn record_inbound(
    tx: &Transaction<'_>,
    session_id: &str,
    row: &OriginRow,
    said: &Said,
    content: &str,
) -> Result<i64, GateError> {
    let mut meta =
        inbound_speech_metadata(&row.session_key, said.author_label.as_deref(), &said.caller);
    let image_urls: Vec<String> = said
        .image_urls()
        .into_iter()
        .filter(|url| url.starts_with("https://"))
        .collect();
    if !image_urls.is_empty() {
        meta["image_urls"] = serde_json::json!(image_urls);
    }
    let local_attachments: Vec<serde_json::Value> = said
        .attachments
        .iter()
        .filter_map(|attachment| match attachment {
            SaidAttachment::LocalFile {
                id,
                name,
                media_type,
                size,
                sha256,
                ..
            } => Some(serde_json::json!({
                "id": id,
                "name": name,
                "media_type": media_type,
                "size": size,
                "sha256": sha256,
                "storage_key": format!("store/{sha256}.bin"),
            })),
            SaidAttachment::ImageUrl(_) => None,
        })
        .collect();
    if !local_attachments.is_empty() {
        meta["attachments"] = serde_json::Value::Array(local_attachments);
    }
    // external_origin は platform 非依存の汎用 field（§9A の e番号採番・長文切り詰めの源）。
    // 全 gateway kind で記録する。旧実装は `kind_id == "nostr"` に閉じていたが、これは汎用採番機構
    // （core conversation.rs は origin 文字列だけを見て platform を解釈しない）へ platform 名が
    // 漏れた既存バグであり、DI 原則（core に個別 gateway 語彙を持ち込まない）に反する。剥がして
    // Discord 等の全 kind で e番号が付くようにする（統括裁定 2026-08-31）。
    meta["external_origin"] = serde_json::json!(said.origin);
    // D-1099: the opaque key other messages use to point at this one (`reply_target`). Core
    // matches it as given and never parses `origin`.
    if let Some(reference) = &said.reference {
        meta["external_ref"] = serde_json::json!(reference);
    }
    // Gatewayが供給した外部reply参照はopaque値として保存し、coreでは解釈しない。
    if let Some(reply_target) = &said.reply_target {
        meta["reply_target"] = serde_json::json!(reply_target);
    }
    insert_session_log(
        tx,
        &SessionLogRow {
            id: None,
            agent_id: row.agent_id.clone(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(said.author_id.clone()),
            turn_number: None,
            metadata_json: Some(meta.to_string()),
            created_at: None,
        },
    )
    .map_err(|e| GateError::store_logged("said.session_log_insert", e))
}

/// D-1085: the agent's own speech row of an agent prelude (same shape as a delivered reply).
pub(crate) fn agent_prelude_row(session_id: &str, agent_id: &str, prelude: &str) -> SessionLogRow {
    SessionLogRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        log_type: "speech".to_string(),
        content: prelude.to_string(),
        speaker_id: Some(agent_id.to_string()),
        turn_number: None,
        metadata_json: Some(
            serde_json::json!({
                "source": TranscriptSource::new("external", "external_response").reply()
            })
            .to_string(),
        ),
        created_at: None,
    }
}

/// D-1085: records `said.agent_prelude` as the agent's own speech and returns the row id.
/// Called right after [`record_inbound`] in the same transaction.
pub fn record_agent_prelude(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
    prelude: &str,
) -> Result<i64, GateError> {
    insert_session_log(conn, &agent_prelude_row(session_id, agent_id, prelude))
        .map_err(|e| GateError::store_logged("said.agent_prelude_insert", e))
}

