use crate::{
    backup::BackupRecord,
    canonical,
    manifest::{Approval, Destination, IdentityEdge},
    source::{self, SourceRow},
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
    binding_ids: Vec<String>,
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
    source: LegacyIdentitySource,
}
#[derive(Debug, Clone)]
struct LegacyIdentitySource {
    instance_id: String,
    id: String,
    user_id: String,
    agent_id: String,
    permission: String,
    created_by: String,
    created_at: String,
    display_name: String,
    platform: String,
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
        let key = (destination.kind_id != "web")
            .then(|| read_master_key(inputs.master_keys.get(&destination.kind_id).context("missing master key")?, &destination.kind_id))
            .transpose()?;
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
        create_legacy_identity_sources(&tx)?;
        let mut inserted = Vec::new();
        let mut accepted = Vec::new();
        let mut credentials = Vec::new();
        for plan in destination_plans {
            let outcome = apply_instance(&tx, plan, key.as_deref())?;
            if outcome.0 {
                inserted.push(key_value("instances", vec![plan.instance_id.clone()], outcome.1.clone())?);
            } else {
                accepted.push(key_value("instances", vec![plan.instance_id.clone()], outcome.1.clone())?);
            }
            if let Some(envelope_sha256) = outcome.2 {
                credentials.push(json!({"instance_sha256":canonical::hash(&plan.instance_id)?,"source":credential_source_category(&plan.credential_source, &destination.kind_id)?,"envelope_sha256":envelope_sha256,"credential_configured":true}));
            }
        }
        for identity in &destination_identities {
            let (was_inserted, hash) = apply_identity(&tx, identity)?;
            let item = key_value("identity_projections", vec![identity.instance_id.clone(), identity.role.clone(), identity.external_id.clone()], hash)?;
            if was_inserted {
                inserted.push(item);
            } else {
                accepted.push(item);
            }
            let (was_inserted, hash) = apply_legacy_identity_source(&tx, &identity.source)?;
            let item = key_value("legacy_identity_sources", vec![identity.source.instance_id.clone(), identity.source.id.clone()], hash)?;
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
            "counts":{"instances":plans.iter().filter(|p| p.destination == *destination).count(),"endpoints":destination_endpoints.len(),"identity_projections":destination_identities.len(),"legacy_identity_sources":destination_identities.len(),"policies":0,"credentials":credentials.len()},
            "inserted_keys":inserted,"accepted_existing_keys":accepted,"credentials":credentials
        }));
    }
    Ok(Value::Array(outputs))
}

fn credential_source_category(source: &str, kind: &str) -> Result<&'static str> {
    match kind {
        "discord" if source.starts_with("legacy-core:agent_discord_config:") => Ok("legacy-core:agent_discord_config"),
        "nostr" if source.starts_with("legacy-core:agent_nostr_config:") => Ok("legacy-core:agent_nostr_config"),
        "discord" if source.starts_with("existing-destination:discord:") => Ok("existing-destination:discord"),
        "nostr" if source.starts_with("existing-destination:nostr:") => Ok("existing-destination:nostr"),
        _ => anyhow::bail!("credential source category mismatch"),
    }
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

include!("destination_plans.rs");
include!("destination_identity_sources.rs");

fn expected_keys(conn: &Connection, plans: &[&InstancePlan], identities: &[IdentityPlan], endpoints: &[EndpointPlan]) -> Result<Vec<ExpectedKey>> {
    let mut out = Vec::new();
    for plan in plans {
        validate_instance_progress(conn, plan)?;
        let expected = instance_semantic(plan)?;
        out.push(expected_key(conn, "instances", vec![plan.instance_id.clone()], expected)?);
    }
    for item in identities {
        out.push(expected_key(
            conn,
            "identity_projections",
            vec![item.instance_id.clone(), item.role.clone(), item.external_id.clone()],
            identity_semantic(item),
        )?);
        out.push(expected_key(
            conn,
            "legacy_identity_sources",
            vec![item.source.instance_id.clone(), item.source.id.clone()],
            legacy_identity_source_semantic(&item.source),
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

fn apply_instance(tx: &Transaction<'_>, plan: &InstancePlan, key: Option<&[u8; 32]>) -> Result<(bool, String, Option<String>)> {
    let expected = instance_semantic(plan)?;
    let expected_hash = row_hash("instances", &[plan.instance_id.clone()], &expected)?;
    if let Some(existing) = current_semantic(tx, "instances", &[plan.instance_id.clone()])? {
        ensure!(existing == expected, "instance conflict");
        validate_instance_progress(tx, plan)?;
        if plan.destination.kind_id == "web" {
            return Ok((false, expected_hash, None));
        }
        let envelope: String = tx.query_row("SELECT credential_envelope FROM instances WHERE instance_id=?1", [&plan.instance_id], |r| r.get(0))?;
        if plan.credential.is_empty() {
            ensure!(!plan.enabled && envelope.is_empty(), "credential conflict");
            return Ok((false, expected_hash, None));
        }
        ensure!(decrypt(&plan.destination.kind_id, &envelope, key.context("missing destination key")?)?.as_slice() == plan.credential.as_slice(), "credential conflict");
        return Ok((false, expected_hash, Some(canonical::hex(&Sha256::digest(envelope.as_bytes())))));
    }
    ensure!(plan.destination.kind_id != "web", "Web instance must already exist");
    let envelope = if plan.credential.is_empty() {
        ensure!(!plan.enabled, "enabled instance requires credential");
        String::new()
    } else {
        encrypt(&plan.destination.kind_id, &plan.credential, key.context("missing destination key")?)?
    };
    match plan.destination.kind_id.as_str() {
        "discord" | "nostr" => {
            tx.execute("INSERT INTO instances(instance_id,agent_id,subject_id,config_b64,addresses_json,credential_envelope,subject_grant_envelope,enabled,desired_generation,applied_generation,lifecycle_state,core_revision,core_digest,binding_inventory_json,process_id,process_nonce,failure_count,retry_at_unix_ms,last_exit,updated_at) VALUES (?1,?2,?3,?4,?5,?6,NULL,?7,1,NULL,'pending',NULL,NULL,'[]',NULL,NULL,0,NULL,NULL,?8)",params![plan.instance_id,plan.agent_id,plan.subject_id,plan.config_b64,serde_json::to_string(&plan.addresses)?,envelope,plan.enabled,plan.created_at])?;
        }
        _ => bail!("unknown destination"),
    }
    let envelope_sha256 = (!envelope.is_empty()).then(|| canonical::hex(&Sha256::digest(envelope.as_bytes())));
    Ok((true, expected_hash, envelope_sha256))
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

fn validate_instance_progress(conn: &Connection, plan: &InstancePlan) -> Result<()> {
    if plan.destination.kind_id == "web" { return Ok(()); }
    let current = conn.query_row(
        "SELECT applied_generation,core_revision,core_digest,binding_inventory_json FROM instances WHERE instance_id=?1",
        [&plan.instance_id],
        |r| Ok((r.get::<_, Option<i64>>(0)?,r.get::<_, Option<i64>>(1)?,r.get::<_, Option<String>>(2)?,r.get::<_, String>(3)?)),
    ).optional()?;
    if let Some((applied, revision, digest, inventory)) = current {
        if applied.is_some() || revision.is_some() || digest.is_some() {
            let decoded = base64::engine::general_purpose::STANDARD.decode(&plan.config_b64)?;
            ensure!(revision == Some(plan.revision) && digest.as_deref() == Some(canonical::hex(&Sha256::digest(&decoded)).as_str()), "existing instance core revision/digest conflict");
            let mut bound: Vec<String> = serde_json::from_str(&inventory)?;
            bound.sort();
            ensure!(bound == plan.binding_ids, "existing instance binding inventory conflict");
        }
    }
    Ok(())
}

fn instance_semantic(plan: &InstancePlan) -> Result<Value> {
    if plan.destination.kind_id == "web" {
        let decoded = base64::engine::general_purpose::STANDARD.decode(&plan.config_b64)?;
        let config: Value = serde_json::from_slice(&decoded)?;
        let author_id = config.get("author_id").and_then(Value::as_str).filter(|id| !id.is_empty()).context("Web core config author_id missing")?;
        return Ok(json!({"agent_id":plan.agent_id,"author_id":author_id,"enabled":plan.enabled,"instance_id":plan.instance_id,"revision":plan.revision}));
    }
    Ok(json!({"agent_id":plan.agent_id,"addresses":plan.addresses,"config_b64":plan.config_b64,"enabled":plan.enabled,"instance_id":plan.instance_id,"revision":plan.revision,"subject_id":plan.subject_id}))
}
fn identity_semantic(item: &IdentityPlan) -> Value {
    json!({"external_id":item.external_id,"instance_id":item.instance_id,"relationship_id":item.relationship_id,"relationship_revision":null,"role":item.role})
}
fn endpoint_semantic(item: &EndpointPlan) -> Value {
    json!({"channel_id":item.channel_id,"guild_id":item.guild_id,"instance_id":item.instance_id,"policy_json":item.policy_json,"readable":item.readable,"writable":item.writable})
}
fn current_semantic(conn: &Connection, table: &str, key: &[String]) -> Result<Option<Value>> {
    match table{
 "instances"=>{let common=conn.query_row("SELECT agent_id,subject_id,config_b64,addresses_json,enabled,core_revision FROM instances WHERE instance_id=?1",[&key[0]],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,bool>(4)?,r.get::<_,Option<i64>>(5)?))).optional();match common{Ok(Some((agent,subject,config,addresses,enabled,core_revision)))=>Ok(Some(json!({"agent_id":agent,"addresses":serde_json::from_str::<Vec<String>>(&addresses)?,"config_b64":config,"enabled":enabled,"instance_id":key[0],"revision":core_revision.unwrap_or(1),"subject_id":subject}))),Ok(None)=>Ok(None),Err(_)=>{let web=conn.query_row("SELECT agent_id,revision,enabled,author_id FROM instances WHERE instance_id=?1",[&key[0]],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,bool>(2)?,r.get::<_,String>(3)?))).optional()?;web.map(|(agent,revision,enabled,author_id)|{
    let roles=conn.prepare("SELECT role FROM identity_projections WHERE instance_id=?1 AND external_id='web-local'")?.query_map([&key[0]],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
    ensure!(roles == ["owner"],"Web local Owner policy missing or conflicting");
    Ok(json!({"agent_id":agent,"author_id":author_id,"enabled":enabled,"instance_id":key[0],"revision":revision}))
}).transpose()}}},
 "identity_projections"=>Ok(conn.query_row("SELECT relationship_id,relationship_revision FROM identity_projections WHERE instance_id=?1 AND role=?2 AND external_id=?3",params![key[0],key[1],key[2]],|r|Ok(json!({"external_id":key[2],"instance_id":key[0],"relationship_id":r.get::<_,Option<String>>(0)?,"relationship_revision":r.get::<_,Option<i64>>(1)?,"role":key[1]}))).optional()?),
 "endpoints"=>Ok(conn.query_row("SELECT guild_id,readable,writable,policy_json FROM endpoints WHERE instance_id=?1 AND channel_id=?2",params![key[0],key[1]],|r|Ok(json!({"channel_id":key[1],"guild_id":r.get::<_,Option<String>>(0)?,"instance_id":key[0],"policy_json":r.get::<_,String>(3)?,"readable":r.get::<_,bool>(1)?,"writable":r.get::<_,bool>(2)?}))).optional()?),
 "legacy_identity_sources"=>current_legacy_identity_source(conn,key),
 _=>bail!("unsupported expected table")}
}
fn row_hash(table: &str, key: &[String], row: &Value) -> Result<String> {
    canonical::hash(&json!({"table":table,"key":key,"row":row}))
}
fn key_value(table: &str, key: Vec<String>, hash: String) -> Result<Value> {
    Ok(json!({"table":table,"key_sha256":canonical::hash(&key)?,"row_sha256":hash}))
}
fn value_key_order(a: &Value, b: &Value) -> std::cmp::Ordering {
    a.to_string().cmp(&b.to_string())
}
fn value_instance_order(a: &Value, b: &Value) -> std::cmp::Ordering {
    a["instance_sha256"].as_str().cmp(&b["instance_sha256"].as_str())
}

fn read_master_key(path: &Path, kind: &str) -> Result<Zeroizing<[u8; 32]>> {
    let value = String::from_utf8(read_secret_file(path)?.to_vec())?;
    match kind {
        "discord" => opencrab_discord_gateway::secret_store::parse_master_key(&value),
        "nostr" => opencrab_nostr_gateway::secret_store::parse_master_key(&value),
        _ => bail!("unknown key kind"),
    }
}
fn encrypt(kind: &str, clear: &[u8], key: &[u8; 32]) -> Result<String> {
    match kind {
        "discord" => opencrab_discord_gateway::secret_store::encrypt(clear, key),
        "nostr" => opencrab_nostr_gateway::secret_store::encrypt(clear, key),
        _ => bail!("unknown key kind"),
    }
}
fn decrypt(kind: &str, envelope: &str, key: &[u8; 32]) -> Result<Zeroizing<Vec<u8>>> {
    match kind {
        "discord" => opencrab_discord_gateway::secret_store::decrypt(envelope, key),
        "nostr" => opencrab_nostr_gateway::secret_store::decrypt(envelope, key),
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

fn require_destination_shape(conn: &Connection, destination: &Destination) -> Result<()> {
    let common_identity = &["instance_id", "role", "external_id", "relationship_id", "relationship_revision"];
    let tables: &[(&str, &[&str])] = match (destination.kind_id.as_str(), destination.schema.as_str()) {
        ("discord" | "nostr", "s5-discord-v1" | "s5-nostr-v1") if destination.schema == format!("s5-{}-v1", destination.kind_id) => &[
            ("instances", &["instance_id", "agent_id", "subject_id", "config_b64", "addresses_json", "credential_envelope", "enabled", "desired_generation", "applied_generation", "lifecycle_state", "core_revision", "core_digest", "binding_inventory_json", "updated_at"]),
            ("endpoints", &["instance_id", "channel_id", "guild_id", "readable", "writable", "policy_json"]),
            ("identity_projections", common_identity),
        ],
        ("web", "s5-web-v1") => &[
            ("instances", &["instance_id", "agent_id", "revision", "author_id", "credential_envelope", "enabled", "updated_at"]),
            ("identity_projections", common_identity),
            ("policies", &["instance_id", "policy_key", "policy_json"]),
        ],
        _ => bail!("destination schema identifier mismatch"),
    };
    for (table, columns) in tables {
        source::require_columns(conn, table, columns)?;
    }
    validate_legacy_identity_sources_if_present(conn)
}

pub fn prevalidate(core: &Connection, rows: &[SourceRow], approval: &Approval, inputs: &Inputs) -> Result<()> {
    ensure!(approval.destinations.len() == inputs.paths.len(), "destination input set mismatch");
    for destination in &approval.destinations {
        let path = inputs.paths.get(&(destination.kind_id.clone(), destination.path_id.clone())).context("destination path missing")?;
        ensure!(path.exists() && !path.is_symlink(), "destination must be an existing regular database path");
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        require_destination_shape(&conn, destination)?;
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
    fn verification_key_never_exposes_external_identity() {
        let entry = key_value("identity_projections", vec!["instance".into(),"owner".into(),"private-external-user".into()], "a".repeat(64)).unwrap();
        assert!(!entry.to_string().contains("private-external-user"), "the verification manifest must contain only key digests");
    }

    #[test]
    fn destination_schema_identifier_is_exact() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("discord.db");
        drop(opencrab_discord_gateway::store::DiscordStore::open(&db).unwrap());
        let destination = Destination { kind_id:"discord".into(), path_id:"main".into(), schema:"wrong".into() };
        let inputs = Inputs { paths:BTreeMap::from([(("discord".into(),"main".into()),db)]), master_keys:BTreeMap::new(), credential_files:BTreeMap::new() };
        assert!(prevalidate(&opencrab_db::init_memory().unwrap(), &[], &approval(destination), &inputs).is_err());
    }

    #[test]
    fn destination_missing_used_column_is_rejected_before_backup() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("discord.db");
        drop(opencrab_discord_gateway::store::DiscordStore::open(&db).unwrap());
        Connection::open(&db).unwrap().execute_batch("ALTER TABLE instances RENAME COLUMN credential_envelope TO missing_credential;").unwrap();
        let destination = Destination { kind_id:"discord".into(), path_id:"main".into(), schema:"s5-discord-v1".into() };
        let inputs = Inputs { paths:BTreeMap::from([(("discord".into(),"main".into()),db)]), master_keys:BTreeMap::new(), credential_files:BTreeMap::new() };
        assert!(prevalidate(&opencrab_db::init_memory().unwrap(), &[], &approval(destination), &inputs).is_err());
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
        let plan = InstancePlan { destination:destination.clone(), instance_id:"i".into(), agent_id:"agent-a".into(), subject_id:1, revision:1, config_b64, addresses:vec![], binding_ids:vec![], enabled:true, credential:Zeroizing::new(vec![1]), credential_source:"x".into(), created_at:"2026-01-01T00:00:00Z".into() };
        let mut approval = approval(destination);
        approval.identity_dispositions.push(IdentityDisposition { source_fingerprint:row.fingerprint.clone(), edges:vec![IdentityEdge::Gateway { kind_id:"discord".into(), instance_id:"i".into() }] });
        assert!(build_identity_plans(&[row], &approval, &[plan]).is_err());
    }

    #[test]
    fn progressed_instance_with_stale_core_revision_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("discord.db");
        drop(opencrab_discord_gateway::store::DiscordStore::open(&db).unwrap());
        let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO instances(instance_id,agent_id,subject_id,config_b64,addresses_json,credential_envelope,enabled,desired_generation,applied_generation,lifecycle_state,core_revision,core_digest,binding_inventory_json,failure_count,updated_at) VALUES ('i','a',1,'e30=','[]','enc:v1:x',1,2,2,'running',7,'digest','[\"b\"]',0,'t')", []).unwrap();
        let row = current_semantic(&conn, "instances", &["i".into()]).unwrap().unwrap();
        assert_eq!(row["revision"], 7, "a stale progressed core revision must not be accepted as revision 1");
    }

    #[test]
    fn existing_web_instance_uses_persisted_author_and_local_owner_without_creation() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("web.db");
        let store = opencrab_web_gateway::store::WebStore::open(&db).unwrap();
        store.upsert("web-i", "agent-a", 3, "author-42", true).unwrap();
        store.set_local_owner("web-i").unwrap();
        drop(store);
        let conn = Connection::open(&db).unwrap();
        let row = current_semantic(&conn, "instances", &["web-i".into()]).unwrap().unwrap();
        assert_eq!(row["author_id"], "author-42");
        opencrab_web_gateway::store::WebStore::open(&db).unwrap().require_local_owner("web-i").unwrap();
    }

    #[test]
    fn missing_web_instance_is_not_invented_by_migration() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("web.db");
        drop(opencrab_web_gateway::store::WebStore::open(&db).unwrap());
        let mut conn = Connection::open(&db).unwrap();
        let plan = InstancePlan {
            destination: Destination {kind_id:"web".into(),path_id:"main".into(),schema:"s5-web-v1".into()},
            instance_id:"missing".into(),agent_id:"agent-a".into(),subject_id:1,revision:1,
            config_b64:"e30=".into(),addresses:vec![],binding_ids:vec![],enabled:true,credential:Zeroizing::new(vec![1]),
            credential_source:"x".into(),created_at:"2026".into()
        };
        let tx = conn.transaction().unwrap();
        assert!(apply_instance(&tx, &plan, None).is_err());
        assert_eq!(tx.query_row("SELECT COUNT(*) FROM instances", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
    }

    #[test]
    fn complete_channel_binding_edge_set_is_validated() {
        let core = opencrab_db::init_memory().unwrap();
        core.execute("INSERT INTO agents(agent_id,name,persona_name,instructions,created_at,updated_at) VALUES ('agent-a','A','A','','2026','2026')", []).unwrap();
        let subject: i64 = core.query_row("SELECT subject_id FROM agents WHERE agent_id='agent-a'", [], |r| r.get(0)).unwrap();
        core.execute("INSERT INTO sessions(id,theme,created_at,updated_at) VALUES ('session-a','t','2026','2026')", []).unwrap();
        core.execute("INSERT INTO agent_sessions(agent_id,session_id) VALUES ('agent-a','session-a')", []).unwrap();
        let cfg = serde_json::json!({"agent_id":"agent-a","self_bot_id":"99","access":{"owners":[],"co_agents":{},"trusted_users":[]},"system_reactions":{}});
        let config_b64 = opencrab_discord_gateway::config::canonicalize_config_b64(&base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&cfg).unwrap())).unwrap();
        let digest = canonical::hex(&Sha256::digest(base64::engine::general_purpose::STANDARD.decode(&config_b64).unwrap()));
        core.execute("INSERT INTO gate_instances(instance_id,kind_id,subject_id,revision,enabled,config_b64,config_digest,created_at,updated_at,association_grandfathered) VALUES ('i','discord',?1,1,1,?2,?3,1,1,1)", params![subject,config_b64,digest]).unwrap();
        core.execute("INSERT INTO gate_bindings(binding_id,instance_id,address,created_at,session_id) VALUES ('b','i','discord-agent-a--42',1,'session-a')", []).unwrap();
        let row = SourceRow { table:"channel_config".into(), fingerprint:"1".repeat(64), columns:vec![
            ("channel_id".into(), crate::source::Cell::Text("42".into())),
            ("agent_id".into(), crate::source::Cell::Text("agent-a".into())),
            ("guild_id".into(), crate::source::Cell::Text("".into())),
        ]};
        let destination = Destination { kind_id:"discord".into(), path_id:"main".into(), schema:"s5-discord-v1".into() };
        let plan = InstancePlan { destination:destination.clone(), instance_id:"i".into(), agent_id:"agent-a".into(), subject_id:subject, revision:1, config_b64, addresses:vec!["discord-agent-a--42".into()], binding_ids:vec!["b".into()], enabled:true, credential:Zeroizing::new(vec![1]), credential_source:"x".into(), created_at:"2026".into() };
        assert!(build_endpoint_plans(&core, &[row], &approval(destination), &[plan]).is_err(), "a matching live binding cannot be omitted from the approved channel edges");
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

}
