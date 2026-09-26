// The pre-S5 gateway store is an S8 source, never a live daemon fallback.
pub(crate) fn legacy_nostr_core_updates(core: &Connection, paths: &BTreeMap<(String, String), PathBuf>) -> Result<Vec<LegacyNostrCoreUpdate>> {
    let mut updates = Vec::new();
    for ((kind, _), path) in paths {
        if kind != "nostr" { continue; }
        let gateway = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let retained: bool = gateway.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='legacy_nostr_instances')",
            [], |row| row.get(0),
        )?;
        if !retained { continue; }
        let mut stmt = gateway.prepare("SELECT i.instance_id,i.agent_id,i.config_b64,i.enabled,i.core_revision,i.core_digest FROM instances i JOIN legacy_nostr_instances l ON l.agent_id=i.agent_id ORDER BY i.instance_id")?;
        let desired = stmt.query_map([], |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,bool>(3)?,row.get::<_,Option<i64>>(4)?,row.get::<_,Option<String>>(5)?)))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        let source_count: i64 = gateway.query_row("SELECT COUNT(*) FROM legacy_nostr_instances", [], |row| row.get(0))?;
        ensure!(source_count == desired.len() as i64, "legacy Nostr source lost an instance");
        for (instance_id, agent_id, config_b64, enabled, destination_revision, destination_digest) in desired {
            let (revision, old_enabled, old_config, kind, bound_agent): (i64, bool, String, String, String) = core.query_row(
                "SELECT i.revision,i.enabled,i.config_b64,i.kind_id,a.agent_id FROM gate_instances i JOIN agents a ON a.subject_id=i.subject_id WHERE i.instance_id=?1 AND i.deleted_at IS NULL",
                [&instance_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
            )?;
            ensure!(kind == "nostr" && bound_agent == agent_id, "legacy Nostr core association changed");
            let canonical = opencrab_nostr_gateway::config::canonicalize_config_b64(&config_b64)?;
            ensure!(canonical == config_b64, "legacy Nostr target config changed");
            let changed = old_enabled != enabled || old_config != config_b64;
            let expected_revision = revision.checked_add(i64::from(changed)).context("Nostr revision overflow")?;
            let digest = canonical::hex(&Sha256::digest(base64::engine::general_purpose::STANDARD.decode(&config_b64)?));
            ensure!(destination_revision == Some(expected_revision) && destination_digest.as_deref() == Some(digest.as_str()), "legacy Nostr destination revision/digest conflict");
            if changed {
                updates.push(LegacyNostrCoreUpdate {
                    instance_id, expected_revision: u64::try_from(revision)?, enabled,
                    config_digest: digest,
                    config_b64,
                });
            }
        }
    }
    Ok(updates)
}
fn authoritative_nostr_config(conn: &Connection, agent_id: &str, core_config_b64: &str, core_enabled: bool) -> Result<(String, bool, bool)> {
    use opencrab_nostr_gateway::config::{AccessConfig, WatchFilter, WatchPlacement, DEFAULT_BUNDLE_MAX_ITEMS};
    let bytes = base64::engine::general_purpose::STANDARD.decode(core_config_b64)?;
    let mut config = opencrab_nostr_gateway::config::parse_instance_config(&bytes)?;
    let (name, relays, filter, enabled, self_pubkey): (String, String, String, bool, String) = conn.query_row(
        "SELECT agent_name,relays_json,filter_json,enabled,self_pubkey FROM instances WHERE agent_id=?1",
        [agent_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).optional()?.context("old Nostr instance missing")?;
    config.name = Some(name);
    config.relays = serde_json::from_str(&relays)?;
    config.filter = serde_json::from_str::<WatchFilter>(&filter)?;
    if !self_pubkey.is_empty() { config.self_pubkey = self_pubkey; }
    let mut access = AccessConfig { followees: config.access.followees.clone(), ..Default::default() };
    let allowed = conn.prepare("SELECT role,external_id,mapped_agent_id FROM allow_identities WHERE agent_id=?1 ORDER BY role,external_id")?
        .query_map([agent_id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (role, external, mapped) in allowed {
        match (role.as_str(), mapped) {
            ("owner", None) => access.owner.push(external),
            ("trusted", None) => access.trusted_users.push(external),
            ("co_agent", Some(agent)) => { access.co_agents.insert(external, agent); },
            _ => bail!("unsupported old Nostr identity role"),
        }
    }
    config.access = access;
    let old_watches = conn.prepare("SELECT watch_id,interval_secs,filter_json FROM watches WHERE agent_id=?1 ORDER BY watch_id")?
        .query_map([agent_id], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut watches = Vec::new();
    for (id, interval_secs, filter) in old_watches {
        let max_items = config.watches.iter().find(|watch| watch.id == id)
            .map(|watch| watch.max_items).unwrap_or(DEFAULT_BUNDLE_MAX_ITEMS);
        watches.push(WatchPlacement { id, interval_secs, max_items,
            filter: serde_json::from_str::<WatchFilter>(&filter)?, filter_json: None });
    }
    config.watches = watches;
    let config_b64 = opencrab_nostr_gateway::config::canonicalize_config_b64(
        &base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&config)?))?;
    Ok((config_b64.clone(), enabled, enabled != core_enabled || config_b64 != core_config_b64))
}
fn legacy_nostr_shape(conn: &Connection) -> Result<bool> {
    let old = conn
        .prepare("PRAGMA table_info(instances)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    if old.contains("instance_id") {
        return Ok(false);
    }
    ensure!(old.contains("agent_id") && old.contains("secret_key"), "unsupported Nostr destination schema");
    for (table, columns) in [
        ("instances", &["agent_id", "agent_name", "secret_key", "relays_json", "filter_json", "enabled", "owner_pubkey", "self_pubkey", "updated_at"][..]),
        ("watches", &["watch_id", "agent_id", "session_id", "interval_secs", "filter_json", "created_at"][..]),
        ("allow_identities", &["agent_id", "role", "external_id", "mapped_agent_id"][..]),
        ("gateway_meta", &["key", "value"][..]),
    ] {
        source::require_columns(conn, table, columns)?;
    }
    let has_new: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name IN ('endpoints','identity_projections','legacy_nostr_instances'))",
        [], |row| row.get(0),
    )?;
    ensure!(!has_new, "mixed old/current Nostr destination schema");
    Ok(true)
}

fn legacy_nostr_credential(conn: &Connection, agent_id: &str, key: &[u8; 32]) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let value: Option<String> = conn.query_row(
        "SELECT secret_key FROM instances WHERE agent_id=?1", [agent_id], |row| row.get(0),
    ).optional()?;
    value.map(|value| {
        if opencrab_nostr_gateway::secret_store::is_encrypted(&value) {
            opencrab_nostr_gateway::secret_store::decrypt(&value, key)
        } else {
            Ok(Zeroizing::new(value.into_bytes()))
        }
    }).transpose()
}

fn validate_legacy_nostr_store(core: &Connection, rows: &[SourceRow], conn: &Connection, selected: &[&InstancePlan]) -> Result<()> {
    let old_agents = conn.prepare("SELECT agent_id FROM instances ORDER BY agent_id")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(old_agents.len() == selected.len(), "unmapped old Nostr instance");
    for agent in &old_agents {
        ensure!(selected.iter().filter(|plan| plan.agent_id == *agent).count() == 1, "old Nostr instance association ambiguous");
    }
    for plan in selected {
        ensure!(!plan.binding_ids.is_empty(), "old Nostr instance has no existing binding");
        let old: (String,String,String,bool,String,String) = conn.query_row(
            "SELECT agent_name,relays_json,filter_json,enabled,owner_pubkey,self_pubkey FROM instances WHERE agent_id=?1",
            [&plan.agent_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
        )?;
        let decoded = base64::engine::general_purpose::STANDARD.decode(&plan.config_b64)?;
        let config = opencrab_nostr_gateway::config::parse_instance_config(&decoded)?;
        ensure!(old.3 == plan.enabled && old.0 == config.name.as_deref().unwrap_or_default(), "old Nostr instance settings conflict");
        ensure!(serde_json::from_str::<Value>(&old.1)? == serde_json::to_value(&config.relays)?
            && serde_json::to_value(serde_json::from_str::<opencrab_nostr_gateway::config::WatchFilter>(&old.2)?)? == serde_json::to_value(&config.filter)?
            && old.5 == config.self_pubkey, "old Nostr gateway config conflict");
        let mut allowed = conn.prepare("SELECT role,external_id,mapped_agent_id FROM allow_identities WHERE agent_id=?1")?;
        let old_allowed = allowed.query_map([&plan.agent_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,row.get::<_, Option<String>>(2)?)))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        for (role, external, mapped) in &old_allowed {
            let represented = match role.as_str() {
                "owner" => mapped.is_none() && config.access.owner.contains(&external),
                "trusted" => mapped.is_none() && config.access.trusted_users.contains(&external),
                "co_agent" => config.access.co_agents.get(external) == mapped.as_ref(),
                _ => false,
            };
            ensure!(represented, "old Nostr allowed identity not represented by current admission");
        }
        let current_allowed = config.access.owner.iter().map(|value| ("owner".to_string(), value.clone(), None))
            .chain(config.access.trusted_users.iter().map(|value| ("trusted".to_string(), value.clone(), None)))
            .chain(config.access.co_agents.iter().map(|(key,value)| ("co_agent".to_string(), key.clone(), Some(value.clone()))))
            .collect::<BTreeSet<_>>();
        ensure!(old_allowed.into_iter().collect::<BTreeSet<_>>() == current_allowed,
            "current Nostr admission differs from old gateway allow identities");
        let old_watch_count: i64 = conn.query_row("SELECT COUNT(*) FROM watches WHERE agent_id=?1", [&plan.agent_id], |row| row.get(0))?;
        ensure!(old_watch_count == config.watches.len() as i64, "current Nostr watch inventory differs from old gateway store");
        let mut watches = conn.prepare("SELECT watch_id,session_id,interval_secs,filter_json FROM watches WHERE agent_id=?1")?;
        for row in watches.query_map([&plan.agent_id], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,String>(3)?)))? {
            let (id, session, interval, filter) = row?;
            let parsed = serde_json::from_str::<opencrab_nostr_gateway::config::WatchFilter>(&filter)?;
            ensure!(config.watches.iter().any(|watch| watch.id == id && watch.interval_secs == interval
                && serde_json::to_value(&parsed).ok() == serde_json::to_value(watch.effective_filter()).ok()),
                "old Nostr watch not represented by current config");
            ensure!(core.query_row("SELECT EXISTS(SELECT 1 FROM gate_bindings WHERE instance_id=?1 AND session_id=?2 AND closed_at IS NULL)",
                params![plan.instance_id,session], |row| row.get::<_,bool>(0))?,
                "old Nostr watch has no existing bound session");
            // The old gateway can own a watch without a duplicate core row. If both
            // stores carry the same watch, neither may silently choose a session.
            for source in rows.iter().filter(|row| row.table == "session_watches"
                && row.text("agent_id").ok() == Some(plan.agent_id.as_str())
                && row.integer("id").ok() == Some(id)) {
                ensure!(source.text("session_id")? == session
                    && source.integer("interval_secs")? == interval
                    && serde_json::to_value(serde_json::from_str::<opencrab_nostr_gateway::config::WatchFilter>(source.text("filter_json")?)?)?
                        == serde_json::to_value(&parsed)?,
                    "old Nostr watch session conflict");
            }
        }
    }
    for table in ["watches", "allow_identities"] {
        let sql = format!("SELECT agent_id FROM {table}");
        for agent in conn.prepare(&sql)?.query_map([], |row| row.get::<_,String>(0))? {
            ensure!(old_agents.contains(&agent?), "orphan old Nostr row");
        }
    }
    Ok(())
}

fn import_legacy_nostr_identities(tx: &Transaction<'_>, plans: &[&InstancePlan], inserted: &mut Vec<Value>) -> Result<usize> {
    let mut statement = tx.prepare("SELECT agent_id,role,external_id,mapped_agent_id FROM allow_identities ORDER BY agent_id,role,external_id")?;
    let entries = statement.query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?)))?
        .collect::<std::result::Result<Vec<_>,_>>()?;
    for (agent, old_role, external, mapped) in &entries {
        let plan = plans.iter().find(|plan| plan.agent_id == *agent).context("unmapped old Nostr identity")?;
        let role = match old_role.as_str() {
            "owner" => "owner",
            "trusted" => "trusted_user",
            "co_agent" => "co_agent",
            _ => bail!("unsupported old Nostr identity role"),
        };
        let key = vec![plan.instance_id.clone(),role.into(),external.clone()];
        let expected = json!({"external_id":external,"instance_id":plan.instance_id,
            "relationship_id":mapped,"relationship_revision":null,"role":role});
        if let Some(existing) = current_semantic(tx, "identity_projections", &key)? {
            ensure!(existing == expected, "old Nostr identity conflicts with core projection");
        } else {
            tx.execute("INSERT INTO identity_projections(instance_id,role,external_id,relationship_id,relationship_revision) VALUES (?1,?2,?3,?4,NULL)",
                params![plan.instance_id,role,external,mapped])?;
            inserted.push(key_value("identity_projections", key.clone(), row_hash("identity_projections", &key, &expected)?)?);
        }
    }
    Ok(entries.len())
}

fn convert_legacy_nostr_store(tx: &Transaction<'_>) -> Result<()> {
    ensure!(legacy_nostr_shape(tx)?, "Nostr conversion requires old store");
    tx.execute_batch("ALTER TABLE instances RENAME TO legacy_nostr_instances;")?;
    opencrab_nostr_gateway::store::NostrStore::initialize_schema(tx)?;
    Ok(())
}
