use crate::{
    backup::BackupRecord,
    canonical,
    manifest::{Approval, Destination, IdentityEdge},
    source::SourceRow,
};
use anyhow::{bail, ensure, Context, Result};
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

pub struct Inputs {
    pub paths: BTreeMap<(String, String), PathBuf>,
    pub master_keys: BTreeMap<String, PathBuf>,
    pub credential_files: BTreeMap<String, PathBuf>,
}

#[derive(Debug)]
struct InstancePlan {
    destination: Destination,
    instance_id: String,
    agent_id: String,
    subject_id: i64,
    revision: i64,
    config_b64: String,
    addresses: Vec<String>,
    enabled: bool,
    credential: Zeroizing<Vec<u8>>,
    credential_source: String,
    created_at: String,
}

#[derive(Debug, Clone)]
struct IdentityPlan {
    instance_id: String,
    role: String,
    external_id: String,
    relationship_id: Option<String>,
}
#[derive(Debug, Clone)]
struct EndpointPlan {
    instance_id: String,
    channel_id: String,
    guild_id: Option<String>,
    readable: bool,
    writable: bool,
    policy_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PartialRecord {
    version: u64,
    record_type: String,
    operation_id: String,
    approval_sha256: String,
    backup_set_sha256: String,
    source_core_sha256: String,
    destination: PartialDestination,
    expected_keys: Vec<ExpectedKey>,
    record_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PartialDestination {
    kind_id: String,
    path_id: String,
    schema: String,
    before_file_sha256: String,
    before_logical_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct ExpectedKey {
    table: String,
    key: Vec<String>,
    before_row_sha256: Option<String>,
    expected_row_sha256: String,
}

pub fn initialize_destinations(approval: &Approval, inputs: &Inputs) -> Result<()> {
    ensure!(approval.destinations.len() == inputs.paths.len(), "destination input set mismatch");
    for destination in &approval.destinations {
        let path = inputs.paths.get(&(destination.kind_id.clone(), destination.path_id.clone())).context("missing destination path")?;
        ensure!(!path.is_symlink(), "destination may not be symlink");
        match destination.kind_id.as_str() {
            "discord" => {
                drop(opencrab_discord_gateway::store::DiscordStore::open(path)?);
            }
            "nostr" => {
                drop(opencrab_nostr_gateway::store::NostrStore::open(path)?);
            }
            "web" => {
                drop(opencrab_web_gateway::store::WebStore::open(path)?);
            }
            _ => bail!("unlisted destination kind"),
        }
    }
    Ok(())
}

pub fn import(
    core: &Connection,
    rows: &[SourceRow],
    approval: &Approval,
    inputs: &Inputs,
    backups: &[BackupRecord],
    backup_dir: &Path,
    backup_set_sha256: &str,
    approval_path: &Path,
) -> Result<Value> {
    let plans = build_instance_plans(core, rows, approval, inputs)?;
    let identities = build_identity_plans(rows, approval, &plans)?;
    let endpoints = build_endpoint_plans(core, rows, approval, &plans)?;
    prevalidate_watch_edges(core, rows, approval, &plans)?;
    validate_source_coverage(rows, approval, &plans)?;

    let partial_dir = partial_dir(approval_path)?;
    let mut outputs = Vec::new();
    for destination in &approval.destinations {
        let path = inputs.paths.get(&(destination.kind_id.clone(), destination.path_id.clone())).unwrap();
        let key = read_master_key(inputs.master_keys.get(&destination.kind_id).context("missing master key")?, &destination.kind_id)?;
        let destination_plans = plans.iter().filter(|plan| plan.destination == *destination).collect::<Vec<_>>();
        let destination_identities = identities.get(&(destination.kind_id.clone(), destination.path_id.clone())).cloned().unwrap_or_default();
        let destination_endpoints = endpoints.get(&(destination.kind_id.clone(), destination.path_id.clone())).cloned().unwrap_or_default();
        let backup = backups
            .iter()
            .find(|item| item.kind_id == destination.kind_id && item.path_id == destination.path_id)
            .context("destination backup missing")?;
        let before_path = crate::backup::database_path(backup_dir, &destination.kind_id, &destination.path_id);
        let before = Connection::open_with_flags(before_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let expected = expected_keys(&before, &destination_plans, &destination_identities, &destination_endpoints)?;
        drop(before);
        let artifact = prepare_partial(&partial_dir, approval, destination, backup, backup_set_sha256, expected)?;
        let mut conn = Connection::open(path)?;
        validate_current_against_artifact(&conn, &artifact)?;
        let tx = conn.transaction()?;
        let mut inserted = Vec::new();
        let mut accepted = Vec::new();
        let mut credentials = Vec::new();
        for plan in destination_plans {
            let outcome = apply_instance(&tx, plan, &key)?;
            if outcome.0 {
                inserted.push(key_value("instances", vec![plan.instance_id.clone()], outcome.1.clone())?);
            } else {
                accepted.push(key_value("instances", vec![plan.instance_id.clone()], outcome.1.clone())?);
            }
            credentials.push(json!({"instance_id":plan.instance_id,"source":plan.credential_source,"envelope_sha256":outcome.2,"credential_configured":true}));
        }
        for identity in &destination_identities {
            let (was_inserted, hash) = apply_identity(&tx, identity)?;
            let item = key_value("identity_projections", vec![identity.instance_id.clone(), identity.role.clone(), identity.external_id.clone()], hash)?;
            if was_inserted {
                inserted.push(item);
            } else {
                accepted.push(item);
            }
        }
        for endpoint in &destination_endpoints {
            let (was_inserted, hash) = apply_endpoint(&tx, endpoint)?;
            let item = key_value("endpoints", vec![endpoint.instance_id.clone(), endpoint.channel_id.clone()], hash)?;
            if was_inserted {
                inserted.push(item);
            } else {
                accepted.push(item);
            }
        }
        verify_expected(&tx, &artifact.expected_keys)?;
        tx.commit()?;
        inserted.sort_by(value_key_order);
        accepted.sort_by(value_key_order);
        credentials.sort_by(value_instance_order);
        outputs.push(json!({
            "kind_id":destination.kind_id,"path_id":destination.path_id,"schema":destination.schema,
            "before_logical_sha256":backup.logical_sha256,"after_logical_sha256":crate::backup::logical_sha256(path)?,
            "counts":{"instances":plans.iter().filter(|p| p.destination == *destination).count(),"endpoints":destination_endpoints.len(),"identity_projections":destination_identities.len(),"policies":0,"credentials":credentials.len()},
            "inserted_keys":inserted,"accepted_existing_keys":accepted,"credentials":credentials
        }));
    }
    Ok(Value::Array(outputs))
}

pub fn validate_project_artifacts(
    approval_path: &Path,
    approval: &Approval,
    paths: &BTreeMap<(String, String), PathBuf>,
    backups: &[BackupRecord],
    backup_set_sha256: &str,
    destinations: &Value,
) -> Result<()> {
    ensure!(paths.len() == approval.destinations.len(), "destination input set mismatch");
    let reports = destinations.as_array().context("destinations array")?;
    ensure!(reports.len() == approval.destinations.len(), "destination report set mismatch");
    let partials = partial_dir(approval_path)?;
    validate_secure(&partials, 0o700)?;
    for destination in &approval.destinations {
        let path = paths.get(&(destination.kind_id.clone(), destination.path_id.clone())).context("missing destination input")?;
        let report = reports
            .iter()
            .find(|item| item["kind_id"] == destination.kind_id && item["path_id"] == destination.path_id)
            .context("destination report missing")?;
        let after = report["after_logical_sha256"].as_str().context("after logical digest missing")?;
        ensure!(crate::backup::logical_sha256(path)? == after, "destination changed after import");
        let locator = canonical::hash(&json!({"operation_id":approval.operation_id,"kind_id":destination.kind_id,"path_id":destination.path_id}))?;
        let artifact_path = partials.join(format!("{locator}.json"));
        validate_secure(&artifact_path, 0o600)?;
        let raw = fs::read(&artifact_path)?;
        let artifact: PartialRecord = serde_json::from_slice(&raw)?;
        ensure!(raw == canonical::bytes(&artifact)?, "partial artifact is not canonical");
        ensure!(artifact.record_sha256 == partial_hash(&artifact)?, "partial artifact hash mismatch");
        let backup = backups
            .iter()
            .find(|item| item.kind_id == destination.kind_id && item.path_id == destination.path_id)
            .context("destination backup missing")?;
        ensure!(artifact.operation_id == approval.operation_id, "partial operation mismatch");
        ensure!(artifact.approval_sha256 == approval.sha256()?, "partial approval mismatch");
        ensure!(artifact.backup_set_sha256 == backup_set_sha256, "partial backup set mismatch");
        ensure!(artifact.source_core_sha256 == approval.source_core_sha256, "partial source mismatch");
        ensure!(
            artifact.destination.kind_id == destination.kind_id && artifact.destination.path_id == destination.path_id && artifact.destination.schema == destination.schema,
            "partial destination mismatch"
        );
        ensure!(
            artifact.destination.before_file_sha256 == backup.file_sha256 && artifact.destination.before_logical_sha256 == backup.logical_sha256,
            "partial before-backup mismatch"
        );
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        validate_current_against_artifact(&conn, &artifact)?;
    }
    Ok(())
}

fn build_instance_plans(core: &Connection, rows: &[SourceRow], approval: &Approval, inputs: &Inputs) -> Result<Vec<InstancePlan>> {
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
    for (instance_id, kind, agent_id, subject_id, revision, enabled, config_b64, digest) in raw {
        if !matches!(kind.as_str(), "discord" | "nostr" | "web") {
            continue;
        }
        let destination = approval.destination(&kind)?.clone();
        let canonical = match kind.as_str() {
            "discord" => opencrab_discord_gateway::config::canonicalize_config_b64(&config_b64)?,
            "nostr" => opencrab_nostr_gateway::config::canonicalize_config_b64(&config_b64)?,
            "web" => config_b64.clone(),
            _ => unreachable!(),
        };
        ensure!(canonical == config_b64, "noncanonical config");
        let decoded = base64::engine::general_purpose::STANDARD.decode(&config_b64)?;
        ensure!(canonical::hex(&Sha256::digest(&decoded)) == digest, "config digest mismatch");
        let mut addresses = core
            .prepare("SELECT address FROM gate_bindings WHERE instance_id=?1 AND closed_at IS NULL ORDER BY address")?
            .query_map([&instance_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        addresses.sort();
        let key_path = inputs.master_keys.get(&kind).context("missing master key")?;
        let key = read_master_key(key_path, &kind)?;
        let destination_path = inputs.paths.get(&(kind.clone(), destination.path_id.clone())).context("missing destination")?;
        let (credential, source) = select_credential(rows, approval, inputs, destination_path, &kind, &instance_id, &agent_id, &key)?;
        plans.push(InstancePlan {
            destination,
            instance_id,
            agent_id,
            subject_id,
            revision,
            config_b64,
            addresses,
            enabled,
            credential,
            credential_source: source,
            created_at: approval.created_at.clone(),
        });
    }
    Ok(plans)
}

fn select_credential(rows: &[SourceRow], approval: &Approval, inputs: &Inputs, path: &Path, kind: &str, instance_id: &str, agent_id: &str, key: &[u8; 32]) -> Result<(Zeroizing<Vec<u8>>, String)> {
    let descriptor = approval.credential_sources.iter().find(|item| item.instance_id == instance_id).context("credential source missing")?;
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
        "web" => inputs.credential_files.get(instance_id).map(|file| read_secret_file(file)).transpose()?,
        _ => None,
    }
    .filter(|value| !value.is_empty());
    let existing_envelope: Option<String> = Connection::open(path)?
        .query_row("SELECT credential_envelope FROM instances WHERE instance_id=?1", [instance_id], |row| row.get(0))
        .optional()?
        .flatten();
    let existing = existing_envelope.as_deref().map(|value| decrypt(kind, value, key)).transpose()?;
    let legacy_source = match kind {
        "discord" => format!("legacy-core:agent_discord_config:{agent_id}"),
        "nostr" => format!("legacy-core:agent_nostr_config:{agent_id}"),
        "web" => format!("operator-file:{instance_id}"),
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
                    prove_access_config(plan, row.text("user_id")?, role)?;
                    output.entry((plan.destination.kind_id.clone(), plan.destination.path_id.clone())).or_default().push(IdentityPlan {
                        instance_id: instance_id.clone(),
                        role: role.into(),
                        external_id: row.text("user_id")?.into(),
                        relationship_id: (role == "co_agent").then(|| row.text("user_id").unwrap().to_string()),
                    });
                }
            }
        }
    }
    for values in output.values_mut() {
        values.sort_by(|a, b| (&a.instance_id, &a.role, &a.external_id).cmp(&(&b.instance_id, &b.role, &b.external_id)));
    }
    Ok(output)
}

fn prove_access_config(plan: &InstancePlan, external_id: &str, role: &str) -> Result<()> {
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
        "web" => matches!(role, "owner" | "trusted_user"),
        _ => false,
    };
    ensure!(matches, "identity is not represented by gateway config");
    Ok(())
}

fn build_endpoint_plans(core: &Connection, rows: &[SourceRow], approval: &Approval, plans: &[InstancePlan]) -> Result<BTreeMap<(String, String), Vec<EndpointPlan>>> {
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
            "agent_discord_config" | "agent_nostr_config" => ensure!(plans.iter().any(|p| p.agent_id == row.text("agent_id").unwrap_or("")), "unmapped credential config"),
            _ => {}
        }
    }
    Ok(())
}

fn expected_keys(conn: &Connection, plans: &[&InstancePlan], identities: &[IdentityPlan], endpoints: &[EndpointPlan]) -> Result<Vec<ExpectedKey>> {
    let mut out = Vec::new();
    for plan in plans {
        let expected = instance_semantic(plan);
        out.push(expected_key(conn, "instances", vec![plan.instance_id.clone()], expected)?);
    }
    for item in identities {
        out.push(expected_key(
            conn,
            "identity_projections",
            vec![item.instance_id.clone(), item.role.clone(), item.external_id.clone()],
            identity_semantic(item),
        )?);
    }
    for item in endpoints {
        out.push(expected_key(conn, "endpoints", vec![item.instance_id.clone(), item.channel_id.clone()], endpoint_semantic(item))?);
    }
    out.sort();
    Ok(out)
}
fn expected_key(conn: &Connection, table: &str, key: Vec<String>, expected: Value) -> Result<ExpectedKey> {
    let current = current_semantic(conn, table, &key)?;
    let before_row_sha256 = current.as_ref().map(|row| row_hash(table, &key, row)).transpose()?;
    let expected_row_sha256 = row_hash(table, &key, &expected)?;
    Ok(ExpectedKey {
        table: table.into(),
        key,
        before_row_sha256,
        expected_row_sha256,
    })
}
fn prepare_partial(dir: &Path, approval: &Approval, destination: &Destination, backup: &BackupRecord, backup_set_sha256: &str, expected_keys: Vec<ExpectedKey>) -> Result<PartialRecord> {
    if !dir.exists() {
        fs::create_dir(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    validate_secure(dir, 0o700)?;
    reject_overlapping_operations(dir, approval, destination, &expected_keys)?;
    let locator = canonical::hash(&json!({"operation_id":approval.operation_id,"kind_id":destination.kind_id,"path_id":destination.path_id}))?;
    let path = dir.join(format!("{locator}.json"));
    let mut record = PartialRecord {
        version: 1,
        record_type: "opencrab-s8-partial-destination".into(),
        operation_id: approval.operation_id.clone(),
        approval_sha256: approval.sha256()?,
        backup_set_sha256: backup_set_sha256.into(),
        source_core_sha256: approval.source_core_sha256.clone(),
        destination: PartialDestination {
            kind_id: destination.kind_id.clone(),
            path_id: destination.path_id.clone(),
            schema: destination.schema.clone(),
            before_file_sha256: backup.file_sha256.clone(),
            before_logical_sha256: backup.logical_sha256.clone(),
        },
        expected_keys,
        record_sha256: String::new(),
    };
    record.record_sha256 = partial_hash(&record)?;
    let content = canonical::bytes(&record)?;
    match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
        Ok(mut file) => {
            file.write_all(&content)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_secure(&path, 0o600)?;
            let existing: PartialRecord = serde_json::from_slice(&fs::read(&path)?)?;
            ensure!(existing == record, "partial artifact mismatch");
            ensure!(partial_hash(&existing)? == existing.record_sha256, "partial artifact hash mismatch");
        }
        Err(error) => return Err(error.into()),
    }
    Ok(record)
}
fn partial_hash(record: &PartialRecord) -> Result<String> {
    let mut copy = record.clone();
    copy.record_sha256.clear();
    let mut value = serde_json::to_value(copy)?;
    value.as_object_mut().unwrap().remove("record_sha256");
    canonical::hash(&value)
}
fn partial_dir(approval_path: &Path) -> Result<PathBuf> {
    let name = approval_path.file_name().context("approval filename")?.to_string_lossy();
    Ok(approval_path.with_file_name(format!("{name}.partials")))
}

fn reject_overlapping_operations(dir: &Path, approval: &Approval, destination: &Destination, expected: &[ExpectedKey]) -> Result<()> {
    let current_keys = expected.iter().map(|item| (&item.table, &item.key)).collect::<BTreeSet<_>>();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        validate_secure(&path, 0o600)?;
        let raw = fs::read(&path)?;
        let record: PartialRecord = serde_json::from_slice(&raw)?;
        ensure!(raw == canonical::bytes(&record)?, "partial artifact is not canonical");
        ensure!(record.record_sha256 == partial_hash(&record)?, "partial artifact hash mismatch");
        let locator = canonical::hash(&json!({"operation_id":record.operation_id,"kind_id":record.destination.kind_id,"path_id":record.destination.path_id}))?;
        ensure!(path.file_name().and_then(|name| name.to_str()) == Some(&format!("{locator}.json")), "partial artifact locator mismatch");
        if record.operation_id != approval.operation_id
            && record.destination.kind_id == destination.kind_id
            && record.destination.path_id == destination.path_id
            && record.expected_keys.iter().any(|item| current_keys.contains(&(&item.table, &item.key)))
        {
            bail!("overlapping migration operation");
        }
    }
    Ok(())
}

fn validate_current_against_artifact(conn: &Connection, artifact: &PartialRecord) -> Result<()> {
    for item in &artifact.expected_keys {
        if let Some(before) = &item.before_row_sha256 {
            ensure!(before == &item.expected_row_sha256, "approved destination row conflicts with before-backup");
        }
        match current_semantic(conn, &item.table, &item.key)? {
            Some(row) => ensure!(row_hash(&item.table, &item.key, &row)? == item.expected_row_sha256, "current destination row conflicts with artifact"),
            None => ensure!(item.before_row_sha256.is_none(), "destination row disappeared after backup"),
        }
    }
    Ok(())
}

fn apply_instance(tx: &Transaction<'_>, plan: &InstancePlan, key: &[u8; 32]) -> Result<(bool, String, String)> {
    let expected = instance_semantic(plan);
    let expected_hash = row_hash("instances", &[plan.instance_id.clone()], &expected)?;
    if let Some(existing) = current_semantic(tx, "instances", &[plan.instance_id.clone()])? {
        ensure!(existing == expected, "instance conflict");
        let envelope: String = tx.query_row("SELECT credential_envelope FROM instances WHERE instance_id=?1", [&plan.instance_id], |r| r.get(0))?;
        ensure!(decrypt(&plan.destination.kind_id, &envelope, key)?.as_slice() == plan.credential.as_slice(), "credential conflict");
        return Ok((false, expected_hash, canonical::hex(&Sha256::digest(envelope.as_bytes()))));
    }
    let envelope = encrypt(&plan.destination.kind_id, &plan.credential, key)?;
    match plan.destination.kind_id.as_str() {
        "discord" | "nostr" => {
            tx.execute("INSERT INTO instances(instance_id,agent_id,subject_id,config_b64,addresses_json,credential_envelope,subject_grant_envelope,enabled,desired_generation,applied_generation,lifecycle_state,core_revision,core_digest,binding_inventory_json,process_id,process_nonce,failure_count,retry_at_unix_ms,last_exit,updated_at) VALUES (?1,?2,?3,?4,?5,?6,NULL,?7,1,NULL,'pending',NULL,NULL,'[]',NULL,NULL,0,NULL,NULL,?8)",params![plan.instance_id,plan.agent_id,plan.subject_id,plan.config_b64,serde_json::to_string(&plan.addresses)?,envelope,plan.enabled,plan.created_at])?;
        }
        "web" => {
            tx.execute(
                "INSERT INTO instances(instance_id,agent_id,revision,author_id,credential_envelope,enabled,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![plan.instance_id, plan.agent_id, plan.revision, plan.agent_id, envelope, plan.enabled, plan.created_at],
            )?;
        }
        _ => bail!("unknown destination"),
    }
    Ok((true, expected_hash, canonical::hex(&Sha256::digest(envelope.as_bytes()))))
}
fn apply_identity(tx: &Transaction<'_>, item: &IdentityPlan) -> Result<(bool, String)> {
    let expected = identity_semantic(item);
    let key = [item.instance_id.clone(), item.role.clone(), item.external_id.clone()];
    let hash = row_hash("identity_projections", &key, &expected)?;
    if let Some(existing) = current_semantic(tx, "identity_projections", &[item.instance_id.clone(), item.role.clone(), item.external_id.clone()])? {
        ensure!(existing == expected, "identity conflict");
        return Ok((false, hash));
    }
    tx.execute(
        "INSERT INTO identity_projections(instance_id,role,external_id,relationship_id,relationship_revision) VALUES (?1,?2,?3,?4,NULL)",
        params![item.instance_id, item.role, item.external_id, item.relationship_id],
    )?;
    Ok((true, hash))
}
fn apply_endpoint(tx: &Transaction<'_>, item: &EndpointPlan) -> Result<(bool, String)> {
    let expected = endpoint_semantic(item);
    let key = [item.instance_id.clone(), item.channel_id.clone()];
    let hash = row_hash("endpoints", &key, &expected)?;
    if let Some(existing) = current_semantic(tx, "endpoints", &[item.instance_id.clone(), item.channel_id.clone()])? {
        ensure!(existing == expected, "endpoint conflict");
        return Ok((false, hash));
    }
    tx.execute(
        "INSERT INTO endpoints(instance_id,channel_id,guild_id,readable,writable,policy_json) VALUES (?1,?2,?3,?4,?5,?6)",
        params![item.instance_id, item.channel_id, item.guild_id, item.readable, item.writable, item.policy_json],
    )?;
    Ok((true, hash))
}
fn verify_expected(conn: &Connection, expected: &[ExpectedKey]) -> Result<()> {
    for item in expected {
        let current = current_semantic(conn, &item.table, &item.key)?.context("expected destination key missing")?;
        ensure!(row_hash(&item.table, &item.key, &current)? == item.expected_row_sha256, "destination verification mismatch");
    }
    Ok(())
}

fn instance_semantic(plan: &InstancePlan) -> Value {
    json!({"agent_id":plan.agent_id,"addresses":plan.addresses,"config_b64":plan.config_b64,"enabled":plan.enabled,"instance_id":plan.instance_id,"revision":plan.revision,"subject_id":plan.subject_id})
}
fn identity_semantic(item: &IdentityPlan) -> Value {
    json!({"external_id":item.external_id,"instance_id":item.instance_id,"relationship_id":item.relationship_id,"relationship_revision":null,"role":item.role})
}
fn endpoint_semantic(item: &EndpointPlan) -> Value {
    json!({"channel_id":item.channel_id,"guild_id":item.guild_id,"instance_id":item.instance_id,"policy_json":item.policy_json,"readable":item.readable,"writable":item.writable})
}
fn current_semantic(conn: &Connection, table: &str, key: &[String]) -> Result<Option<Value>> {
    match table{
 "instances"=>{let common=conn.query_row("SELECT agent_id,subject_id,config_b64,addresses_json,enabled FROM instances WHERE instance_id=?1",[&key[0]],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,bool>(4)?))).optional();match common{Ok(Some((agent,subject,config,addresses,enabled)))=>Ok(Some(json!({"agent_id":agent,"addresses":serde_json::from_str::<Vec<String>>(&addresses)?,"config_b64":config,"enabled":enabled,"instance_id":key[0],"revision":1,"subject_id":subject}))),Ok(None)=>Ok(None),Err(_)=>{let web=conn.query_row("SELECT agent_id,revision,enabled FROM instances WHERE instance_id=?1",[&key[0]],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,bool>(2)?))).optional()?;Ok(web.map(|(agent,revision,enabled)|json!({"agent_id":agent,"addresses":[],"config_b64":"","enabled":enabled,"instance_id":key[0],"revision":revision,"subject_id":0})))}}},
 "identity_projections"=>Ok(conn.query_row("SELECT relationship_id,relationship_revision FROM identity_projections WHERE instance_id=?1 AND role=?2 AND external_id=?3",params![key[0],key[1],key[2]],|r|Ok(json!({"external_id":key[2],"instance_id":key[0],"relationship_id":r.get::<_,Option<String>>(0)?,"relationship_revision":r.get::<_,Option<i64>>(1)?,"role":key[1]}))).optional()?),
 "endpoints"=>Ok(conn.query_row("SELECT guild_id,readable,writable,policy_json FROM endpoints WHERE instance_id=?1 AND channel_id=?2",params![key[0],key[1]],|r|Ok(json!({"channel_id":key[1],"guild_id":r.get::<_,Option<String>>(0)?,"instance_id":key[0],"policy_json":r.get::<_,String>(3)?,"readable":r.get::<_,bool>(1)?,"writable":r.get::<_,bool>(2)?}))).optional()?), _=>bail!("unsupported expected table")}
}
fn row_hash(table: &str, key: &[String], row: &Value) -> Result<String> {
    canonical::hash(&json!({"table":table,"key":key,"row":row}))
}
fn key_value(table: &str, key: Vec<String>, hash: String) -> Result<Value> {
    Ok(json!({"table":table,"key":key,"row_sha256":hash}))
}
fn value_key_order(a: &Value, b: &Value) -> std::cmp::Ordering {
    a.to_string().cmp(&b.to_string())
}
fn value_instance_order(a: &Value, b: &Value) -> std::cmp::Ordering {
    a["instance_id"].as_str().cmp(&b["instance_id"].as_str())
}

fn read_master_key(path: &Path, kind: &str) -> Result<Zeroizing<[u8; 32]>> {
    let value = String::from_utf8(read_secret_file(path)?.to_vec())?;
    match kind {
        "discord" => opencrab_discord_gateway::secret_store::parse_master_key(&value),
        "nostr" => opencrab_nostr_gateway::secret_store::parse_master_key(&value),
        "web" => opencrab_web_gateway::secret_store::parse_master_key(&value),
        _ => bail!("unknown key kind"),
    }
}
fn encrypt(kind: &str, clear: &[u8], key: &[u8; 32]) -> Result<String> {
    match kind {
        "discord" => opencrab_discord_gateway::secret_store::encrypt(clear, key),
        "nostr" => opencrab_nostr_gateway::secret_store::encrypt(clear, key),
        "web" => opencrab_web_gateway::secret_store::encrypt(clear, key),
        _ => bail!("unknown key kind"),
    }
}
fn decrypt(kind: &str, envelope: &str, key: &[u8; 32]) -> Result<Zeroizing<Vec<u8>>> {
    match kind {
        "discord" => opencrab_discord_gateway::secret_store::decrypt(envelope, key),
        "nostr" => opencrab_nostr_gateway::secret_store::decrypt(envelope, key),
        "web" => opencrab_web_gateway::secret_store::decrypt(envelope, key),
        _ => bail!("unknown key kind"),
    }
}
fn read_secret_file(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    validate_secure(path, 0o600)?;
    Ok(Zeroizing::new(fs::read(path)?))
}
fn validate_secure(path: &Path, mode: u32) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.file_type().is_symlink() && ((mode == 0o700 && meta.is_dir()) || (mode == 0o600 && meta.is_file())),
        "unsafe file type"
    );
    ensure!(meta.uid() == unsafe { libc::geteuid() }, "wrong owner");
    ensure!(meta.mode() & 0o777 == mode, "wrong mode");
    Ok(())
}

pub fn prevalidate(core: &Connection, rows: &[SourceRow], approval: &Approval, inputs: &Inputs) -> Result<()> {
    ensure!(approval.destinations.len() == inputs.paths.len(), "destination input set mismatch");
    for destination in &approval.destinations {
        let path = inputs.paths.get(&(destination.kind_id.clone(), destination.path_id.clone())).context("destination path missing")?;
        ensure!(path.exists() && !path.is_symlink(), "destination must be an existing regular database path");
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        for table in match destination.kind_id.as_str() {
            "discord" | "nostr" => vec!["instances", "endpoints", "identity_projections"],
            "web" => vec!["instances", "identity_projections", "policies"],
            _ => bail!("unknown destination kind"),
        } {
            let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?;
            ensure!(exists, "destination schema missing {table}");
        }
    }
    let plans = build_instance_plans(core, rows, approval, inputs)?;
    let identities = build_identity_plans(rows, approval, &plans)?;
    let endpoints = build_endpoint_plans(core, rows, approval, &plans)?;
    prevalidate_watch_edges(core, rows, approval, &plans)?;
    validate_source_coverage(rows, approval, &plans)?;
    for destination in &approval.destinations {
        let path = inputs.paths.get(&(destination.kind_id.clone(), destination.path_id.clone())).unwrap();
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let destination_plans = plans.iter().filter(|plan| plan.destination == *destination).collect::<Vec<_>>();
        let ids = identities.get(&(destination.kind_id.clone(), destination.path_id.clone())).cloned().unwrap_or_default();
        let eps = endpoints.get(&(destination.kind_id.clone(), destination.path_id.clone())).cloned().unwrap_or_default();
        let _ = expected_keys(&conn, &destination_plans, &ids, &eps)?;
    }
    Ok(())
}

#[cfg(test)]
mod s8_review_red_tests {
    use super::*;
    use crate::manifest::{Approval, IdentityDisposition};

    fn approval(destination: Destination) -> Approval {
        Approval { version:1, operation_id:"00000000-0000-4000-8000-000000000008".into(), created_at:"2026-01-01T00:00:00Z".into(), core_user_version:56, source_core_sha256:"a".repeat(64), destinations:vec![destination], identity_dispositions:vec![], channel_edges:vec![], watch_edges:vec![], credential_sources:vec![] }
    }

    #[test]
    fn destination_schema_identifier_is_exact() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("discord.db");
        drop(opencrab_discord_gateway::store::DiscordStore::open(&db).unwrap());
        let destination = Destination { kind_id:"discord".into(), path_id:"main".into(), schema:"wrong".into() };
        let inputs = Inputs { paths:BTreeMap::from([(("discord".into(),"main".into()),db)]), master_keys:BTreeMap::new(), credential_files:BTreeMap::new() };
        assert!(initialize_destinations(&approval(destination), &inputs).is_err());
    }

    #[test]
    fn rest_identity_has_exactly_one_api_principal_edge() {
        let row = SourceRow { table:"trusted_users".into(), fingerprint:"1".repeat(64), columns:vec![
            ("user_id".into(), crate::source::Cell::Text("123456789012345678".into())),
            ("agent_id".into(), crate::source::Cell::Text("agent-a".into())),
            ("permission".into(), crate::source::Cell::Text("user".into())),
            ("platform".into(), crate::source::Cell::Text("rest".into())),
        ]};
        let cfg = serde_json::json!({"agent_id":"agent-a","self_bot_id":"99","access":{"owners":[],"co_agents":{},"trusted_users":["123456789012345678"]},"system_reactions":{}});
        let raw = serde_json::to_vec(&cfg).unwrap();
        let config_b64 = opencrab_discord_gateway::config::canonicalize_config_b64(&base64::engine::general_purpose::STANDARD.encode(raw)).unwrap();
        let destination = Destination { kind_id:"discord".into(), path_id:"main".into(), schema:"s5-discord-v1".into() };
        let plan = InstancePlan { destination:destination.clone(), instance_id:"i".into(), agent_id:"agent-a".into(), subject_id:1, revision:1, config_b64, addresses:vec![], enabled:true, credential:Zeroizing::new(vec![1]), credential_source:"x".into(), created_at:"2026-01-01T00:00:00Z".into() };
        let mut approval = approval(destination);
        approval.identity_dispositions.push(IdentityDisposition { source_fingerprint:row.fingerprint.clone(), edges:vec![IdentityEdge::Gateway { kind_id:"discord".into(), instance_id:"i".into() }] });
        assert!(build_identity_plans(&[row], &approval, &[plan]).is_err());
    }

    #[test]
    fn progressed_instance_semantic_includes_lifecycle_lineage() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("discord.db");
        drop(opencrab_discord_gateway::store::DiscordStore::open(&db).unwrap());
        let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO instances(instance_id,agent_id,subject_id,config_b64,addresses_json,credential_envelope,enabled,desired_generation,applied_generation,lifecycle_state,core_revision,core_digest,binding_inventory_json,failure_count,updated_at) VALUES ('i','a',1,'e30=','[]','enc:v1:x',1,2,2,'running',7,'digest','[\"b\"]',0,'t')", []).unwrap();
        let row = current_semantic(&conn, "instances", &["i".into()]).unwrap().unwrap();
        assert_eq!(row["desired_generation"], 2);
        assert_eq!(row["core_revision"], 7);
        assert_eq!(row["binding_inventory"], json!(["b"]));
    }

    #[test]
    fn existing_web_instance_uses_persisted_author_and_role_without_creation() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("web.db");
        let store = opencrab_web_gateway::store::WebStore::open(&db).unwrap();
        store.upsert("web-i", "agent-a", 3, "author-42", Some("secret"), true, &[9u8; 32]).unwrap();
        store.set_caller_role("web-i", "trusted_user").unwrap();
        drop(store);
        let conn = Connection::open(&db).unwrap();
        let row = current_semantic(&conn, "instances", &["web-i".into()]).unwrap().unwrap();
        assert_eq!(row["author_id"], "author-42");
        assert_eq!(row["caller_role"], "trusted_user");
    }

    #[test]
    fn complete_channel_binding_edge_set_is_validated() {
        let source = include_str!("destination.rs");
        assert!(source.contains("validate_complete_channel_edges"), "all eligible open bindings require exact edge-set validation");
    }

    #[test]
    fn each_watch_row_has_exactly_one_edge() {
        let row = SourceRow { table:"session_watches".into(), fingerprint:"2".repeat(64), columns:vec![] };
        let destination = Destination { kind_id:"nostr".into(), path_id:"main".into(), schema:"s5-nostr-v1".into() };
        let mut approval = approval(destination);
        approval.watch_edges = vec![
            crate::manifest::WatchEdge{source_fingerprint:row.fingerprint.clone(),instance_id:"i".into()},
            crate::manifest::WatchEdge{source_fingerprint:row.fingerprint.clone(),instance_id:"i".into()},
        ];
        assert!(validate_source_coverage(&[row], &approval, &[]).is_err());
    }

    #[test]
    fn secret_files_are_loaded_once_before_planning() {
        let source = include_str!("destination.rs");
        assert!(source.contains("struct LoadedSecrets"), "inputs must own read-once zeroizing secret bytes");
        assert_eq!(source.matches("read_master_key(").count(), 2, "one definition plus one loader call only");
    }
}
