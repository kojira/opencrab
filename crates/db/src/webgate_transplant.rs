//! 既存 web セッション → `extgate-{binding_id}` への 14 store 一回移送。
//! 件数/digest 不一致は中止。V3 wire は触らない。

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const WEBGATE_MARKER: &str = "webgate_v3_session_id";
pub const WEBGATE_INVENTORY: &str = "webgate_v3_inventory";

const STORES: &[Store] = &[
    Store::new(
        "memory_sessions",
        "session_id",
        "id",
        &[
            "id",
            "agent_id",
            "log_type",
            "content",
            "speaker_id",
            "turn_number",
            "metadata_json",
            "created_at",
        ],
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
        &[
            "agent_id",
            "enabled",
            "interval_secs",
            "anchor_at",
            "last_fired_at",
            "updated_at",
        ],
    ),
    Store::new(
        "agent_schedules",
        "session_id",
        "id",
        &[
            "id",
            "agent_id",
            "cron_expr",
            "timezone",
            "message",
            "enabled",
            "anchor_at",
            "last_fired_at",
            "created_at",
            "updated_at",
        ],
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
        &[
            "id",
            "agent_id",
            "goal",
            "contract",
            "status",
            "created_at",
            "updated_at",
        ],
    ),
    Store::new(
        "session_watches",
        "session_id",
        "id",
        &[
            "id",
            "agent_id",
            "interval_secs",
            "filter_json",
            "created_at",
        ],
    ),
    Store::new(
        "tool_logs",
        "session_id",
        "id",
        &[
            "id",
            "agent_id",
            "tool_name",
            "args_json",
            "outcome",
            "result_text",
            "started_at",
            "created_at",
            "latency_ms",
            "iteration",
        ],
    ),
    Store::new(
        "impressions",
        "session_id",
        "id",
        &[
            "id",
            "agent_id",
            "target_id",
            "target_name",
            "personality",
            "communication_style",
            "recent_behavior",
            "agreement",
            "notes",
            "last_updated_turn",
            "created_at",
            "updated_at",
        ],
    ),
    Store::new(
        "llm_usage_metrics",
        "session_id",
        "id",
        &[
            "id",
            "agent_id",
            "timestamp",
            "provider",
            "model",
            "purpose",
            "task_type",
            "complexity",
            "input_tokens",
            "output_tokens",
            "total_tokens",
            "estimated_cost_usd",
            "latency_ms",
            "time_to_first_token_ms",
            "quality_score",
            "self_evaluation",
            "task_success",
            "would_use_again",
            "better_model_suggestion",
            "tags",
            "created_at",
        ],
    ),
    Store::new(
        "llm_logs",
        "session_id",
        "id",
        &[
            "id",
            "agent_id",
            "model",
            "prompt",
            "response",
            "tool_calls",
            "latency_ms",
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "error_code",
            "error_body",
            "requested_at",
            "trigger_message_id",
            "is_bot_iteration",
            "cache_read_tokens",
            "cache_creation_tokens",
            "created_at",
        ],
    ),
    Store::new(
        "pending_interactions",
        "session_id",
        "id",
        &[
            "id",
            "agent_id",
            "channel_id",
            "message_id",
            "platform",
            "surface_id",
            "a2ui_components_json",
            "status",
            "response_json",
            "responder_id",
            "owner_only",
            "timeout_secs",
            "created_at",
            "responded_at",
            "updated_at",
        ],
    ),
    Store::new(
        "memory_index_nodes",
        "source_session_id",
        "id",
        &[
            "id",
            "agent_id",
            "parent_id",
            "node_type",
            "source_type",
            "title",
            "summary",
            "start_log_id",
            "end_log_id",
            "date_from",
            "date_to",
            "depth",
            "child_count",
            "token_count",
            "created_at",
            "updated_at",
            "short_id",
        ],
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
        bail!(
            "prefix agent {} != binding subject {}",
            matches[0],
            m.agent_id
        );
    }
    let raw: String = conn.query_row(
        "SELECT participant_ids_json FROM sessions WHERE id = ?1",
        [&m.logical],
        |r| r.get(0),
    )?;
    let participants: Vec<String> = match serde_json::from_str(&raw)? {
        serde_json::Value::Array(ids) => ids
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => bail!(
            "legacy session {} participant_ids_json is not an array",
            m.logical
        ),
    };
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

pub fn snapshot_session(
    conn: &Connection,
    session_id: &str,
) -> Result<BTreeMap<String, StoreSnap>> {
    let mut map = BTreeMap::new();
    for store in STORES {
        map.insert(
            store.table.to_string(),
            snap_store(conn, store, session_id)?,
        );
    }
    Ok(map)
}

fn snap_store(conn: &Connection, store: &Store, session_id: &str) -> Result<StoreSnap> {
    snap_store_for_ids(conn, store, &[session_id])
}

fn snap_store_for_ids(conn: &Connection, store: &Store, session_ids: &[&str]) -> Result<StoreSnap> {
    if session_ids.is_empty() {
        return Ok(empty_snap());
    }
    let placeholders = session_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM {} WHERE {} IN ({placeholders})",
            store.table, store.session_col
        ),
        rusqlite::params_from_iter(session_ids.iter()),
        |r| r.get(0),
    )?;
    let cols = store.digest_cols.join(", ");
    let sql = format!(
        "SELECT {cols} FROM {} WHERE {} IN ({placeholders}) ORDER BY {}",
        store.table, store.session_col, store.pk
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut hasher = Sha256::new();
    let mut rows = stmt.query(rusqlite::params_from_iter(session_ids.iter()))?;
    while let Some(row) = rows.next()? {
        hash_row(&mut hasher, store, row)?;
        hasher.update([0xff]);
    }
    Ok(StoreSnap {
        count,
        digest: hex_lower(&hasher.finalize()),
    })
}

fn hash_row(hasher: &mut Sha256, store: &Store, row: &rusqlite::Row<'_>) -> Result<()> {
    for i in 0..store.digest_cols.len() {
        let v = field_to_digest_text(store, i, row.get_ref(i)?)?;
        hasher.update(v.as_deref().unwrap_or("").as_bytes());
        hasher.update([0u8]);
    }
    Ok(())
}

fn field_to_digest_text(
    store: &Store,
    col_idx: usize,
    value: rusqlite::types::ValueRef<'_>,
) -> Result<Option<String>> {
    match value {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Integer(n) => Ok(Some(n.to_string())),
        rusqlite::types::ValueRef::Real(n) => Ok(Some(n.to_string())),
        rusqlite::types::ValueRef::Text(t) => {
            let s = std::str::from_utf8(t).map_err(|_| {
                anyhow::anyhow!(
                    "invalid utf-8 in {}.{}",
                    store.table,
                    store.digest_cols[col_idx]
                )
            })?;
            Ok(Some(s.to_string()))
        }
        rusqlite::types::ValueRef::Blob(b) => {
            let s = std::str::from_utf8(b).map_err(|_| {
                anyhow::anyhow!(
                    "invalid utf-8 in {}.{}",
                    store.table,
                    store.digest_cols[col_idx]
                )
            })?;
            Ok(Some(s.to_string()))
        }
    }
}

fn empty_snap() -> StoreSnap {
    let hasher = Sha256::new();
    StoreSnap {
        count: 0,
        digest: hex_lower(&hasher.finalize()),
    }
}

fn expected_after(conn: &Connection, m: &Mapping) -> Result<BTreeMap<String, StoreSnap>> {
    let mut map = BTreeMap::new();
    for store in STORES {
        let snap = if store.table == "agent_sessions" {
            expected_agent_sessions(conn, m)?
        } else {
            snap_store_for_ids(conn, store, &[&m.logical, &m.physical])?
        };
        map.insert(store.table.to_string(), snap);
    }
    Ok(map)
}

fn expected_agent_sessions(conn: &Connection, m: &Mapping) -> Result<StoreSnap> {
    let (legacy, physical) = load_agent_session_sides(conn, m)?;
    if legacy.is_none() && physical.is_none() {
        return Ok(empty_snap());
    }
    let last = stronger_time(
        legacy.as_ref().and_then(|r| r.1.clone()),
        physical.as_ref().and_then(|r| r.1.clone()),
    );
    let done = i64::from(
        legacy.as_ref().is_some_and(|r| r.2 != 0) || physical.as_ref().is_some_and(|r| r.2 != 0),
    );
    let mut hasher = Sha256::new();
    hasher.update(m.agent_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(last.as_deref().unwrap_or("").as_bytes());
    hasher.update([0u8]);
    hasher.update(done.to_string().as_bytes());
    hasher.update([0u8]);
    hasher.update([0xff]);
    Ok(StoreSnap {
        count: 1,
        digest: hex_lower(&hasher.finalize()),
    })
}

type AgentSessionSide = Option<(String, Option<String>, i64)>;

fn load_agent_session_sides(
    conn: &Connection,
    m: &Mapping,
) -> Result<(AgentSessionSide, AgentSessionSide)> {
    let legacy = load_agent_session_rows(conn, &m.logical)?;
    let physical = load_agent_session_rows(conn, &m.physical)?;
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
    Ok((legacy.into_iter().next(), physical.into_iter().next()))
}

fn load_agent_session_rows(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<(String, Option<String>, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, last_speech_at, done_declared FROM agent_sessions WHERE session_id = ?1",
    )?;
    let rows = stmt
        .query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
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

fn read_legacy_meta(conn: &Connection, logical: &str) -> Result<Option<serde_json::Value>> {
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT metadata_json FROM sessions WHERE id = ?1",
            [logical],
            |r| r.get(0),
        )
        .optional()?;
    let Some(Some(raw)) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(Some(serde_json::json!({})));
    }
    Ok(Some(serde_json::from_str(&raw)?))
}

fn marker_present(conn: &Connection, logical: &str, physical: &str) -> Result<bool> {
    let Some(v) = read_legacy_meta(conn, logical)? else {
        return Ok(false);
    };
    Ok(v.get(WEBGATE_MARKER).and_then(|x| x.as_str()) == Some(physical))
}

fn saved_inventory(
    conn: &Connection,
    logical: &str,
) -> Result<Option<BTreeMap<String, StoreSnap>>> {
    let Some(v) = read_legacy_meta(conn, logical)? else {
        return Ok(None);
    };
    let Some(inv) = v.get(WEBGATE_INVENTORY) else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_value(inv.clone())?))
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

fn assert_physical_matches_inventory(
    conn: &Connection,
    m: &Mapping,
    saved: &BTreeMap<String, StoreSnap>,
) -> Result<()> {
    let physical = snapshot_session(conn, &m.physical)?;
    for store in STORES {
        let got = &physical[store.table];
        let want = saved
            .get(store.table)
            .ok_or_else(|| anyhow::anyhow!("inventory missing store {}", store.table))?;
        if got != want {
            bail!(
                "re-run: {} physical count/digest mismatch: got {:?} want {:?}",
                store.table,
                got,
                want
            );
        }
    }
    if physical.len() != STORES.len() || saved.len() != STORES.len() {
        bail!("re-run: inventory does not cover all 14 stores");
    }
    Ok(())
}

pub fn transplant_mapping(conn: &Connection, m: &Mapping) -> Result<TransplantOutcome> {
    validate_legacy_session(conn, m)?;
    let already = marker_present(conn, &m.logical, &m.physical)?;
    if already {
        if legacy_refs(conn, &m.logical)? != 0 {
            bail!("re-run: alias present but legacy refs remain");
        }
        let Some(saved) = saved_inventory(conn, &m.logical)? else {
            bail!("re-run: alias present but inventory missing");
        };
        assert_physical_matches_inventory(conn, m, &saved)?;
        return Ok(TransplantOutcome::WriteZero);
    }
    if legacy_refs(conn, &m.logical)? == 0 {
        let inventory = snapshot_session(conn, &m.physical)?;
        write_alias_marker(conn, m, &inventory)?;
        return Ok(TransplantOutcome::WriteZero);
    }

    let expected = expected_after(conn, m)?;
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

    let after_legacy = snapshot_session(&tx, &m.logical)?;
    let after_physical = snapshot_session(&tx, &m.physical)?;
    for store in STORES {
        if after_legacy[store.table].count != 0 {
            bail!("{} still has legacy refs after transplant", store.table);
        }
        if after_physical[store.table] != expected[store.table] {
            bail!(
                "{} count/digest mismatch: got {:?} want {:?}",
                store.table,
                after_physical[store.table],
                expected[store.table]
            );
        }
    }

    let logs: Vec<i64> = tx
        .prepare("SELECT id FROM memory_sessions WHERE session_id = ?1 ORDER BY id ASC")?
        .query_map([&m.physical], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut sorted = logs.clone();
    sorted.sort_unstable();
    if logs != sorted {
        bail!("physical logs are not id ASC");
    }
    write_alias_marker_in_tx(&tx, m, &after_physical)?;
    tx.commit()?;
    Ok(TransplantOutcome::Migrated)
}

fn migrate_agent_sessions(tx: &rusqlite::Transaction<'_>, m: &Mapping) -> Result<()> {
    let (legacy, physical) = load_agent_session_sides(tx, m)?;
    let last = stronger_time(
        legacy.as_ref().and_then(|r| r.1.clone()),
        physical.as_ref().and_then(|r| r.1.clone()),
    );
    let done = i64::from(
        legacy.as_ref().is_some_and(|r| r.2 != 0) || physical.as_ref().is_some_and(|r| r.2 != 0),
    );
    tx.execute(
        "DELETE FROM agent_sessions WHERE session_id = ?1",
        [&m.logical],
    )?;
    if physical.is_none() {
        if legacy.is_some() {
            tx.execute(
                "INSERT INTO agent_sessions (agent_id, session_id, last_speech_at, done_declared)
                 VALUES (?1, ?2, ?3, ?4)",
                params![m.agent_id, m.physical, last, done],
            )?;
        }
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

fn write_alias_marker(
    conn: &Connection,
    m: &Mapping,
    inventory: &BTreeMap<String, StoreSnap>,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    write_alias_marker_in_tx(&tx, m, inventory)?;
    tx.commit()?;
    Ok(())
}

fn write_alias_marker_in_tx(
    tx: &rusqlite::Transaction<'_>,
    m: &Mapping,
    inventory: &BTreeMap<String, StoreSnap>,
) -> Result<()> {
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
    map.insert(
        WEBGATE_INVENTORY.to_string(),
        serde_json::to_value(inventory)?,
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
    use crate::init_memory;
    use crate::queries::{insert_session, insert_session_log, SessionLogRow, SessionRow};

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
        put_web_binding(&conn, "a1", logical, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
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
        put_web_binding(&conn, "a1", logical, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
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
        put_web_binding(&conn, "a1", logical, "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee");
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
        put_web_binding(&conn, "a1", logical, "cccccccc-cccc-4ccc-8ccc-cccccccccccc");
        let m = list_web_mappings(&conn).unwrap();
        assert!(transplant_mapping(&conn, &m[0]).is_err());
    }

    #[test]
    fn transplant_ten_thousand_logs_preserve_order() {
        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        let logical = "web-a1-big";
        legacy_session(&conn, logical, "a1");
        put_web_binding(&conn, "a1", logical, "dddddddd-dddd-4ddd-8ddd-dddddddddddd");
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

    #[test]
    fn transplant_zero_logs_still_validates_prefix() {
        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        agent(&conn, "a1x");
        let logical = "web-a1x-c";
        legacy_session(&conn, logical, "a1x");
        put_web_binding(&conn, "a1", logical, "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee");
        let m = list_web_mappings(&conn).unwrap();
        assert!(transplant_mapping(&conn, &m[0]).is_err());
    }

    #[test]
    fn transplant_rerun_fails_when_physical_digest_changes() {
        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        let logical = "web-a1-c1";
        legacy_session(&conn, logical, "a1");
        put_web_binding(&conn, "a1", logical, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        speech(&conn, "a1", logical, "hello");
        let m = list_web_mappings(&conn).unwrap();
        assert_eq!(
            transplant_mapping(&conn, &m[0]).unwrap(),
            TransplantOutcome::Migrated
        );
        conn.execute(
            "UPDATE memory_sessions SET content = 'tampered' WHERE session_id = ?1",
            [&m[0].physical],
        )
        .unwrap();
        let err = transplant_mapping(&conn, &m[0]).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "{err}");
    }

    #[test]
    fn transplant_mixed_legacy_and_physical_digest() {
        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        let logical = "web-a1-mix";
        legacy_session(&conn, logical, "a1");
        put_web_binding(&conn, "a1", logical, "ffffffff-ffff-4fff-8fff-ffffffffffff");
        speech(&conn, "a1", logical, "legacy");
        speech(
            &conn,
            "a1",
            &list_web_mappings(&conn).unwrap()[0].physical,
            "phys",
        );
        let m = list_web_mappings(&conn).unwrap();
        let expected = expected_after(&conn, &m[0]).unwrap();
        assert_eq!(expected["memory_sessions"].count, 2);
        assert_eq!(
            transplant_mapping(&conn, &m[0]).unwrap(),
            TransplantOutcome::Migrated
        );
        let after = snapshot_session(&conn, &m[0].physical).unwrap();
        assert_eq!(after["memory_sessions"], expected["memory_sessions"]);
        assert_eq!(
            after["memory_sessions_fts"],
            expected["memory_sessions_fts"]
        );
        assert_eq!(after["agent_sessions"], expected["agent_sessions"]);
        assert_eq!(
            transplant_mapping(&conn, &m[0]).unwrap(),
            TransplantOutcome::WriteZero
        );
    }

    #[test]
    fn transplant_rejects_invalid_utf8() {
        let conn = init_memory().unwrap();
        agent(&conn, "a1");
        let logical = "web-a1-bin";
        legacy_session(&conn, logical, "a1");
        put_web_binding(&conn, "a1", logical, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaa99");
        conn.execute(
            "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at)
             VALUES ('a1', ?1, 'speech', x'c3', datetime('now'))",
            [logical],
        )
        .unwrap();
        let m = list_web_mappings(&conn).unwrap();
        let err = transplant_mapping(&conn, &m[0]).unwrap_err();
        assert!(err.to_string().contains("invalid utf-8"), "{err}");
    }
}
