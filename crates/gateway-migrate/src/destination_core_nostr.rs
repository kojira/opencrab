// Core-only pre-separation Nostr source has no schema-0 gateway store. The
// historical importer took owner from agent_nostr_config, trusted senders from
// trusted_users(platform='nostr'), and eligible peers by their Nostr self key.
fn normalize_historical_nostr_watches(
    core: &Connection,
    agent_id: &str,
    instance_id: &str,
    config_b64: &str,
) -> Result<(String, bool)> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(config_b64)?;
    let mut value: Value = serde_json::from_slice(&bytes)?;
    let mut converted = false;
    if let Some(watches) = value.get_mut("watches").and_then(Value::as_array_mut) {
        for watch in watches {
            let object = watch.as_object_mut().context("Nostr watch must be an object")?;
            let Some(session) = object.remove("session_id") else { continue };
            let session = session.as_str().context("historical Nostr watch session invalid")?;
            let id = object.get("id").and_then(Value::as_i64).context("historical Nostr watch ID missing")?;
            let interval = object.get("interval_secs").and_then(Value::as_i64).context("historical Nostr watch interval missing")?;
            let source = core.prepare("SELECT session_id,interval_secs,filter_json FROM session_watches WHERE agent_id=?1 AND id=?2")?
                .query_row(params![agent_id,id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?)))
                .optional()?.context("historical Nostr watch source missing")?;
            ensure!(session == source.0 && interval == source.1, "historical Nostr watch source mismatch");
            let filter = object.get("filter_json").or_else(|| object.get("filter")).context("historical Nostr watch filter missing")?;
            let parsed: opencrab_nostr_gateway::config::WatchFilter = serde_json::from_value(filter.clone())?;
            let source_filter: opencrab_nostr_gateway::config::WatchFilter = serde_json::from_str(&source.2)?;
            ensure!(serde_json::to_value(parsed)? == serde_json::to_value(source_filter)?, "historical Nostr watch filter mismatch");
            let bound: bool = core.query_row("SELECT EXISTS(SELECT 1 FROM gate_bindings b JOIN agent_sessions s ON s.session_id=b.session_id WHERE b.instance_id=?1 AND b.session_id=?2 AND s.agent_id=?3 AND b.closed_at IS NULL)",
                params![instance_id,session,agent_id], |row| row.get(0))?;
            ensure!(bound, "historical Nostr watch has no bound session");
            converted = true;
        }
    }
    if !converted { return Ok((config_b64.into(), false)); }
    Ok((base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&value)?), true))
}

fn core_only_nostr_config(
    core: &Connection,
    rows: &[SourceRow],
    agent_id: &str,
    instance_id: &str,
    config_b64: &str,
) -> Result<Option<String>> {
    let Some(source) = rows.iter().find(|row| row.table == "agent_nostr_config" && row.text("agent_id").ok() == Some(agent_id)) else {
        return Ok(None);
    };
    let (normalized, _) = normalize_historical_nostr_watches(core, agent_id, instance_id, config_b64)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(normalized)?;
    let mut config = opencrab_nostr_gateway::config::parse_instance_config(&bytes)?;
    let self_key = source.text("self_pubkey")?;
    ensure!(self_key.is_empty() || self_key == config.self_pubkey, "Nostr self key source conflicts with generic config");
    let owner = source.text("owner_pubkey")?;
    if !owner.is_empty() {
        ensure!(config.access.owner.is_empty() || config.access.owner == [owner], "Nostr owner source conflicts with generic config");
        config.access.owner = vec![owner.into()];
    }
    let mut trusted = rows.iter().filter(|row| row.table == "trusted_users" && row.text("agent_id").ok() == Some(agent_id) && row.text("platform").ok() == Some("nostr"))
        .map(|row| row.text("user_id").map(str::to_owned)).collect::<Result<Vec<_>>>()?;
    trusted.sort();
    trusted.dedup();
    ensure!(config.access.trusted_users.is_empty() || config.access.trusted_users == trusted, "Nostr trusted source conflicts with generic config");
    config.access.trusted_users = trusted;
    let old_peers = std::mem::take(&mut config.access.co_agents);
    let mut stmt = core.prepare("SELECT co_agent_id FROM trusted_co_agents WHERE agent_id=?1 ORDER BY co_agent_id")?;
    for peer in stmt.query_map([agent_id], |row| row.get::<_, String>(0))? {
        let peer = peer?;
        if let Some(key) = rows.iter().find(|row| row.table == "agent_nostr_config" && row.text("agent_id").ok() == Some(peer.as_str()))
            .map(|row| row.text("self_pubkey")).transpose()?.filter(|key| !key.is_empty()) {
            ensure!(config.access.co_agents.get(key).is_none_or(|existing| existing == &peer), "Nostr peer identity conflicts with generic config");
            config.access.co_agents.insert(key.to_owned(), peer);
        }
    }
    ensure!(old_peers.is_empty() || old_peers == config.access.co_agents, "Nostr peer source conflicts with generic config");
    let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&config)?);
    Ok(Some(opencrab_nostr_gateway::config::canonicalize_config_b64(&encoded)?))
}
