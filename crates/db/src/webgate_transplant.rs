//! 既存 web セッション → `extgate-{binding_id}` への 14 store 一回移送。
//! 件数/digest 不一致は中止。V3 wire は触らない。

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const WEBGATE_MARKER: &str = "webgate_v3_session_id";

const STORES: &[Store] = &[
    Store::new(
        "memory_sessions",
        "session_id",
        "id",
        &["id", "agent_id", "log_type", "content", "speaker_id", "turn_number", "metadata_json", "created_at"],
    ),
    Store::new(
        "memory_sessions_fts",
        "session_id",
        "rowid",
        &["rowid", "content", "agent_id", "log_type"],
    ),
    Store::new(
        "agent_sessions",
        "session_id",
        "agent_id",
        &["agent_id", "last_speech_at", "done_declared"],
    ),
    Store::new(
        "session_heartbeat_config",
        "session_id",
        "agent_id",
        &["agent_id", "enabled", "interval_secs", "anchor_at", "last_fired_at", "updated_at"],
    ),
    Store::new(
        "agent_schedules",
        "session_id",
        "id",
        &["id", "agent_id", "cron_expr", "timezone", "message", "enabled", "anchor_at", "last_fired_at", "created_at", "updated_at"],
    ),
    Store::new(
        "skill_usage_log",
        "session_id",
        "id",
        &["id", "agent_id", "skill_id", "created_at"],
    ),
    Store::new(
        "task_ledger",
        "session_id",
        "id",
        &["id", "agent_id", "goal", "contract", "status", "created_at", "updated_at"],
    ),
    Store::new(
        "session_watches",
        "session_id",
        "id",
        &["id", "agent_id", "interval_secs", "filter_json", "created_at"],
    ),
    Store::new(
        "tool_logs",
        "session_id",
        "id",
        &["id", "agent_id", "tool_name", "args_json", "outcome", "result_text", "started_at", "created_at", "latency_ms", "iteration"],
    ),
    Store::new(
        "impressions",
        "session_id",
        "id",
        &["id", "agent_id", "target_id", "target_name", "personality", "communication_style", "recent_behavior", "agreement", "notes", "last_updated_turn", "created_at", "updated_at"],
    ),
    Store::new(
        "llm_usage_metrics",
        "session_id",
        "id",
        &["id", "agent_id", "timestamp", "provider", "model", "purpose", "task_type", "complexity", "input_tokens", "output_tokens", "total_tokens", "estimated_cost_usd", "latency_ms", "time_to_first_token_ms", "quality_score", "self_evaluation", "task_success", "would_use_again", "better_model_suggestion", "tags", "created_at"],
    ),
    Store::new(
        "llm_logs",
        "session_id",
        "id",
        &["id", "agent_id", "model", "prompt", "response", "tool_calls", "latency_ms", "prompt_tokens", "completion_tokens", "total_tokens", "error_code", "error_body", "requested_at", "trigger_message_id", "is_bot_iteration", "cache_read_tokens", "cache_creation_tokens", "created_at"],
    ),
    Store::new(
        "pending_interactions",
        "session_id",
        "id",
        &["id", "agent_id", "channel_id", "message_id", "platform", "surface_id", "a2ui_components_json", "status", "response_json", "responder_id", "owner_only", "timeout_secs", "created_at", "responded_at", "updated_at"],
    ),
    Store::new(
        "memory_index_nodes",
        "source_session_id",
        "id",
        &["id", "agent_id", "parent_id", "node_type", "source_type", "title", "summary", "start_log_id", "end_log_id", "date_from", "date_to", "depth", "child_count", "token_count", "created_at", "updated_at", "short_id"],
    ),
];

struct Store {
    table: &'static str,
    session_col: &'static str,
    pk: &'static str,
    digest_cols: &'static [&'static str],
}

impl Store {
    const fn new(
        table: &'static str,
        session_col: &'static str,
        pk: &'static str,
        digest_cols: &'static [&'static str],
    ) -> Self {
        Self {
            table,
            session_col,
            pk,
            digest_cols,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSnap {
    pub count: i64,
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct Mapping {
    pub logical: String,
    pub physical: String,
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransplantOutcome {
    Migrated,
    WriteZero,
}

pub fn session_id_for_binding(binding_id: &str) -> String {
    format!("extgate-{binding_id}")
}

/// 開いている web binding から logical/physical を列挙する。
pub fn list_web_mappings(conn: &Connection) -> Result<Vec<Mapping>> {
    let mut stmt = conn.prepare(
        "SELECT b.binding_id, b.address, a.agent_id
         FROM gate_bindings b
         JOIN gate_instances i ON i.instance_id = b.instance_id
         JOIN agents a ON a.subject_id = i.subject_id
         WHERE b.closed_at IS NULL AND i.deleted_at IS NULL AND i.kind_id = 'web'
         ORDER BY b.binding_id",
    )?;
    let rows = stmt.query_map([], |r| {
        let binding_id: String = r.get(0)?;
        Ok(Mapping {
            logical: r.get(1)?,
            physical: session_id_for_binding(&binding_id),
            agent_id: r.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn validate_legacy_session(conn: &Connection, m: &Mapping) -> Result<()> {
    let prefix = format!("web-{}-", m.agent_id);
    if !m.logical.starts_with(&prefix) || m.logical.len() <= prefix.len() {
        bail!("logical session {} is not web-{}-*", m.logical, m.agent_id);
    }
    let agents: Vec<String> = conn
        .prepare("SELECT agent_id FROM agents ORDER BY agent_id")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let matches: Vec<_> = agents
        .iter()
        .filter(|a| m.logical.starts_with(&format!("web-{a}-")))
        .collect();
    if matches.len() != 1 {
        bail!("prefix matches {} agents for {}", matches.len(), m.logical);
    }
    if matches[0] != &m.agent_id {
        bail!("prefix agent {} != binding subject {}", matches[0], m.agent_id);
    }
    let participants: Vec<String> = crate::queries::list_session_participants(conn, &m.logical)?;
    if participants.len() != 1 {
        bail!(
            "legacy session {} has {} participants",
            m.logical,
            participants.len()
        );
    }
    if participants[0] != m.agent_id {
        bail!(
            "legacy sole participant {} != {}",
            participants[0],
            m.agent_id
        );
    }
    Ok(())
}

pub fn snapshot_session(conn: &Connection, session_id: &str) -> Result<BTreeMap<String, StoreSnap>> {
    let mut map = BTreeMap::new();
    for store in STORES {
        map.insert(store.table.to_string(), snap_store(conn, store, session_id)?);
    }
    Ok(map)
}

fn snap_store(conn: &Connection, store: &Store, session_id: &str) -> Result<StoreSnap> {
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM {} WHERE {} = ?1",
            store.table, store.session_col
        ),
        [session_id],
        |r| r.get(0),
    )?;
    let cols = store.digest_cols.join(", ");
    let sql = format!(
        "SELECT {cols} FROM {} WHERE {} = ?1 ORDER BY {}",
        store.table, store.session_col, store.pk
    );
    let mut stmt = conn.prepare(&sql)?;
    let col_count = store.digest_cols.len();
    let mut hasher = Sha256::new();
    let mut rows = stmt.query([session_id])?;
    while let Some(row) = rows.next()? {
        for i in 0..col_count {
            let v: Option<String> = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(n) => Some(n.to_string()),
                rusqlite::types::ValueRef::Real(n) => Some(n.to_string()),
                rusqlite::types::ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
                rusqlite::types::ValueRef::Blob(b) => Some(hex_lower(b)),
            };
            hasher.update(v.as_deref().unwrap_or("").as_bytes());
            hasher.update([0u8]);
        }
        hasher.update([0xff]);
    }
    Ok(StoreSnap {
        count,
        digest: hex_lower(&hasher.finalize()),
    })
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn marker_present(conn: &Connection, logical: &str, physical: &str) -> Result<bool> {
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT metadata_json FROM sessions WHERE id = ?1",
            [logical],
            |r| r.get(0),
        )
        .optional()?;
    let Some(Some(raw)) = raw else {
        return Ok(false);
    };
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    Ok(v.get(WEBGATE_MARKER).and_then(|x| x.as_str()) == Some(physical))
}

fn legacy_refs(conn: &Connection, logical: &str) -> Result<i64> {
    let mut total = 0i64;
    for store in STORES {
        let n: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM {} WHERE {} = ?1",
                store.table, store.session_col
            ),
            [logical],
            |r| r.get(0),
        )?;
        total += n;
    }
    Ok(total)
}

pub fn transplant_mapping(conn: &Connection, m: &Mapping) -> Result<TransplantOutcome> {
    let already = marker_present(conn, &m.logical, &m.physical)?;
    if already {
        if legacy_refs(conn, &m.logical)? != 0 {
            bail!("re-run: alias present but legacy refs remain");
        }
        return Ok(TransplantOutcome::WriteZero);
    }
    if legacy_refs(conn, &m.logical)? == 0 {
        write_alias_marker(conn, m)?;
        return Ok(TransplantOutcome::WriteZero);
    }
    validate_legacy_session(conn, m)?;

    let before_legacy = snapshot_session(conn, &m.logical)?;
    let before_physical = snapshot_session(conn, &m.physical)?;
    let tx = conn.unchecked_transaction()?;
    migrate_agent_sessions(&tx, m)?;
    for store in STORES {
        if store.table == "agent_sessions" {
            continue;
        }
        if store.table == "memory_sessions_fts" {
            continue;
        }
        tx.execute(
            &format!(
                "UPDATE {} SET {} = ?1 WHERE {} = ?2",
                store.table, store.session_col, store.session_col
            ),
            params![m.physical, m.logical],
        )?;
    }
    tx.execute(
        "UPDATE memory_sessions_fts SET session_id = ?1 WHERE rowid IN (
            SELECT id FROM memory_sessions WHERE session_id = ?1
         ) AND session_id = ?2",
        params![m.physical, m.logical],
    )?;
    write_alias_marker_in_tx(&tx, m)?;
    tx.commit()?;

    let after_legacy = snapshot_session(conn, &m.logical)?;
    let after_physical = snapshot_session(conn, &m.physical)?;
    for store in STORES {
        if store.table == "agent_sessions" {
            continue;
        }
        let after_l = &after_legacy[store.table];
        if after_l.count != 0 {
            bail!("{} still has legacy refs after transplant", store.table);
        }
        let after_p = &after_physical[store.table];
        let before_total = before_legacy[store.table].count + before_physical[store.table].count;
        if after_p.count != before_total {
            bail!(
                "{} count changed: before {before_total} after {}",
                store.table,
                after_p.count
            );
        }
        if store.table != "memory_sessions_fts"
            && before_legacy[store.table].count > 0
            && before_physical[store.table].count == 0
            && after_p.digest != before_legacy[store.table].digest
        {
            bail!("{} digest mismatch", store.table);
        }
    }

    let logs: Vec<i64> = conn
        .prepare("SELECT id FROM memory_sessions WHERE session_id = ?1 ORDER BY id ASC")?
        .query_map([&m.physical], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut sorted = logs.clone();
    sorted.sort_unstable();
    if logs != sorted {
        bail!("physical logs are not id ASC");
    }
    Ok(TransplantOutcome::Migrated)
}

fn migrate_agent_sessions(tx: &rusqlite::Transaction<'_>, m: &Mapping) -> Result<()> {
    let legacy: Vec<(String, Option<String>, i64)> = tx
        .prepare("SELECT agent_id, last_speech_at, done_declared FROM agent_sessions WHERE session_id = ?1")?
        .query_map([&m.logical], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let physical: Vec<(String, Option<String>, i64)> = tx
        .prepare("SELECT agent_id, last_speech_at, done_declared FROM agent_sessions WHERE session_id = ?1")?
        .query_map([&m.physical], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if legacy.len() > 1 || physical.len() > 1 {
        bail!("agent_sessions has multiple memberships");
    }
    if let Some((agent, _, _)) = legacy.first() {
        if agent != &m.agent_id {
            bail!("agent_sessions agent {} != {}", agent, m.agent_id);
        }
    }
    if let Some((agent, _, _)) = physical.first() {
        if agent != &m.agent_id {
            bail!("physical agent_sessions agent {} != {}", agent, m.agent_id);
        }
    }
    let last = stronger_time(
        legacy.first().and_then(|r| r.1.clone()),
        physical.first().and_then(|r| r.1.clone()),
    );
    let done = i64::from(
        legacy.first().is_some_and(|r| r.2 != 0) || physical.first().is_some_and(|r| r.2 != 0),
    );
    tx.execute(
        "DELETE FROM agent_sessions WHERE session_id = ?1",
        [&m.logical],
    )?;
    if physical.is_empty() {
        tx.execute(
            "INSERT INTO agent_sessions (agent_id, session_id, last_speech_at, done_declared)
             VALUES (?1, ?2, ?3, ?4)",
            params![m.agent_id, m.physical, last, done],
        )?;
    } else {
        tx.execute(
            "UPDATE agent_sessions SET last_speech_at = ?1, done_declared = ?2
             WHERE agent_id = ?3 AND session_id = ?4",
            params![last, done, m.agent_id, m.physical],
        )?;
    }
    Ok(())
}

fn stronger_time(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(x), Some(y)) => Some(if x >= y { x } else { y }),
    }
}

fn write_alias_marker(conn: &Connection, m: &Mapping) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    write_alias_marker_in_tx(&tx, m)?;
    tx.commit()?;
    Ok(())
}

fn write_alias_marker_in_tx(tx: &rusqlite::Transaction<'_>, m: &Mapping) -> Result<()> {
    let raw: Option<Option<String>> = tx
        .query_row(
            "SELECT metadata_json FROM sessions WHERE id = ?1",
            [&m.logical],
            |r| r.get(0),
        )
        .optional()?;
    let raw = raw.flatten();
    let mut obj = match raw.as_deref() {
        Some(s) if !s.is_empty() => serde_json::from_str(s)?,
        _ => serde_json::json!({}),
    };
    let Some(map) = obj.as_object_mut() else {
        bail!("legacy metadata_json is not an object");
    };
    map.insert(
        WEBGATE_MARKER.to_string(),
        serde_json::Value::String(m.physical.clone()),
    );
    let encoded = serde_json::to_string(&obj)?;
    tx.execute(
        "UPDATE sessions SET metadata_json = ?1 WHERE id = ?2",
        rusqlite::params![encoded, m.logical],
    )?;
    Ok(())
}

/// 設計 §3.3: mapping ごとに 1 TX で 14 store を移す。
pub fn transplant_all(conn: &Connection) -> Result<Vec<(String, TransplantOutcome)>> {
    let mappings = list_web_mappings(conn)?;
    let mut out = Vec::new();
    for m in mappings {
        let r = transplant_mapping(conn, &m)?;
        out.push((m.logical, r));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::{insert_session, insert_session_log, SessionLogRow, SessionRow};
    use crate::init_memory;

    fn agent(conn: &Connection, id: &str) {
        crate::queries::upsert_agent(
            conn,
            &crate::queries::AgentRow {
                agent_id: id.into(),
                name: id.into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "p".into(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }

    fn put_web_binding(conn: &Connection, agent_id: &str, logical: &str, binding_id: &str) {
        let subject: i64 = conn
            .query_row(
                "SELECT subject_id FROM agents WHERE agent_id = ?1",
                [agent_id],
                |r| r.get(0),
            )
            .unwrap();
        let instance = format!("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaa{subject:02}");
        let now = 1i64;
        conn.execute(
            "INSERT OR IGNORE INTO gate_instances
             (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
             VALUES (?1, 'web', ?2, 1, 1, 'e30=', '0000000000000000000000000000000000000000000000000000000000000000', ?3, ?3)",
            params![instance, subject, now],
        )
        .unwrap();
        let physical = session_id_for_binding(binding_id);
        let ts = chrono::Utc::now().to_rfc3339();
        crate::queries::insert_session_in_tx(
            &conn.unchecked_transaction().unwrap(),
            &physical,
            logical,
            &ts,
        )
        .ok();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (id, theme, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            params![physical, logical, ts],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agent_sessions (agent_id, session_id) VALUES (?1, ?2)",
            params![agent_id, physical],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![binding_id, instance, logical, now],
        )
        .unwrap();
    }

    fn legacy_session(conn: &Connection, id: &str, agent_id: &str) {
        insert_session(
            conn,
            &SessionRow {
                id: id.into(),
                mode: "web".into(),
                theme: id.into(),
                phase: "divergent".into(),
                turn_number: 0,
                status: "active".into(),
                participant_ids_json: serde_json::to_string(&vec![agent_id]).unwrap(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: Some(r#"{"keep":true}"#.into()),
            },
        )
        .unwrap();
    }

    fn speech(conn: &Connection, agent_id: &str, session: &str, text: &str) {
        insert_session_log(
            conn,
            &SessionLogRow {
                id: None,
                agent_id: agent_id.into(),
                session_id: session.into(),
                log_type: "speech".into(),
                content: text.into(),
                speaker_id: Some(agent_id.into()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn transplant_zero_one_and_rerun() {
        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        let logical = "web-a1-c1";
        legacy_session(&conn, logical, "a1");
        put_web_binding(
            &conn,
            "a1",
            logical,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        );
        let m = list_web_mappings(&conn).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(
            transplant_mapping(&conn, &m[0]).unwrap(),
            TransplantOutcome::Migrated
        );
        assert_eq!(
            transplant_mapping(&conn, &m[0]).unwrap(),
            TransplantOutcome::WriteZero
        );

        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        let logical = "web-a1-c1";
        legacy_session(&conn, logical, "a1");
        put_web_binding(
            &conn,
            "a1",
            logical,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        );
        speech(&conn, "a1", logical, "hello");
        let m = list_web_mappings(&conn).unwrap();
        let before = snapshot_session(&conn, logical).unwrap();
        assert_eq!(before["memory_sessions"].count, 1);
        assert_eq!(
            transplant_mapping(&conn, &m[0]).unwrap(),
            TransplantOutcome::Migrated
        );
        let after_l = snapshot_session(&conn, logical).unwrap();
        let after_p = snapshot_session(&conn, &m[0].physical).unwrap();
        assert_eq!(after_l["memory_sessions"].count, 0);
        assert_eq!(after_p["memory_sessions"].count, 1);
        assert_eq!(
            after_p["memory_sessions"].digest,
            before["memory_sessions"].digest
        );
        let fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_sessions_fts WHERE session_id = ?1",
                [&m[0].physical],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts, 1);
        let meta: String = conn
            .query_row(
                "SELECT metadata_json FROM sessions WHERE id = ?1",
                [logical],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&meta).unwrap();
        assert_eq!(v["keep"], true);
        assert_eq!(v[WEBGATE_MARKER], m[0].physical);
        assert_eq!(
            transplant_mapping(&conn, &m[0]).unwrap(),
            TransplantOutcome::WriteZero
        );
    }

    #[test]
    fn transplant_rejects_prefix_mismatch() {
        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        agent(&conn, "a1x");
        let logical = "web-a1x-c";
        legacy_session(&conn, logical, "a1x");
        put_web_binding(
            &conn,
            "a1",
            logical,
            "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
        );
        let m = list_web_mappings(&conn).unwrap();
        assert!(transplant_mapping(&conn, &m[0]).is_err());
    }

    #[test]
    fn transplant_rejects_multiple_participants() {
        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        agent(&conn, "a2");
        let logical = "web-a1-c2";
        insert_session(
            &conn,
            &SessionRow {
                id: logical.into(),
                mode: "web".into(),
                theme: logical.into(),
                phase: "divergent".into(),
                turn_number: 0,
                status: "active".into(),
                participant_ids_json: r#"["a1","a2"]"#.into(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            },
        )
        .unwrap();
        put_web_binding(
            &conn,
            "a1",
            logical,
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        );
        let m = list_web_mappings(&conn).unwrap();
        assert!(transplant_mapping(&conn, &m[0]).is_err());
    }

    #[test]
    fn transplant_ten_thousand_logs_preserve_order() {
        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        let logical = "web-a1-big";
        legacy_session(&conn, logical, "a1");
        put_web_binding(
            &conn,
            "a1",
            logical,
            "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        );
        for i in 0..10_000 {
            speech(&conn, "a1", logical, &format!("m{i}"));
        }
        let before = snapshot_session(&conn, logical).unwrap();
        assert_eq!(before["memory_sessions"].count, 10_000);
        let m = list_web_mappings(&conn).unwrap();
        transplant_mapping(&conn, &m[0]).unwrap();
        let after = snapshot_session(&conn, &m[0].physical).unwrap();
        assert_eq!(after["memory_sessions"].count, 10_000);
        assert_eq!(
            after["memory_sessions"].digest,
            before["memory_sessions"].digest
        );
        let first: String = conn
            .query_row(
                "SELECT content FROM memory_sessions WHERE session_id = ?1 ORDER BY id ASC LIMIT 1",
                [&m[0].physical],
                |r| r.get(0),
            )
            .unwrap();
        let last: String = conn
            .query_row(
                "SELECT content FROM memory_sessions WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
                [&m[0].physical],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(first, "m0");
        assert_eq!(last, "m9999");
    }
}
