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

