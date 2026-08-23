mod report;
mod schema;
mod source;
mod time;
mod uuid;

pub use report::{ClassAccounting, ConversionReport};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{params, Connection, OpenFlags, Transaction};
use schema::{create_phase1_schema, require_empty_target};
use source::{SourceRow, SourceTable};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use time::parse_utc_nanos;
use uuid::parse_canonical_uuid;

const SOURCE_DB: &str = "data/opencrab.db";

pub type Result<T> = std::result::Result<T, ConverterError>;

#[derive(Debug)]
pub enum ConverterError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    SourceSchema(String),
    InstanceSet(String),
    TargetNotEmpty,
    Accounting(String),
}

impl Display for ConverterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Sql(error) => write!(formatter, "SQLite error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
            Self::SourceSchema(message) => write!(formatter, "source schema error: {message}"),
            Self::InstanceSet(message) => write!(formatter, "instance-set error: {message}"),
            Self::TargetNotEmpty => write!(formatter, "target database is not empty"),
            Self::Accounting(message) => write!(formatter, "accounting error: {message}"),
        }
    }
}

impl std::error::Error for ConverterError {}

impl From<std::io::Error> for ConverterError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for ConverterError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<serde_json::Error> for ConverterError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationInstance {
    pub instance_id: String,
    pub kind_id: String,
    uuid_bytes: [u8; 16],
}

impl MigrationInstance {
    pub fn new(instance_id: impl Into<String>, kind_id: impl Into<String>) -> Result<Self> {
        let instance_id = instance_id.into();
        let kind_id = kind_id.into();
        let uuid_bytes = parse_canonical_uuid(&instance_id).ok_or_else(|| {
            ConverterError::InstanceSet(format!(
                "instance_id {instance_id:?} is not a canonical lowercase UUID"
            ))
        })?;
        if !matches!(kind_id.as_str(), "discord" | "nostr" | "web" | "rest") {
            return Err(ConverterError::InstanceSet(format!(
                "kind_id {kind_id:?} is outside the closed platform table"
            )));
        }
        Ok(Self {
            instance_id,
            kind_id,
            uuid_bytes,
        })
    }

    pub fn read_set(path: impl AsRef<Path>) -> Result<Vec<Self>> {
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let entries = value.as_array().ok_or_else(|| {
            ConverterError::InstanceSet("instance set must be a JSON array".into())
        })?;
        let mut instances = Vec::with_capacity(entries.len());
        let mut ids = BTreeSet::new();
        for entry in entries {
            let object = entry.as_object().ok_or_else(|| {
                ConverterError::InstanceSet("each instance must be an object".into())
            })?;
            if object.len() != 2
                || !object.contains_key("instance_id")
                || !object.contains_key("kind_id")
            {
                return Err(ConverterError::InstanceSet(
                    "each instance must contain exactly instance_id and kind_id".into(),
                ));
            }
            let instance_id = object["instance_id"]
                .as_str()
                .ok_or_else(|| ConverterError::InstanceSet("instance_id must be text".into()))?;
            let kind_id = object["kind_id"]
                .as_str()
                .ok_or_else(|| ConverterError::InstanceSet("kind_id must be text".into()))?;
            let instance = Self::new(instance_id, kind_id)?;
            if !ids.insert(instance.instance_id.clone()) {
                return Err(ConverterError::InstanceSet(format!(
                    "duplicate instance_id {:?}",
                    instance.instance_id
                )));
            }
            instances.push(instance);
        }
        instances.sort_by_key(|instance| instance.uuid_bytes);
        Ok(instances)
    }
}

#[derive(Clone, Debug)]
pub struct ConvertOptions {
    pub source: PathBuf,
    pub target: PathBuf,
    /// Complete output of the earlier instance-assembly phase. Phase 1 consumes this set but does
    /// not manufacture instance rows or defaults.
    pub migration_instances: Vec<MigrationInstance>,
}

#[derive(Clone, Debug)]
pub struct ConvertOutcome {
    pub report: ConversionReport,
}

#[derive(Default)]
struct RawCollector {
    rows: BTreeMap<(String, Vec<u8>), RawRecord>,
}

struct RawRecord {
    row_values: Vec<u8>,
    reasons: BTreeSet<&'static str>,
}

impl RawCollector {
    fn add(&mut self, table: &SourceTable, row: &SourceRow, reason: &'static str) -> Result<()> {
        let key = (table.name.to_string(), row.source_key.clone());
        match self.rows.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(RawRecord {
                    row_values: row.row_values.clone(),
                    reasons: BTreeSet::from([reason]),
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get().row_values != row.row_values {
                    return Err(ConverterError::Accounting(format!(
                        "raw carrier collision for {} source key {}",
                        table.name,
                        source::hex(&row.source_key)
                    )));
                }
                entry.get_mut().reasons.insert(reason);
            }
        }
        Ok(())
    }

    fn write(&self, target: &Transaction<'_>) -> Result<()> {
        for ((table, source_key), record) in &self.rows {
            let reason = record.reasons.iter().copied().collect::<Vec<_>>().join(",");
            target.execute(
                "INSERT INTO legacy_unowned_source_rows(source_db,source_table,source_key,row_values,reason)
                 VALUES(?1,?2,?3,?4,?5)",
                params![SOURCE_DB, table, source_key, record.row_values, reason],
            )?;
        }
        Ok(())
    }
}

struct Principal {
    subject_id: i64,
    platform: String,
    external_id: String,
    public_id: String,
    display_name: String,
    created_at: i64,
    contributors: Vec<usize>,
    instances: Vec<String>,
}

#[derive(Clone, Copy)]
enum GrantSource {
    User,
    CoAgent,
}

struct GrantContributor {
    source: GrantSource,
    agent_subject_id: i64,
    principal_subject_id: i64,
    role: &'static str,
    external_id: String,
    gate_kind: Option<String>,
    permission: Option<String>,
    allowed_actions: Option<String>,
    source_record_key: Option<String>,
    created_by: String,
    created_at: i64,
    source_key: Vec<u8>,
    row_digest: [u8; 32],
}

pub fn convert(options: ConvertOptions) -> Result<ConvertOutcome> {
    validate_instances(&options.migration_instances)?;
    let source = Connection::open_with_flags(
        &options.source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    source.execute_batch("BEGIN")?;
    let agents = SourceTable::load(&source, "agents")?;
    let trusted_users = SourceTable::load(&source, "trusted_users")?;
    let trusted_co_agents = SourceTable::load(&source, "trusted_co_agents")?;
    agents.require_exact_columns(&[
        "agent_id",
        "name",
        "job_title",
        "organization",
        "image_url",
        "persona_name",
        "personality",
        "instructions",
        "heartbeat_instructions",
        "model",
        "reasoning_effort",
        "web_search",
        "metadata_json",
        "created_at",
        "updated_at",
    ])?;
    trusted_users.require_exact_columns(&[
        "id",
        "user_id",
        "agent_id",
        "permission",
        "created_by",
        "created_at",
        "display_name",
        "platform",
    ])?;
    trusted_co_agents.require_exact_columns(&[
        "id",
        "agent_id",
        "co_agent_id",
        "allowed_actions",
        "created_by",
        "created_at",
    ])?;
    source.execute_batch("ROLLBACK")?;

    let mut target = Connection::open(&options.target)?;
    require_empty_target(&target)?;
    let transaction = target.transaction()?;
    create_phase1_schema(&transaction)?;

    let mut raw = RawCollector::default();
    let mut report = ConversionReport::default();

    // The current ledger has no source for either required runtime policy. It explicitly routes
    // every complete agents row to raw; no fallback policy or partial subject is permitted.
    for row in &agents.rows {
        raw.add(
            &agents,
            row,
            "create-subject-public-id-v1:missing_history_and_output_policy",
        )?;
    }
    report.classes.push(ClassAccounting {
        source_table: agents.name.into(),
        logical_class: "agent_aggregate".into(),
        source_rows: agents.rows.len() as u64,
        canonical_outcomes: 0,
        raw_outcomes: agents.rows.len() as u64,
        exact_one_violations: 0,
        physical_rows: BTreeMap::from([
            ("subjects".into(), 0),
            ("subject_profiles".into(), 0),
            ("subject_runtime_configs".into(), 0),
        ]),
    });
    let agent_subjects = BTreeMap::<String, i64>::new();

    let principals = assemble_principals(
        &transaction,
        &trusted_users,
        &options.migration_instances,
        &mut raw,
        &mut report,
    )?;
    let principal_subjects = principals
        .iter()
        .map(|principal| {
            (
                (principal.platform.clone(), principal.external_id.clone()),
                principal.subject_id,
            )
        })
        .collect::<BTreeMap<_, _>>();

    assemble_grants(
        &transaction,
        &trusted_users,
        &trusted_co_agents,
        &agent_subjects,
        &principal_subjects,
        &mut raw,
        &mut report,
    )?;
    raw.write(&transaction)?;

    for table in [
        "subjects",
        "subject_profiles",
        "subject_runtime_configs",
        "gate_subject_identities",
        "grant_sets",
        "agent_grants",
        "grant_actions",
        "grant_source_provenance",
        "legacy_unowned_source_rows",
    ] {
        let count = transaction.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, u64>(0)
        })?;
        report.physical_rows.insert(table.into(), count);
    }
    report.verify()?;
    transaction.commit()?;
    Ok(ConvertOutcome { report })
}

fn assemble_principals(
    target: &Transaction<'_>,
    trusted_users: &SourceTable,
    instances: &[MigrationInstance],
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<Vec<Principal>> {
    let mut groups = BTreeMap::<(String, String), Vec<usize>>::new();
    let mut raw_outcomes = 0_u64;
    for (index, row) in trusted_users.rows.iter().enumerate() {
        let (Some(platform), Some(external_id)) = (
            trusted_users.text(row, "platform"),
            trusted_users.text(row, "user_id"),
        ) else {
            raw.add(
                trusted_users,
                row,
                "resolve-external-principal-v1:noncanonical_storage",
            )?;
            raw_outcomes += 1;
            continue;
        };
        if !matches!(platform, "discord" | "nostr" | "web" | "rest") {
            raw.add(
                trusted_users,
                row,
                "resolve-external-principal-v1:unknown_platform",
            )?;
            raw_outcomes += 1;
            continue;
        }
        groups
            .entry((platform.to_string(), external_id.to_string()))
            .or_default()
            .push(index);
    }

    let mut candidates = Vec::<Principal>::new();
    for ((platform, external_id), mut contributors) in groups {
        contributors.sort_by(|left, right| {
            let left = &trusted_users.rows[*left];
            let right = &trusted_users.rows[*right];
            left.source_key
                .cmp(&right.source_key)
                .then(left.row_digest.cmp(&right.row_digest))
        });
        let mut matching_instances = instances
            .iter()
            .filter(|instance| instance.kind_id == platform)
            .collect::<Vec<_>>();
        matching_instances.sort_by_key(|instance| instance.uuid_bytes);
        let matching_instances = matching_instances
            .into_iter()
            .map(|instance| instance.instance_id.clone())
            .collect::<Vec<_>>();
        let created_at = contributors
            .first()
            .and_then(|index| trusted_users.text(&trusted_users.rows[*index], "created_at"))
            .and_then(parse_utc_nanos);
        let display_name = contributors.iter().find_map(|index| {
            trusted_users
                .text(&trusted_users.rows[*index], "display_name")
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
        let reason = if matching_instances.is_empty() {
            Some("resolve-external-principal-v1:zero_matching_instances")
        } else if created_at.is_none() {
            Some("resolve-external-principal-v1:invalid_created_at")
        } else if display_name.is_none() {
            Some("resolve-external-principal-v1:missing_display_name")
        } else {
            None
        };
        if let Some(reason) = reason {
            for index in contributors {
                raw.add(trusted_users, &trusted_users.rows[index], reason)?;
                raw_outcomes += 1;
            }
            continue;
        }
        let public_id = format!(
            "external:{platform}:{}",
            URL_SAFE_NO_PAD.encode(external_id.as_bytes())
        );
        candidates.push(Principal {
            subject_id: 0,
            platform,
            external_id,
            public_id,
            display_name: display_name.expect("checked above"),
            created_at: created_at.expect("checked above"),
            contributors,
            instances: matching_instances,
        });
    }

    candidates.sort_by(|left, right| {
        left.platform
            .as_bytes()
            .cmp(right.platform.as_bytes())
            .then(
                left.external_id
                    .as_bytes()
                    .cmp(right.external_id.as_bytes()),
            )
    });
    let mut canonical_outcomes = 0_u64;
    let mut identity_rows = 0_u64;
    for (index, principal) in candidates.iter_mut().enumerate() {
        principal.subject_id = i64::try_from(index + 1)
            .map_err(|_| ConverterError::Accounting("subject id overflow".into()))?;
        target.execute(
            "INSERT INTO subjects(id,kind,public_id,display_name,created_at)
             VALUES(?1,'human',?2,?3,?4)",
            params![
                principal.subject_id,
                principal.public_id,
                principal.display_name,
                principal.created_at
            ],
        )?;
        for instance in &principal.instances {
            target.execute(
                "INSERT INTO gate_subject_identities(instance_id,external_id,subject_id,display_name)
                 VALUES(?1,?2,?3,?4)",
                params![
                    instance,
                    principal.external_id,
                    principal.subject_id,
                    principal.display_name
                ],
            )?;
            identity_rows += 1;
        }
        canonical_outcomes += principal.contributors.len() as u64;
    }
    report.classes.push(ClassAccounting {
        source_table: trusted_users.name.into(),
        logical_class: "external_principal".into(),
        source_rows: trusted_users.rows.len() as u64,
        canonical_outcomes,
        raw_outcomes,
        exact_one_violations: 0,
        physical_rows: BTreeMap::from([
            ("subjects".into(), candidates.len() as u64),
            ("gate_subject_identities".into(), identity_rows),
        ]),
    });
    Ok(candidates)
}

#[allow(clippy::too_many_arguments)]
fn assemble_grants(
    target: &Transaction<'_>,
    trusted_users: &SourceTable,
    trusted_co_agents: &SourceTable,
    agent_subjects: &BTreeMap<String, i64>,
    principal_subjects: &BTreeMap<(String, String), i64>,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<()> {
    let mut contributors = Vec::<GrantContributor>::new();
    let mut raw_outcomes = 0_u64;
    for row in &trusted_users.rows {
        match parse_user_grant(trusted_users, row, agent_subjects, principal_subjects) {
            Ok(contributor) => contributors.push(contributor),
            Err(reason) => {
                raw.add(trusted_users, row, reason)?;
                raw_outcomes += 1;
            }
        }
    }
    for row in &trusted_co_agents.rows {
        match parse_co_agent_grant(trusted_co_agents, row, agent_subjects) {
            Ok(contributor) => contributors.push(contributor),
            Err(reason) => {
                raw.add(trusted_co_agents, row, reason)?;
                raw_outcomes += 1;
            }
        }
    }

    contributors.sort_by(|left, right| {
        grant_source_rank(left.source)
            .cmp(&grant_source_rank(right.source))
            .then(left.source_key.cmp(&right.source_key))
            .then(left.row_digest.cmp(&right.row_digest))
    });
    let mut groups = BTreeMap::<i64, Vec<GrantContributor>>::new();
    for contributor in contributors {
        groups
            .entry(contributor.agent_subject_id)
            .or_default()
            .push(contributor);
    }

    let mut grant_rows = 0_u64;
    let mut action_rows = 0_u64;
    let mut provenance_rows = 0_u64;
    let mut canonical_outcomes = 0_u64;
    for (agent, group) in groups {
        let created_at = group[0].created_at;
        target.execute(
            "INSERT INTO grant_sets(agent_subject_id,revision,created_at) VALUES(?1,1,?2)",
            params![agent, created_at],
        )?;
        let mut selected_roles = BTreeMap::<i64, &'static str>::new();
        let mut actions = BTreeSet::<(i64, String)>::new();
        for contributor in &group {
            selected_roles
                .entry(contributor.principal_subject_id)
                .and_modify(|role| {
                    if role_rank(contributor.role) < role_rank(role) {
                        *role = contributor.role;
                    }
                })
                .or_insert(contributor.role);
            if let Some(action) = &contributor.allowed_actions {
                actions.insert((contributor.principal_subject_id, action.clone()));
            }
            target.execute(
                "INSERT INTO grant_source_provenance(
                   agent_subject_id,principal_subject_id,gate_kind,external_id,source_permission,
                   source_allowed_actions,source_record_key,created_by,created_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    contributor.agent_subject_id,
                    contributor.principal_subject_id,
                    contributor.gate_kind,
                    contributor.external_id,
                    contributor.permission,
                    contributor.allowed_actions,
                    contributor.source_record_key,
                    contributor.created_by,
                    contributor.created_at,
                ],
            )?;
            provenance_rows += 1;
        }
        for (principal, role) in selected_roles {
            target.execute(
                "INSERT INTO agent_grants(
                   grant_set_revision,grant_set_subject_id,principal_subject_id,role,scope
                 ) VALUES(1,?1,?2,?3,'agent')",
                params![agent, principal, role],
            )?;
            grant_rows += 1;
        }
        for (principal, action) in actions {
            target.execute(
                "INSERT INTO grant_actions(
                   grant_set_revision,grant_set_subject_id,principal_subject_id,action
                 ) VALUES(1,?1,?2,?3)",
                params![agent, principal, action],
            )?;
            action_rows += 1;
        }
        canonical_outcomes += group.len() as u64;
    }

    report.classes.push(ClassAccounting {
        source_table: "trusted_users+trusted_co_agents".into(),
        logical_class: "grant_contributor".into(),
        source_rows: (trusted_users.rows.len() + trusted_co_agents.rows.len()) as u64,
        canonical_outcomes,
        raw_outcomes,
        exact_one_violations: 0,
        physical_rows: BTreeMap::from([
            ("grant_sets".into(), report_group_count(target)?),
            ("agent_grants".into(), grant_rows),
            ("grant_actions".into(), action_rows),
            ("grant_source_provenance".into(), provenance_rows),
        ]),
    });
    Ok(())
}

fn parse_user_grant(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
    principals: &BTreeMap<(String, String), i64>,
) -> std::result::Result<GrantContributor, &'static str> {
    let agent_id = table
        .text(row, "agent_id")
        .ok_or("grant-permission-v1:noncanonical_storage")?;
    let agent_subject_id = *agents
        .get(agent_id)
        .ok_or("grant-permission-v1:unresolved_agent")?;
    let platform = table
        .text(row, "platform")
        .ok_or("grant-permission-v1:noncanonical_storage")?;
    if !matches!(platform, "discord" | "nostr" | "web" | "rest") {
        return Err("grant-permission-v1:unknown_platform");
    }
    let external_id = table
        .text(row, "user_id")
        .ok_or("grant-permission-v1:noncanonical_storage")?;
    let principal_subject_id = *principals
        .get(&(platform.to_string(), external_id.to_string()))
        .ok_or("grant-permission-v1:unresolved_principal")?;
    let permission = table
        .text(row, "permission")
        .ok_or("grant-permission-v1:noncanonical_storage")?;
    let role = match permission {
        "owner" => "owner",
        "user" => "trusted",
        "co-agent" => "owner_equivalent",
        _ => return Err("grant-permission-v1:unknown_permission"),
    };
    let created_at = table
        .text(row, "created_at")
        .and_then(parse_utc_nanos)
        .ok_or("grant-permission-v1:invalid_created_at")?;
    let created_by = table
        .text(row, "created_by")
        .ok_or("grant-permission-v1:noncanonical_storage")?;
    let source_record_key = table
        .nullable_text(row, "id")
        .ok_or("grant-permission-v1:noncanonical_storage")?
        .map(str::to_string);
    Ok(GrantContributor {
        source: GrantSource::User,
        agent_subject_id,
        principal_subject_id,
        role,
        external_id: external_id.into(),
        gate_kind: Some(platform.into()),
        permission: Some(permission.into()),
        allowed_actions: None,
        source_record_key,
        created_by: created_by.into(),
        created_at,
        source_key: row.source_key.clone(),
        row_digest: row.row_digest,
    })
}

fn parse_co_agent_grant(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
) -> std::result::Result<GrantContributor, &'static str> {
    let agent_id = table
        .text(row, "agent_id")
        .ok_or("grant-permission-v1:noncanonical_storage")?;
    let agent_subject_id = *agents
        .get(agent_id)
        .ok_or("grant-permission-v1:unresolved_agent")?;
    let co_agent_id = table
        .text(row, "co_agent_id")
        .ok_or("grant-permission-v1:noncanonical_storage")?;
    let principal_subject_id = *agents
        .get(co_agent_id)
        .ok_or("grant-permission-v1:unresolved_principal")?;
    let created_at = table
        .text(row, "created_at")
        .and_then(parse_utc_nanos)
        .ok_or("grant-permission-v1:invalid_created_at")?;
    let created_by = table
        .text(row, "created_by")
        .ok_or("grant-permission-v1:noncanonical_storage")?;
    let source_record_key = table
        .nullable_text(row, "id")
        .ok_or("grant-permission-v1:noncanonical_storage")?
        .map(str::to_string);
    let allowed_actions = table
        .nullable_text(row, "allowed_actions")
        .ok_or("grant-permission-v1:noncanonical_storage")?
        .map(str::to_string);
    Ok(GrantContributor {
        source: GrantSource::CoAgent,
        agent_subject_id,
        principal_subject_id,
        role: "owner_equivalent",
        external_id: co_agent_id.into(),
        gate_kind: None,
        permission: None,
        allowed_actions,
        source_record_key,
        created_by: created_by.into(),
        created_at,
        source_key: row.source_key.clone(),
        row_digest: row.row_digest,
    })
}

fn grant_source_rank(source: GrantSource) -> u8 {
    match source {
        GrantSource::User => 0,
        GrantSource::CoAgent => 1,
    }
}

fn role_rank(role: &str) -> u8 {
    match role {
        "owner" => 0,
        "owner_equivalent" => 1,
        "trusted" => 2,
        _ => 3,
    }
}

fn report_group_count(target: &Transaction<'_>) -> Result<u64> {
    Ok(target.query_row("SELECT COUNT(*) FROM grant_sets", [], |row| row.get(0))?)
}

fn validate_instances(instances: &[MigrationInstance]) -> Result<()> {
    let mut seen = BTreeMap::<&str, &str>::new();
    for instance in instances {
        if let Some(existing_kind) = seen.insert(&instance.instance_id, &instance.kind_id) {
            return Err(ConverterError::InstanceSet(format!(
                "duplicate instance_id {:?} for kinds {existing_kind:?} and {:?}",
                instance.instance_id, instance.kind_id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
