use opencrab_actions::TranscriptSource;
use opencrab_db::queries::{
    insert_session_log, is_trusted_user, SessionLogRow, TRUSTED_PLATFORM_EXTGATE,
};
use rusqlite::{params, Connection, Transaction};

use crate::error::GateError;
use crate::protocol::Said;
use crate::registry::ExtgateState;

use super::binding::OriginRow;
use super::nostr_profile::parse_v1_reply_to;

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

pub(super) fn record_inbound(
    tx: &Transaction<'_>,
    session_id: &str,
    row: &OriginRow,
    said: &Said,
    content: &str,
) -> Result<(), GateError> {
    let mut meta = serde_json::json!({
        "source": TranscriptSource::External.inbound(),
        "user_name": "",
        "channel_id": row.address,
    });
    if !said.attachments.is_empty() {
        meta["image_urls"] = serde_json::json!(said.attachments);
    }
    // external_origin は platform 非依存の汎用 field（§9A の e番号採番・長文切り詰めの源）。
    // 全 gateway kind で記録する。旧実装は `kind_id == "nostr"` に閉じていたが、これは汎用採番機構
    // （core conversation.rs は origin 文字列だけを見て platform を解釈しない）へ platform 名が
    // 漏れた既存バグであり、DI 原則（core に個別 gateway 語彙を持ち込まない）に反する。剥がして
    // Discord 等の全 kind で e番号が付くようにする（統括裁定 2026-08-31）。
    meta["external_origin"] = serde_json::json!(said.origin);
    // 返信/リアクション/リポストの対象 event_id を記録（row295c 6b）。会話表示が
    // `(reply→e番号)` を解決するのに使う。旧行は未記録＝表示側が `→外部` フォールバック。
    // 対象抽出は Nostr の V1 アンカー本文に依存する platform 固有処理なのでガード内に残す。
    if row.kind_id == "nostr" {
        if let Some(reply_to) = parse_v1_reply_to(&said.text) {
            meta["reply_target"] = serde_json::json!(reply_to);
        }
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
    .map_err(|e| GateError::store_logged("said.session_log_insert", e))?;
    Ok(())
}

/// owner 一致または trusted_users。query failure は false。
pub fn dm_allowed(conn: &Connection, sender: &str, agent_id: &str, owner_id: &str) -> bool {
    if opencrab_core::owner::is_owner_id(owner_id, sender) {
        return true;
    }
    is_trusted_user(conn, TRUSTED_PLATFORM_EXTGATE, sender, agent_id)
}

/// 当該 agent/instance/address の open binding exact 1 行だけ true。
pub fn channel_whitelisted(
    conn: &Connection,
    agent_id: &str,
    instance_id: &str,
    address: &str,
) -> bool {
    let result = conn.query_row(
        "SELECT COUNT(*) FROM gate_bindings b
         JOIN gate_instances i ON i.instance_id = b.instance_id
         JOIN agents a ON a.subject_id = i.subject_id
         WHERE a.agent_id = ?1 AND b.instance_id = ?2 AND b.address = ?3
           AND b.closed_at IS NULL AND i.deleted_at IS NULL",
        params![agent_id, instance_id, address],
        |r| r.get::<_, i64>(0),
    );
    matches!(result, Ok(1))
}
