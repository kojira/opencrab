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

