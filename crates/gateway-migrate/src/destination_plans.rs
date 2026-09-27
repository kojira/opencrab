fn build_instance_plans(core: &Connection, rows: &[SourceRow], approval: &Approval, inputs: &Inputs) -> Result<Vec<InstancePlan>> {
    ensure!(inputs.credential_files.is_empty(), "Web credential files are not supported");
    let mut statement = core.prepare(
        "SELECT i.instance_id,i.kind_id,a.agent_id,i.subject_id,i.revision,i.enabled,i.config_b64,i.config_digest
         FROM gate_instances i JOIN agents a ON a.subject_id=i.subject_id
         WHERE i.deleted_at IS NULL ORDER BY i.kind_id,i.instance_id",
    )?;
    let raw = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut plans = Vec::new();
    for (instance_id, kind, agent_id, subject_id, mut revision, mut enabled, mut config_b64, digest) in raw {
        if !matches!(kind.as_str(), "discord" | "nostr" | "web") {
            continue;
        }
        let destination = approval.destination(&kind)?.clone();
        let decoded = base64::engine::general_purpose::STANDARD.decode(&config_b64)?;
        ensure!(canonical::hex(&Sha256::digest(&decoded)) == digest, "config digest mismatch");
        let destination_path = inputs.paths.get(&(kind.clone(), destination.path_id.clone())).context("missing destination")?;
        let nostr_conn = if kind == "nostr" {
            Some(Connection::open_with_flags(destination_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?)
        } else {
            None
        };
        let old_nostr_store = nostr_conn.as_ref().map(legacy_nostr_shape).transpose()?.unwrap_or(false);
        let retained_old_nostr = if let Some(conn) = nostr_conn.as_ref() {
            conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='legacy_nostr_instances')", [], |row| row.get::<_, bool>(0))?
        } else {
            false
        };
        let core_only_nostr = kind == "nostr" && !old_nostr_store && !retained_old_nostr
            && rows.iter().any(|row| row.table == "agent_nostr_config" && row.text("agent_id").ok() == Some(agent_id.as_str()));
        let (normalized, historical_watch) = if core_only_nostr {
            normalize_historical_nostr_watches(core, &agent_id, &instance_id, &config_b64)?
        } else { (config_b64.clone(), false) };
        let canonical = match kind.as_str() {
            "discord" => opencrab_discord_gateway::config::canonicalize_config_b64(&config_b64)?,
            "nostr" => canonicalize_nostr_migration_config_b64(&normalized, !enabled && !core_only_nostr)?,
            "web" => config_b64.clone(),
            _ => unreachable!(),
        };
        if kind == "nostr" && !enabled && !core_only_nostr && canonical != config_b64 {
            config_b64 = canonical.clone();
        }
        ensure!(canonical == config_b64 || old_nostr_store || retained_old_nostr || historical_watch, "noncanonical config");
        let mut addresses = core
            .prepare("SELECT address FROM gate_bindings WHERE instance_id=?1 AND closed_at IS NULL ORDER BY address")?
            .query_map([&instance_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        addresses.sort();
        let binding_ids = core.prepare("SELECT binding_id FROM gate_bindings WHERE instance_id=?1 AND closed_at IS NULL ORDER BY binding_id")?
            .query_map([&instance_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut legacy_nostr = false;
        if core_only_nostr {
            let desired = core_only_nostr_config(core, rows, &agent_id, &instance_id, &config_b64)?.context("Nostr source missing")?;
            if desired != config_b64 {
                revision = revision.checked_add(1).context("Nostr core revision overflow")?;
                config_b64 = desired;
            }
        }
        if let Some(conn) = nostr_conn {
            if old_nostr_store {
                let (merged, old_enabled, changed) = authoritative_nostr_config(&conn, &agent_id, &config_b64, enabled)?;
                config_b64 = merged;
                enabled = old_enabled;
                if changed { revision = revision.checked_add(1).context("Nostr core revision overflow")?; }
                legacy_nostr = true;
            } else if retained_old_nostr {
                let (merged, old_enabled): (String, bool) = conn.query_row(
                    "SELECT config_b64,enabled FROM instances WHERE instance_id=?1 AND agent_id=?2",
                    params![instance_id,agent_id], |row| Ok((row.get(0)?,row.get(1)?)),
                )?;
                ensure!(opencrab_nostr_gateway::config::canonicalize_config_b64(&merged)? == merged, "retained old Nostr config invalid");
                if old_enabled != enabled || merged != config_b64 {
                    revision = revision.checked_add(1).context("Nostr core revision overflow")?;
                }
                config_b64 = merged;
                enabled = old_enabled;
                legacy_nostr = true;
            }
        }
        let (credential, source) = if kind == "web" {
            ensure!(
                !approval.credential_sources.iter().any(|item| item.instance_id == instance_id),
                "Web credential is not part of historical admission"
            );
            (Zeroizing::new(Vec::new()), String::new())
        } else {
            let key_path = inputs.master_keys.get(&kind).context("missing master key")?;
            let key = read_master_key(key_path, &kind)?;
            select_credential(rows, approval, destination_path, &kind, &instance_id, &agent_id, enabled, &key)?
        };
        plans.push(InstancePlan {
            destination,
            instance_id,
            agent_id,
            subject_id,
            revision,
            config_b64,
            addresses,
            binding_ids,
            enabled,
            credential,
            credential_source: source,
            created_at: approval.created_at.clone(),
            legacy_nostr,
            core_only_nostr,
        });
    }
    Ok(plans)
}

fn canonicalize_nostr_migration_config_b64(config_b64: &str, allow_empty_access: bool) -> Result<String> {
    if !allow_empty_access {
        return opencrab_nostr_gateway::config::canonicalize_config_b64(config_b64);
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(config_b64)?;
    let mut config: opencrab_nostr_gateway::config::InstanceConfig = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("instance config is not valid JSON object: {e}"))?;
    if config.delivery_mode.is_none() {
        config.delivery_mode = Some("tool_driven".into());
    }
    if !config.access.is_empty() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&config)?);
        return opencrab_nostr_gateway::config::canonicalize_config_b64(&encoded);
    }
    ensure!(!config.relays.is_empty(), "relays must be nonempty");
    ensure!(is_lower_hex_pubkey(&config.self_pubkey), "self_pubkey must be 64 lowercase hex");
    ensure!(config.name.as_deref().map(str::trim).is_some_and(|name| !name.is_empty()), "name must be nonempty");
    ensure!(matches!(config.delivery_mode.as_deref(), Some("say") | Some("tool_driven")), "delivery_mode must be say or tool_driven");
    for watch in &config.watches {
        ensure!(watch.interval_secs > 0, "watch {} interval_secs must be a positive integer", watch.id);
        ensure!(watch.max_items > 0, "watch {} max_items must be a positive integer", watch.id);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&config)?))
}

fn is_lower_hex_pubkey(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn select_credential(rows: &[SourceRow], approval: &Approval, path: &Path, kind: &str, instance_id: &str, agent_id: &str, enabled: bool, key: &[u8; 32]) -> Result<(Zeroizing<Vec<u8>>, String)> {
    let descriptor = approval.credential_sources.iter().find(|item| item.instance_id == instance_id);
    let legacy = match kind {
        "discord" => rows
            .iter()
            .find(|row| row.table == "agent_discord_config" && row.text("agent_id").ok() == Some(agent_id))
            .map(|row| row.text("bot_token").map(|value| Zeroizing::new(value.as_bytes().to_vec())))
            .transpose()?,
        "nostr" => rows
            .iter()
            .find(|row| row.table == "agent_nostr_config" && row.text("agent_id").ok() == Some(agent_id))
            .map(|row| row.text("secret_key").map(|value| Zeroizing::new(value.as_bytes().to_vec())))
            .transpose()?,
        _ => None,
    }
    .filter(|value| !value.is_empty());
    let destination = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let old_store = kind == "nostr" && legacy_nostr_shape(&destination)?;
    if old_store {
        if let Some(old) = legacy_nostr_credential(&destination, agent_id, key)? {
            // The historical gateway upsert could rotate its signing key without
            // writing back to the old core source row. The gateway key was live.
            if old.is_empty() {
                ensure!(!enabled, "enabled instance requires credential");
                ensure!(descriptor.is_none(), "empty old credential has an approval descriptor");
                return Ok((old, String::new()));
            }
            let selected = format!("legacy-nostr-store:instance:{instance_id}");
            let descriptor = descriptor.context("credential source missing")?;
            ensure!(descriptor.source == selected, "old Nostr credential source descriptor mismatch");
            return Ok((old, selected));
        }
        ensure!(!descriptor.is_some_and(|item| item.source.starts_with("legacy-nostr-store:instance:")), "old Nostr instance missing");
    }
    let existing_envelope: Option<String> = if old_store { None } else {
        destination.query_row("SELECT credential_envelope FROM instances WHERE instance_id=?1", [instance_id], |row| row.get::<_, Option<String>>(0)).optional()?.flatten()
    };
    let existing = existing_envelope.as_deref().filter(|value| !value.is_empty()).map(|value| decrypt(kind, value, key)).transpose()?;
    if kind == "nostr" && descriptor.is_some_and(|item| item.source == format!("legacy-nostr-store:instance:{instance_id}")) {
        source::require_columns(&destination, "legacy_nostr_instances", &["agent_id", "secret_key"])?;
        let retained: String = destination.query_row(
            "SELECT secret_key FROM legacy_nostr_instances WHERE agent_id=?1", [agent_id], |row| row.get(0),
        )?;
        let clear = decrypt(kind, &retained, key)?;
        ensure!(existing.as_ref().is_some_and(|value| value.as_slice() == clear.as_slice()), "old Nostr credential rerun conflict");
        return Ok((clear, descriptor.unwrap().source.clone()));
    }
    if !enabled && descriptor.is_none() && legacy.is_none() && existing.is_none() {
        return Ok((Zeroizing::new(Vec::new()), String::new()));
    }
    let descriptor = descriptor.context("credential source missing")?;
    let legacy_source = match kind {
        "discord" => format!("legacy-core:agent_discord_config:{agent_id}"),
        "nostr" => format!("legacy-core:agent_nostr_config:{agent_id}"),
        _ => unreachable!(),
    };
    let existing_source = format!("existing-destination:{kind}:{instance_id}");
    match (legacy, existing) {
        (Some(left), Some(right)) => {
            ensure!(left.as_slice() == right.as_slice(), "credential candidates conflict");
            ensure!(descriptor.source == legacy_source || descriptor.source == existing_source, "credential source descriptor mismatch");
            Ok((left, descriptor.source.clone()))
        }
        (Some(value), None) => {
            ensure!(descriptor.source == legacy_source, "credential source descriptor mismatch");
            Ok((value, descriptor.source.clone()))
        }
        (None, Some(value)) => {
            ensure!(descriptor.source == existing_source, "credential source descriptor mismatch");
            Ok((value, descriptor.source.clone()))
        }
        (None, None) => bail!("credential is required"),
    }
}

fn build_identity_plans(rows: &[SourceRow], approval: &Approval, plans: &[InstancePlan]) -> Result<BTreeMap<(String, String), Vec<IdentityPlan>>> {
    let mut output: BTreeMap<(String, String), Vec<IdentityPlan>> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.table == "trusted_users") {
        let disposition = approval
            .identity_dispositions
            .iter()
            .find(|item| item.source_fingerprint == row.fingerprint)
            .context("identity disposition missing")?;
        let platform = row.text("platform")?;
        ensure!(
            if platform == "rest" {
                disposition.edges.len() == 1 && matches!(disposition.edges[0], IdentityEdge::ApiPrincipal)
            } else {
                disposition.edges.iter().all(|edge| matches!(edge, IdentityEdge::Gateway { .. }))
            },
            "REST identity requires only api_principal; gateway identities cannot target core"
        );
        let permission = row.text("permission")?;
        let role = match permission {
            "owner" => "owner",
            "co-agent" => "co_agent",
            _ => "trusted_user",
        };
        for edge in &disposition.edges {
            match edge {
                IdentityEdge::ApiPrincipal => ensure!(row.text("platform")? == "rest", "only rest may become api principal"),
                IdentityEdge::Gateway { kind_id, instance_id } => {
                    let plan = plans
                        .iter()
                        .find(|plan| &plan.instance_id == instance_id && &plan.destination.kind_id == kind_id)
                        .context("identity target instance missing")?;
                    ensure!(plan.agent_id == row.text("agent_id")?, "identity agent mismatch");
                    let active = if kind_id == "web" {
                        ensure!(row.text("user_id")? != "web-local", "Web source identity conflicts with local Owner policy");
                        true
                    } else if plan.legacy_nostr {
                        access_config_contains(plan, row.text("user_id")?, role)?
                    } else {
                        prove_access_config(plan, row.text("user_id")?, role)?;
                        true
                    };
                    let relationship_id = if active && plan.legacy_nostr && role == "co_agent" {
                        let bytes = base64::engine::general_purpose::STANDARD.decode(&plan.config_b64)?;
                        opencrab_nostr_gateway::config::parse_instance_config(&bytes)?.access.co_agents
                            .get(row.text("user_id")?).cloned()
                    } else {
                        (role == "co_agent").then(|| row.text("user_id").unwrap().to_string())
                    };
                    output.entry((plan.destination.kind_id.clone(), plan.destination.path_id.clone())).or_default().push(IdentityPlan {
                        instance_id: instance_id.clone(),
                        role: role.into(),
                        external_id: row.text("user_id")?.into(),
                        relationship_id,
                        active,
                        source: Some(LegacyIdentitySource {
                            instance_id: instance_id.clone(),
                            id: row.text("id")?.into(),
                            user_id: row.text("user_id")?.into(),
                            agent_id: row.text("agent_id")?.into(),
                            permission: row.text("permission")?.into(),
                            created_by: row.text("created_by")?.into(),
                            created_at: row.text("created_at")?.into(),
                            display_name: row.text("display_name")?.into(),
                            platform: row.text("platform")?.into(),
                        }),
                    });
                }
            }
        }
    }
    for plan in plans.iter().filter(|plan| plan.core_only_nostr) {
        let bytes = base64::engine::general_purpose::STANDARD.decode(&plan.config_b64)?;
        let access = opencrab_nostr_gateway::config::parse_instance_config(&bytes)?.access;
        let values = output.entry((plan.destination.kind_id.clone(), plan.destination.path_id.clone())).or_default();
        for (role, external, relationship_id) in access.owner.into_iter().map(|key| ("owner", key, None))
            .chain(access.co_agents.into_iter().map(|(key, agent)| ("co_agent", key, Some(agent)))) {
            if !values.iter().any(|item| item.instance_id == plan.instance_id && item.role == role && item.external_id == external) {
                values.push(IdentityPlan { instance_id:plan.instance_id.clone(),role:role.into(),external_id:external,relationship_id,active:true,source:None });
            }
        }
    }
    for values in output.values_mut() {
        values.sort_by(|a, b| (&a.instance_id, &a.role, &a.external_id).cmp(&(&b.instance_id, &b.role, &b.external_id)));
    }
    Ok(output)
}

fn prove_access_config(plan: &InstancePlan, external_id: &str, role: &str) -> Result<()> {
    ensure!(access_config_contains(plan, external_id, role)?, "identity is not represented by gateway config");
    Ok(())
}

fn access_config_contains(plan: &InstancePlan, external_id: &str, role: &str) -> Result<bool> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(&plan.config_b64)?;
    let matches = match plan.destination.kind_id.as_str() {
        "discord" => {
            let cfg = opencrab_discord_gateway::config::parse_instance_config(&bytes)?;
            match role {
                "owner" => cfg.access.owners.iter().any(|v| v == external_id),
                "co_agent" => cfg.access.co_agents.contains_key(external_id),
                _ => cfg.access.trusted_users.iter().any(|v| v == external_id),
            }
        }
        "nostr" => {
            let cfg = opencrab_nostr_gateway::config::parse_instance_config(&bytes)?;
            match role {
                "owner" => cfg.access.owner.iter().any(|v| v == external_id),
                "co_agent" => cfg.access.co_agents.contains_key(external_id),
                _ => cfg.access.trusted_users.iter().any(|v| v == external_id),
            }
        }
        _ => false,
    };
    Ok(matches)
}

fn build_endpoint_plans(core: &Connection, rows: &[SourceRow], approval: &Approval, plans: &[InstancePlan]) -> Result<BTreeMap<(String, String), Vec<EndpointPlan>>> {
    let mut expected = BTreeSet::new();
    for plan in plans.iter().filter(|plan| plan.destination.kind_id == "discord") {
        let bindings = core.prepare("SELECT binding_id,address,session_id FROM gate_bindings WHERE instance_id=?1 AND closed_at IS NULL")?
            .query_map([&plan.instance_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        for (binding_id,address,session_id) in bindings {
            if let Some((guild,channel)) = opencrab_discord_gateway::map::parse_address(&plan.agent_id, &address) {
                for source in rows.iter().filter(|row| row.table == "channel_config") {
                    let owner = source.text("agent_id")?;
                    if source.text("channel_id")? == channel && source.text("guild_id")? == guild && (owner.is_empty() || owner == plan.agent_id) {
                        expected.insert((source.fingerprint.clone(),plan.instance_id.clone(),binding_id.clone(),session_id.clone()));
                    }
                }
            }
        }
    }
    let actual = approval.channel_edges.iter().map(|e| (e.source_fingerprint.clone(),e.instance_id.clone(),e.binding_id.clone(),e.session_id.clone())).collect::<BTreeSet<_>>();
    ensure!(actual == expected, "approved channel edges must cover every eligible binding exactly");
    let mut grouped: BTreeMap<(String, String, String), (&InstancePlan, Vec<&SourceRow>)> = BTreeMap::new();
    for edge in &approval.channel_edges {
        let source = rows
            .iter()
            .find(|row| row.fingerprint == edge.source_fingerprint && row.table == "channel_config")
            .context("channel source missing")?;
        let plan = plans
            .iter()
            .find(|plan| plan.instance_id == edge.instance_id && plan.destination.kind_id == "discord")
            .context("discord instance missing")?;
        let binding: (String, String) = core.query_row(
            "SELECT instance_id,session_id FROM gate_bindings WHERE binding_id=?1 AND closed_at IS NULL",
            [&edge.binding_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(binding.0 == plan.instance_id && binding.1 == edge.session_id, "channel binding mismatch");
        ensure!(
            core.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_sessions WHERE agent_id=?1 AND session_id=?2)",
                params![plan.agent_id, edge.session_id],
                |row| row.get::<_, bool>(0)
            )?,
            "channel session membership missing"
        );
        let key = (plan.destination.path_id.clone(), plan.instance_id.clone(), source.text("channel_id")?.into());
        grouped.entry(key).or_insert_with(|| (plan, Vec::new())).1.push(source);
    }
    let mut output: BTreeMap<(String, String), Vec<EndpointPlan>> = BTreeMap::new();
    for ((_path_id, instance_id, channel_id), (plan, sources)) in grouped {
        let exact = sources.iter().filter(|row| row.text("agent_id").ok() == Some(plan.agent_id.as_str())).copied().collect::<Vec<_>>();
        let global = sources.iter().filter(|row| row.text("agent_id").ok() == Some("")).copied().collect::<Vec<_>>();
        ensure!(exact.len() <= 1 && global.len() <= 1 && exact.len() + global.len() == sources.len(), "ambiguous channel source rows");
        let effective = exact.first().copied().or_else(|| global.first().copied()).context("channel source has no exact/global row")?;
        let mut fingerprints = sources.iter().map(|row| row.fingerprint.clone()).collect::<Vec<_>>();
        fingerprints.sort();
        let policy = crate::canonical::value_bytes(&json!({"channel_name":effective.text("channel_name")?,"source_fingerprints":fingerprints,"whitelisted":effective.integer("whitelisted")?!=0}))?;
        let guild_id = effective.text("guild_id")?.to_string();
        output.entry((plan.destination.kind_id.clone(), plan.destination.path_id.clone())).or_default().push(EndpointPlan {
            instance_id,
            channel_id,
            guild_id: (!guild_id.is_empty()).then_some(guild_id),
            readable: effective.integer("readable")? != 0,
            writable: effective.integer("writable")? != 0,
            policy_json: String::from_utf8(policy)?,
        });
    }
    for values in output.values_mut() {
        values.sort_by(|a, b| (&a.instance_id, &a.channel_id).cmp(&(&b.instance_id, &b.channel_id)));
    }
    Ok(output)
}

fn prevalidate_watch_edges(core: &Connection, rows: &[SourceRow], approval: &Approval, plans: &[InstancePlan]) -> Result<()> {
    for edge in &approval.watch_edges {
        let row = rows
            .iter()
            .find(|row| row.fingerprint == edge.source_fingerprint && row.table == "session_watches")
            .context("watch source missing")?;
        let plan = plans
            .iter()
            .find(|plan| plan.instance_id == edge.instance_id && plan.destination.kind_id == "nostr")
            .context("nostr watch target missing")?;
        ensure!(plan.agent_id == row.text("agent_id")?, "watch agent mismatch");
        let session = row.text("session_id")?;
        let bound: bool = core.query_row(
            "SELECT EXISTS(SELECT 1 FROM gate_bindings WHERE instance_id=?1 AND session_id=?2 AND closed_at IS NULL)",
            params![plan.instance_id, session],
            |r| r.get(0),
        )?;
        ensure!(bound, "watch session not bound");
        let bytes = base64::engine::general_purpose::STANDARD.decode(&plan.config_b64)?;
        let cfg = opencrab_nostr_gateway::config::parse_instance_config(&bytes)?;
        let watch_id = row.integer("id")?;
        let interval_secs = row.integer("interval_secs")?;
        let source_filter = serde_json::from_str::<Value>(row.text("filter_json")?)?;
        let found = cfg
            .watches
            .iter()
            .any(|watch| watch.id == watch_id && watch.interval_secs == interval_secs && serde_json::to_value(watch.effective_filter()).ok() == Some(source_filter.clone()));
        ensure!(found, "watch not represented by Nostr config");
    }
    Ok(())
}

fn validate_source_coverage(rows: &[SourceRow], approval: &Approval, plans: &[InstancePlan]) -> Result<()> {
    for row in rows.iter().filter(|row| row.table == "session_watches") {
        ensure!(approval.watch_edges.iter().filter(|edge| edge.source_fingerprint == row.fingerprint).count() == 1, "watch requires exactly one edge");
    }
    let identity = approval.identity_dispositions.iter().map(|item| item.source_fingerprint.as_str()).collect::<BTreeSet<_>>();
    let channels = approval.channel_edges.iter().map(|item| item.source_fingerprint.as_str()).collect::<BTreeSet<_>>();
    let watches = approval.watch_edges.iter().map(|item| item.source_fingerprint.as_str()).collect::<BTreeSet<_>>();
    for row in rows {
        match row.table.as_str() {
            "trusted_users" => ensure!(identity.contains(row.fingerprint.as_str()), "unmapped identity"),
            "channel_config" => ensure!(channels.contains(row.fingerprint.as_str()), "unmapped channel"),
            "session_watches" => {
                ensure!(watches.contains(row.fingerprint.as_str()), "unmapped watch")
            }
            "agent_discord_config" | "agent_nostr_config" => {
                let (kind, secret_column) = if row.table == "agent_discord_config" {
                    ("discord", "bot_token")
                } else {
                    ("nostr", "secret_key")
                };
                let agent_id = row.text("agent_id")?;
                let secret = row.text(secret_column)?;
                ensure!(
                    plans.iter().any(|plan| {
                        plan.destination.kind_id == kind
                            && plan.agent_id == agent_id
                            && (plan.legacy_nostr || plan.credential.as_slice() == secret.as_bytes())
                    }),
                    "unmapped credential config"
                );
            },
            _ => {}
        }
    }
    Ok(())
}
