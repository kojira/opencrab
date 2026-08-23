mod phase3;
mod provenance;
mod report;
mod schema;
mod source;
mod time;
mod uuid;

pub use report::{ClassAccounting, ConversionReport};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use phase3::assemble_phase3;
use provenance::{composite_key, digest_file, integer_key, text, MigrationProvenance};
use report::ContributionKey;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use schema::create_migration_owned_schema;
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use sha2::{Digest, Sha256};
use source::{SourceRow, SourceTable};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::path::Path;
use time::parse_utc_nanos;
use uuid::parse_canonical_uuid;

const SOURCE_DB: &str = "data/opencrab.db";

/// Verbatim firing Policy JSON written by oc2 `create_place` / `provision_place`
/// when given `Policy::default()` (DESIGN-DB-MIGRATION §12.8.1).
///
/// Config-derived Discord admission is the ledger transform into
/// `place_default_policies` / `place_subject_policies`, not this column.
/// History/closed places use the same JSON: `Policy::default()` is the unique
/// non-firing value in code (empty `immediate`, no unconditional interval, no
/// `default_subject`); archival is `closed_at` / `close_reason`.
fn default_place_policy_json() -> String {
    serde_json::json!({
        "immediate": [],
        "immediate_from": "anyone",
        "batch_window_ms": serde_json::Value::Null,
        "unconditional_interval_ms": serde_json::Value::Null,
        "default_subject": serde_json::Value::Null,
    })
    .to_string()
}

pub type Result<T> = std::result::Result<T, ConverterError>;

#[derive(Debug)]
pub enum ConverterError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    SourceSnapshot(String),
    SourceSchema(String),
    InstanceSet(String),
    AlreadyApplied,
    Accounting(String),
}

impl Display for ConverterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Sql(error) => write!(formatter, "SQLite error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
            Self::SourceSnapshot(message) => write!(formatter, "source snapshot error: {message}"),
            Self::SourceSchema(message) => write!(formatter, "source schema error: {message}"),
            Self::InstanceSet(message) => write!(formatter, "instance-set error: {message}"),
            Self::AlreadyApplied => write!(
                formatter,
                "schema_migration_state already has inplace-v1; refuse double INSERT"
            ),
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
struct MigrationInstance {
    instance_id: String,
    kind_id: String,
    uuid_bytes: [u8; 16],
}

impl MigrationInstance {
    fn new(instance_id: impl Into<String>, kind_id: impl Into<String>) -> Result<Self> {
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
}

#[derive(Clone, Debug, Default)]
struct MigrationInstanceSet(Vec<MigrationInstance>);

/// Phase boundary used by the production conversion coordinator.
///
/// Implementations read from the same open source snapshot and assemble migration-created
/// instances directly in the fresh target transaction. They do not return a caller-supplied
/// authority set: after this phase completes, `migrate_in_place` reads the authoritative set back from
/// `gate_instances` before assembling principals.
pub trait MigrationInstanceAssembler {
    fn assemble(&self, source: &Connection, target: &MigrationInstanceTarget<'_, '_>)
        -> Result<()>;
}

/// Restricted target writer passed to the migration instance assembly phase.
pub struct MigrationInstanceTarget<'target, 'connection> {
    transaction: &'target Transaction<'connection>,
}

impl MigrationInstanceTarget<'_, '_> {
    /// Writes one complete migration-created instance row.
    ///
    /// Phase 1 fixes the initial revision and lifecycle values; the assembly owns the instance ID,
    /// kind, label, and optional owner produced from its migration sources.
    pub fn create_instance(
        &self,
        instance_id: &str,
        kind_id: &str,
        label: &str,
        owner_subject_id: Option<i64>,
    ) -> Result<()> {
        MigrationInstance::new(instance_id, kind_id)?;
        if label.is_empty() {
            return Err(ConverterError::InstanceSet(
                "migration-created instance label must not be empty".into(),
            ));
        }
        self.transaction.execute(
            "INSERT INTO gate_instances(
               instance_id,kind_id,label,owner_subject_id,active_revision,lifecycle
             ) VALUES(?1,?2,?3,?4,1,'stopped')",
            params![instance_id, kind_id, label, owner_subject_id],
        )?;
        Ok(())
    }
}

/// Instance phase for a conversion slice which has no instance-producing source family.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoMigrationInstances;

impl MigrationInstanceAssembler for NoMigrationInstances {
    fn assemble(
        &self,
        _source: &Connection,
        _target: &MigrationInstanceTarget<'_, '_>,
    ) -> Result<()> {
        Ok(())
    }
}

pub(crate) struct RawCollector<'target, 'connection> {
    target: &'target Transaction<'connection>,
}

impl<'target, 'connection> RawCollector<'target, 'connection> {
    fn new(target: &'target Transaction<'connection>) -> Self {
        Self { target }
    }

    fn add(&mut self, table: &SourceTable, row: &SourceRow, reason: &'static str) -> Result<()> {
        let existing = self
            .target
            .query_row(
                "SELECT row_values,reason FROM legacy_unowned_source_rows
                 WHERE source_db=?1 AND source_table=?2 AND source_key=?3",
                params![SOURCE_DB, table.name, row.source_key],
                |result| Ok((result.get::<_, Vec<u8>>(0)?, result.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((values, existing_reasons)) = existing {
            if values != row.row_values {
                return Err(ConverterError::Accounting(format!(
                    "raw carrier collision for {} source key {}",
                    table.name,
                    source::hex(&row.source_key)
                )));
            }
            let mut reasons = existing_reasons.split(',').collect::<BTreeSet<_>>();
            reasons.insert(reason);
            self.target.execute(
                "UPDATE legacy_unowned_source_rows SET reason=?1
                 WHERE source_db=?2 AND source_table=?3 AND source_key=?4",
                params![
                    reasons.into_iter().collect::<Vec<_>>().join(","),
                    SOURCE_DB,
                    table.name,
                    row.source_key,
                ],
            )?;
        } else {
            self.target.execute(
                "INSERT INTO legacy_unowned_source_rows(source_db,source_table,source_key,row_values,reason)
                 VALUES(?1,?2,?3,?4,?5)",
                params![
                    SOURCE_DB,
                    table.name,
                    row.source_key,
                    row.row_values,
                    reason,
                ],
            )?;
        }
        Ok(())
    }

    fn write(&self, _target: &Transaction<'_>) -> Result<()> {
        Ok(())
    }
}

struct Principal {
    subject_id: i64,
    platform: String,
    external_id: String,
    #[allow(dead_code)]
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
    gate_kind: String,
    permission: String,
    allowed_actions: Option<String>,
    source_record_key: Option<String>,
    created_by: String,
    created_at: i64,
    source_key: Vec<u8>,
    row_digest: [u8; 32],
    contribution: ContributionKey,
}

#[derive(Clone, Debug, Default)]
struct EffectiveConfig {
    default_model: Option<String>,
    compaction_ratio: f64,
    source_config: Option<Vec<u8>>,
    discord: Option<SharedDiscordConfig>,
}

#[derive(Clone, Debug)]
struct SharedDiscordConfig {
    enabled: bool,
    token: Vec<u8>,
    agent_ids: Vec<String>,
    guild_ids: Vec<String>,
    owner_external_id: String,
}

struct EffectiveConfigSnapshot {
    config: EffectiveConfig,
    digest: [u8; 32],
}

fn load_effective_config(
    database_digest: [u8; 32],
    captured_at: i64,
    config_path: &Path,
    environment_path: &Path,
) -> Result<EffectiveConfigSnapshot> {
    let raw = std::fs::read(config_path)?;
    let environment_raw = std::fs::read(environment_path)?;
    if environment_assignment_values_contain_dollar(&environment_raw) {
        return Err(ConverterError::SourceSchema(
            "environment snapshot must contain resolved values, not variable references".into(),
        ));
    }
    let mut environment = BTreeMap::<String, String>::new();
    for parsed in dotenvy::from_read_iter(environment_raw.as_slice()) {
        let (name, value) = parsed.map_err(|error| {
            ConverterError::SourceSchema(format!("environment snapshot is invalid: {error}"))
        })?;
        // Production dotenv loading preserves the first definition and does not overwrite an
        // already captured value. The converter receives the complete effective environment as
        // one immutable resource, so it never consults its own process environment.
        environment.entry(name).or_insert(value);
    }
    let text = std::str::from_utf8(&raw)
        .map_err(|_| ConverterError::SourceSchema("config snapshot is not valid UTF-8".into()))?;
    let expanded = expand_environment(text, &environment);
    let parsed = parse_config_projection(&expanded)?;
    let default_model = parsed.default_model;
    let compaction_ratio = parsed.compaction_ratio.unwrap_or(0.5);
    if !compaction_ratio.is_finite() || compaction_ratio < 0.0 {
        return Err(ConverterError::SourceSchema(
            "llm.compaction_ratio must be finite and non-negative".into(),
        ));
    }
    let source_config = Some(serde_json::to_vec(&serde_json::json!({
        "llm": {
            "compaction_ratio": compaction_ratio,
            "default_model": default_model,
        }
    }))?);
    let discord = parsed.discord;
    let config = EffectiveConfig {
        default_model,
        compaction_ratio,
        source_config,
        discord,
    };
    let mut digest = Sha256::new();
    digest.update(b"opencrab-converter-input-snapshot-v2\0");
    digest.update(database_digest);
    digest.update(b"captured-at-utc-nanos\0");
    digest.update(captured_at.to_be_bytes());
    digest.update((raw.len() as u64).to_be_bytes());
    digest.update(&raw);
    digest.update((environment_raw.len() as u64).to_be_bytes());
    digest.update(&environment_raw);
    for (name, value) in &environment {
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    Ok(EffectiveConfigSnapshot {
        config,
        digest: digest.finalize().into(),
    })
}

fn environment_assignment_values_contain_dollar(environment_raw: &[u8]) -> bool {
    for mut line in environment_raw.split(|&b| b == b'\n') {
        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }
        let start = line
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(line.len());
        let trimmed = &line[start..];
        if trimmed.is_empty() || trimmed[0] == b'#' {
            continue;
        }
        match trimmed.iter().position(|&b| b == b'=') {
            Some(eq) => {
                if trimmed[eq + 1..].contains(&b'$') {
                    return true;
                }
            }
            None => {
                if trimmed.contains(&b'$') {
                    return true;
                }
            }
        }
    }
    false
}

fn expand_environment(input: &str, environment: &BTreeMap<String, String>) -> String {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let Some(start) = rest.find("${") else {
            output.push_str(rest);
            break;
        };
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let line_end = after.find('\n').unwrap_or(after.len());
        if let Some(close) = after[..line_end].find('}') {
            let name = &after[..close];
            output.push_str(environment.get(name).map(String::as_str).unwrap_or(""));
            rest = &after[close + 1..];
        } else {
            output.push_str("${");
            rest = after;
        }
    }
    output
}

#[derive(Default)]
struct ConfigProjection {
    default_model: Option<String>,
    compaction_ratio: Option<f64>,
    discord: Option<SharedDiscordConfig>,
}

fn parse_config_projection(input: &str) -> Result<ConfigProjection> {
    let mut section = String::new();
    let mut default_model = None;
    let mut compaction_ratio = None;
    let mut discord_seen = false;
    let mut discord_enabled = false;
    let mut discord_token = None;
    let mut discord_owner = String::new();
    let mut discord_agents = Vec::new();
    let mut discord_guilds = Vec::new();
    for raw_line in input.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_owned();
            if section == "gateway.discord" {
                discord_seen = true;
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match (section.as_str(), key) {
            ("llm", "default_model") => default_model = Some(parse_toml_string(value)?),
            ("llm", "compaction_ratio") => {
                compaction_ratio = Some(value.parse::<f64>().map_err(|_| {
                    ConverterError::SourceSchema("llm.compaction_ratio is not a number".into())
                })?)
            }
            ("gateway.discord", "enabled") => {
                discord_enabled = value.parse::<bool>().map_err(|_| {
                    ConverterError::SourceSchema("gateway.discord.enabled is not boolean".into())
                })?
            }
            ("gateway.discord", "token") => {
                discord_token = Some(parse_toml_string(value)?.into_bytes())
            }
            ("gateway.discord", "owner_discord_id") => discord_owner = parse_toml_string(value)?,
            ("gateway.discord", "agent_ids") => {
                for id in parse_string_array(value, "gateway.discord.agent_ids")? {
                    if !id.is_empty() && !discord_agents.contains(&id) {
                        discord_agents.push(id);
                    }
                }
            }
            ("gateway.discord", "guild_ids") => {
                let values: serde_json::Value = serde_json::from_str(value).map_err(|_| {
                    ConverterError::SourceSchema(
                        "gateway.discord.guild_ids is not a TOML-compatible array".into(),
                    )
                })?;
                discord_guilds = values
                    .as_array()
                    .ok_or_else(|| {
                        ConverterError::SourceSchema(
                            "gateway.discord.guild_ids is not an array".into(),
                        )
                    })?
                    .iter()
                    .map(|value| match value {
                        serde_json::Value::String(value) => Ok(value.clone()),
                        serde_json::Value::Number(value) if value.as_u64().is_some() => {
                            Ok(value.to_string())
                        }
                        _ => Err(ConverterError::SourceSchema(
                            "gateway.discord.guild_ids contains an invalid value".into(),
                        )),
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            _ => {}
        }
    }
    let discord = if discord_seen {
        Some(SharedDiscordConfig {
            enabled: discord_enabled,
            token: discord_token.ok_or_else(|| {
                ConverterError::SourceSchema("gateway.discord.token is missing".into())
            })?,
            agent_ids: discord_agents,
            guild_ids: discord_guilds,
            owner_external_id: discord_owner,
        })
    } else {
        None
    };
    Ok(ConfigProjection {
        default_model,
        compaction_ratio,
        discord,
    })
}

fn parse_toml_string(value: &str) -> Result<String> {
    serde_json::from_str(value).map_err(|_| {
        ConverterError::SourceSchema(format!("unsupported TOML string value: {value}"))
    })
}

fn parse_string_array(value: &str, name: &str) -> Result<Vec<String>> {
    let parsed: serde_json::Value = serde_json::from_str(value).map_err(|_| {
        ConverterError::SourceSchema(format!("{name} is not a TOML-compatible string array"))
    })?;
    parsed
        .as_array()
        .ok_or_else(|| ConverterError::SourceSchema(format!("{name} is not an array")))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ConverterError::SourceSchema(format!("{name} contains non-string")))
        })
        .collect()
}

fn strip_toml_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if quoted && character == '\\' && !escaped {
            escaped = true;
            continue;
        }
        if character == '"' && !escaped {
            quoted = !quoted;
        } else if character == '#' && !quoted {
            return &line[..index];
        }
        escaped = false;
    }
    line
}

fn deterministic_uuid(snapshot_digest: [u8; 32], locator: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(snapshot_digest);
    hasher.update((locator.len() as u64).to_be_bytes());
    hasher.update(locator);
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

#[allow(clippy::too_many_arguments)]
fn assemble_gate_configs(
    source: &Connection,
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    captured_at: i64,
    config: &EffectiveConfig,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<()> {
    if let Some(discord) = &config.discord {
        for agent in &discord.agent_ids {
            if !agents.contains_key(agent) {
                return Err(ConverterError::InstanceSet(format!(
                    "shared Discord config member {agent:?} is absent from assembled agent subjects"
                )));
            }
        }
        let instance_id = deterministic_uuid(
            snapshot_digest,
            b"external:config/default.toml#/gateway/discord",
        );
        let config_bytes = serde_json::to_vec(&serde_json::json!({
            "agent_ids": discord.agent_ids,
            "guild_ids": discord.guild_ids,
            "owner_external_id": discord.owner_external_id,
            "self_external_id": serde_json::Value::Null,
        }))?;
        write_gate_assembly(
            target,
            &instance_id,
            "discord",
            "shared:discord",
            None,
            discord.enabled,
            captured_at,
            "gate-config/discord/v1",
            &config_bytes,
            "discord_bot_token",
            &discord.token,
        )?;
    }
    if SourceTable::exists(source, "agent_discord_config")? {
        let table = SourceTable::load_schema(source, "agent_discord_config")?;
        table.require_exact_columns(&[
            "agent_id",
            "bot_token",
            "owner_discord_id",
            "enabled",
            "updated_at",
            "bot_user_id",
        ])?;
        assemble_dedicated_gate_table(
            source,
            target,
            &table,
            "discord",
            snapshot_digest,
            captured_at,
            agents,
            provenance,
            raw,
            report,
        )?;
    }
    if SourceTable::exists(source, "agent_nostr_config")? {
        let table = SourceTable::load_schema(source, "agent_nostr_config")?;
        table.require_exact_columns(&[
            "agent_id",
            "secret_key",
            "relays_json",
            "filter_json",
            "enabled",
            "updated_at",
            "owner_pubkey",
            "self_pubkey",
        ])?;
        assemble_dedicated_gate_table(
            source,
            target,
            &table,
            "nostr",
            snapshot_digest,
            captured_at,
            agents,
            provenance,
            raw,
            report,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn assemble_dedicated_gate_table(
    source: &Connection,
    target: &Transaction<'_>,
    table: &SourceTable,
    kind: &'static str,
    snapshot_digest: [u8; 32],
    captured_at: i64,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<()> {
    let mut accounting =
        ClassAccounting::streaming(table.name, "dedicated_gate_config", BTreeMap::new());
    let mut canonical = 0_u64;
    let mut provenance_rows = 0_u64;
    table.for_each_row(source, "agent_id COLLATE BINARY,rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(table, row);
        let parsed = parse_dedicated_gate(table, row, kind, agents);
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(reason) => {
                raw.add(table, row, reason)?;
                row_accounting.raw();
                accounting.finish_streamed_row(row_accounting)?;
                return Ok(());
            }
        };
        let locator = [table.name.as_bytes(), b"\0", &row.source_key].concat();
        let instance_id = deterministic_uuid(snapshot_digest, &locator);
        let label = format!(
            "dedicated:{kind}:{}",
            URL_SAFE_NO_PAD.encode(&row.source_key)
        );
        write_gate_assembly(
            target,
            &instance_id,
            kind,
            &label,
            Some(parsed.owner_subject_id),
            parsed.enabled,
            captured_at,
            parsed.config_schema_id,
            &parsed.config_bytes,
            parsed.secret_name,
            &parsed.secret,
        )?;
        let instance_key = text(&instance_id);
        for entity in [
            "gate_instances",
            "gate_instance_revisions",
            "secret_sets",
            "secret_values",
        ] {
            provenance.write(
                target,
                entity,
                &composite_key(std::slice::from_ref(&instance_key)),
                table,
                row,
            )?;
            provenance_rows += 1;
        }
        canonical += 1;
        row_accounting.canonical();
        accounting.finish_streamed_row(row_accounting)?;
        Ok(())
    })?;
    accounting.physical_rows = BTreeMap::from([
        ("gate_instances".into(), canonical),
        ("gate_instance_revisions".into(), canonical),
        ("secret_sets".into(), canonical),
        ("secret_values".into(), canonical),
        ("migration_provenance".into(), provenance_rows),
    ]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedDedicatedGate {
    owner_subject_id: i64,
    enabled: bool,
    config_schema_id: &'static str,
    config_bytes: Vec<u8>,
    secret_name: &'static str,
    secret: Vec<u8>,
}

fn parse_dedicated_gate(
    table: &SourceTable,
    row: &SourceRow,
    kind: &str,
    agents: &BTreeMap<String, i64>,
) -> std::result::Result<ParsedDedicatedGate, &'static str> {
    let agent = table
        .text(row, "agent_id")
        .ok_or("gate-instance-and-subject-v1:unknown_owner")?;
    let owner_subject_id = *agents
        .get(agent)
        .ok_or("gate-instance-and-subject-v1:unknown_owner")?;
    let enabled = table
        .integer(row, "enabled")
        .map(|value| value != 0)
        .ok_or("gate-instance-and-subject-v1:noncanonical_storage")?;
    let legacy_updated_at = URL_SAFE_NO_PAD.encode(
        table
            .encoded_value(row, "updated_at")
            .ok_or("gate-instance-and-subject-v1:noncanonical_storage")?,
    );
    match kind {
        "discord" => {
            let secret = table
                .bytes(row, "bot_token")
                .ok_or("gate-instance-and-subject-v1:noncanonical_storage")?
                .to_vec();
            let owner_external_id = table
                .text(row, "owner_discord_id")
                .ok_or("gate-instance-and-subject-v1:noncanonical_storage")?;
            let self_external_id = table
                .text(row, "bot_user_id")
                .ok_or("gate-instance-and-subject-v1:noncanonical_storage")?;
            Ok(ParsedDedicatedGate {
                owner_subject_id,
                enabled,
                config_schema_id: "gate-config/discord/v1",
                config_bytes: serde_json::to_vec(&serde_json::json!({
                    "agent_ids": [],
                    "legacy_updated_at": legacy_updated_at,
                    "owner_external_id": owner_external_id,
                    "self_external_id": self_external_id,
                }))
                .map_err(|_| "gate-instance-and-subject-v1:config_encoding")?,
                secret_name: "discord_bot_token",
                secret,
            })
        }
        "nostr" => {
            let secret = table
                .bytes(row, "secret_key")
                .ok_or("gate-instance-and-subject-v1:noncanonical_storage")?
                .to_vec();
            let relays = table
                .text(row, "relays_json")
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                .ok_or("gate-instance-and-subject-v1:invalid_relays_json")?;
            let filter = table
                .text(row, "filter_json")
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                .ok_or("gate-instance-and-subject-v1:invalid_filter_json")?;
            let owner_pubkey = table
                .text(row, "owner_pubkey")
                .ok_or("gate-instance-and-subject-v1:noncanonical_storage")?;
            let self_pubkey = table
                .text(row, "self_pubkey")
                .ok_or("gate-instance-and-subject-v1:noncanonical_storage")?;
            Ok(ParsedDedicatedGate {
                owner_subject_id,
                enabled,
                config_schema_id: "gate-config/nostr/v1",
                config_bytes: serde_json::to_vec(&serde_json::json!({
                    "filter": filter,
                    "legacy_updated_at": legacy_updated_at,
                    "owner_pubkey": owner_pubkey,
                    "relays": relays,
                    "self_pubkey": self_pubkey,
                }))
                .map_err(|_| "gate-instance-and-subject-v1:config_encoding")?,
                secret_name: "nostr_secret_key",
                secret,
            })
        }
        _ => Err("gate-instance-and-subject-v1:unknown_kind"),
    }
}

#[allow(clippy::too_many_arguments)]
fn write_gate_assembly(
    target: &Transaction<'_>,
    instance_id: &str,
    kind: &str,
    label: &str,
    owner_subject_id: Option<i64>,
    enabled: bool,
    created_at: i64,
    config_schema_id: &str,
    config_bytes: &[u8],
    secret_name: &str,
    secret: &[u8],
) -> Result<()> {
    MigrationInstance::new(instance_id, kind)?;
    let secret_set_id = deterministic_uuid(
        Sha256::digest(instance_id.as_bytes()).into(),
        b"secret-set\0revision-1",
    );
    let config_digest = Sha256::digest(config_bytes);
    let secret_digest = Sha256::digest(secret);
    let at_rest_format = if secret.starts_with(b"enc:v1:") {
        "enc:v1"
    } else {
        "source-plaintext"
    };
    target.execute(
        "INSERT INTO gate_instances(instance_id,kind_id,label,owner_subject_id,active_revision,lifecycle)
         VALUES(?1,?2,?3,?4,1,'stopped')",
        params![instance_id, kind, label, owner_subject_id],
    )?;
    target.execute(
        "INSERT INTO gate_instance_revisions(
           instance_id,revision,present,enabled,created_at,config_schema_id,config_bytes,
           config_digest,secret_set_id
         ) VALUES(?1,1,1,?2,?3,?4,?5,?6,?7)",
        params![
            instance_id,
            enabled,
            created_at,
            config_schema_id,
            config_bytes,
            config_digest.as_slice(),
            secret_set_id,
        ],
    )?;
    target.execute(
        "INSERT INTO secret_sets(secret_set_id,revision,scope,created_at)
         VALUES(?1,1,?2,?3)",
        params![
            secret_set_id,
            format!("gate-instance:{instance_id}"),
            created_at
        ],
    )?;
    target.execute(
        "INSERT INTO secret_values(secret_set_id,name,at_rest_format,value,value_digest)
         VALUES(?1,?2,?3,?4,?5)",
        params![
            secret_set_id,
            secret_name,
            at_rest_format,
            secret,
            secret_digest.as_slice(),
        ],
    )?;
    Ok(())
}

#[derive(Clone)]
struct ChannelPolicy {
    row: SourceRow,
    place_id: i64,
    subject_id: Option<i64>,
    readable: bool,
    writable: bool,
    whitelisted: bool,
    heartbeat_enabled: bool,
    heartbeat_interval_secs: Option<i64>,
    heartbeat_instructions: String,
    source_updated_at: i64,
    signature: Vec<u8>,
}

#[allow(clippy::too_many_arguments)]
fn assemble_discord_channel_policies(
    source: &Connection,
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    captured_at: i64,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "discord_channel_config")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "discord_channel_config")?;
    table.require_exact_columns(&[
        "channel_id",
        "agent_id",
        "guild_id",
        "channel_name",
        "readable",
        "writable",
        "whitelisted",
        "heartbeat_enabled",
        "heartbeat_interval_secs",
        "updated_at",
        "heartbeat_instructions",
    ])?;
    let mut accounting =
        ClassAccounting::streaming(table.name, "discord_channel_policy", BTreeMap::new());
    let mut next_place_id = next_integer_id(target, "places")?;
    let mut place_rows = 0_u64;
    let mut default_rows = 0_u64;
    let mut subject_rows = 0_u64;
    let mut provenance_rows = 0_u64;
    let mut current_channel = None::<String>;
    let mut current_place = None::<i64>;
    let mut candidates = BTreeMap::<String, Vec<ChannelPolicy>>::new();
    table.for_each_row(
        source,
        "channel_id COLLATE BINARY,agent_id COLLATE BINARY,rowid",
        |row| {
            let channel = match table.text(row, "channel_id") {
                Some(value) if !value.is_empty() => value,
                _ => {
                    let mut row_accounting = accounting.start_streamed_row(&table, row);
                    raw.add(
                        &table,
                        row,
                        "discord-channel-policy-router-v1:noncanonical_storage",
                    )?;
                    row_accounting.raw();
                    accounting.finish_streamed_row(row_accounting)?;
                    return Ok(());
                }
            };
            let created_at = table
                .text(row, "updated_at")
                .and_then(parse_utc_nanos)
                .unwrap_or(captured_at);
            if current_channel.as_deref() != Some(channel) {
                flush_channel_policy_candidates(
                    target,
                    snapshot_digest,
                    &table,
                    provenance,
                    raw,
                    &mut accounting,
                    &mut candidates,
                    &mut default_rows,
                    &mut subject_rows,
                    &mut provenance_rows,
                )?;
                let matches = target.query_row(
                    "SELECT COUNT(*),MIN(place_id) FROM place_source_refs
                 WHERE source_system='discord' AND source_address=?1",
                    [channel],
                    |result| Ok((result.get::<_, i64>(0)?, result.get::<_, Option<i64>>(1)?)),
                )?;
                let place_id = match matches {
                    (0, _) => {
                        let place_id = next_place_id;
                        next_place_id = next_place_id.checked_add(1).ok_or_else(|| {
                            ConverterError::Accounting("place id overflow".into())
                        })?;
                        target.execute(
                            "INSERT INTO places(
                           id,address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,
                           created_at,closed_at,close_reason
                         ) VALUES(?1,?2,NULL,?3,NULL,NULL,?4,NULL,NULL)",
                            params![
                                place_id,
                                format!("config:discord:{channel}"),
                                default_place_policy_json(),
                                created_at,
                            ],
                        )?;
                        target.execute(
                            "INSERT INTO place_source_refs(
                           place_id,classification,source_system,source_address,source_id,
                           source_record_digest,mode,theme,phase,source_turn_number,source_status,
                           participant_public_ids,facilitator_subject_id,source_done_count,
                           source_max_turns,metadata,updated_at
                         ) VALUES(?1,'config_only','discord',?2,?3,NULL,NULL,NULL,NULL,NULL,NULL,
                                  NULL,NULL,NULL,NULL,NULL,?4)",
                            params![place_id, channel, channel.as_bytes(), created_at],
                        )?;
                        place_rows += 1;
                        place_id
                    }
                    (1, Some(place_id)) => place_id,
                    _ => {
                        let mut row_accounting = accounting.start_streamed_row(&table, row);
                        raw.add(
                            &table,
                            row,
                            "discord-channel-policy-router-v1:multiple_place_matches",
                        )?;
                        row_accounting.raw();
                        accounting.finish_streamed_row(row_accounting)?;
                        return Ok(());
                    }
                };
                current_channel = Some(channel.to_owned());
                current_place = Some(place_id);
            }
            let place_id = current_place.expect("channel transition always assigns a place");
            match parse_channel_policy(&table, row, place_id, agents, captured_at) {
                Ok(policy) => {
                    let key = match policy.subject_id {
                        Some(subject) => format!("subject:{place_id}:discord:{subject}"),
                        None => format!("default:{place_id}:discord"),
                    };
                    candidates.entry(key).or_default().push(policy);
                }
                Err(reason) => {
                    let mut row_accounting = accounting.start_streamed_row(&table, row);
                    raw.add(&table, row, reason)?;
                    row_accounting.raw();
                    accounting.finish_streamed_row(row_accounting)?;
                }
            }
            Ok(())
        },
    )?;
    flush_channel_policy_candidates(
        target,
        snapshot_digest,
        &table,
        provenance,
        raw,
        &mut accounting,
        &mut candidates,
        &mut default_rows,
        &mut subject_rows,
        &mut provenance_rows,
    )?;
    accounting.physical_rows = BTreeMap::from([
        ("places".into(), place_rows),
        ("place_source_refs".into(), place_rows),
        ("place_default_policies".into(), default_rows),
        ("place_subject_policies".into(), subject_rows),
        ("migration_provenance".into(), provenance_rows),
    ]);
    report.classes.push(accounting);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn flush_channel_policy_candidates(
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    table: &SourceTable,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector<'_, '_>,
    accounting: &mut ClassAccounting,
    candidates: &mut BTreeMap<String, Vec<ChannelPolicy>>,
    default_rows: &mut u64,
    subject_rows: &mut u64,
    provenance_rows: &mut u64,
) -> Result<()> {
    for (key, mut group) in std::mem::take(candidates) {
        group.sort_by(|left, right| left.row.source_key.cmp(&right.row.source_key));
        if group
            .iter()
            .skip(1)
            .any(|candidate| candidate.signature != group[0].signature)
        {
            for candidate in group {
                raw.add(
                    table,
                    &candidate.row,
                    "discord-channel-policy-router-v1:conflicting_policy_class",
                )?;
                let mut row_accounting = accounting.start_streamed_row(table, &candidate.row);
                row_accounting.raw();
                accounting.finish_streamed_row(row_accounting)?;
            }
            continue;
        }
        let selected = &group[0];
        let target_key = if let Some(subject_id) = selected.subject_id {
            target.execute(
                "INSERT INTO place_subject_policies(
                   place_id,kind_id,subject_id,admission,readable,writable,whitelisted,
                   heartbeat_enabled,heartbeat_interval_secs,heartbeat_instructions,
                   instructions_revision,source_row,source_updated_at
                 ) VALUES(?1,'discord',?2,?3,?4,?5,?6,?7,?8,?9,1,?10,?11)",
                params![
                    selected.place_id,
                    subject_id,
                    if selected.whitelisted {
                        "open"
                    } else {
                        "closed"
                    },
                    selected.readable,
                    selected.writable,
                    selected.whitelisted,
                    selected.heartbeat_enabled,
                    selected.heartbeat_interval_secs,
                    selected.heartbeat_instructions,
                    selected.row.row_values,
                    selected.source_updated_at,
                ],
            )?;
            *subject_rows += 1;
            composite_key(&[
                source::SqliteValue::Integer(selected.place_id),
                text("discord"),
                source::SqliteValue::Integer(subject_id),
            ])
        } else {
            let default_id = deterministic_uuid(snapshot_digest, key.as_bytes());
            target.execute(
                "INSERT INTO place_default_policies(
                   default_id,place_id,kind_id,resolution,source_row,source_updated_at
                 ) VALUES(?1,?2,'discord','active',?3,?4)",
                params![
                    default_id,
                    selected.place_id,
                    selected.row.row_values,
                    selected.source_updated_at,
                ],
            )?;
            *default_rows += 1;
            composite_key(&[text(&default_id)])
        };
        let entity = if selected.subject_id.is_some() {
            "place_subject_policies"
        } else {
            "place_default_policies"
        };
        for candidate in group {
            provenance.write(target, entity, &target_key, table, &candidate.row)?;
            *provenance_rows += 1;
            let mut row_accounting = accounting.start_streamed_row(table, &candidate.row);
            row_accounting.canonical();
            accounting.finish_streamed_row(row_accounting)?;
        }
    }
    Ok(())
}

fn parse_channel_policy(
    table: &SourceTable,
    row: &SourceRow,
    place_id: i64,
    agents: &BTreeMap<String, i64>,
    captured_at: i64,
) -> std::result::Result<ChannelPolicy, &'static str> {
    let agent_id = table
        .text(row, "agent_id")
        .ok_or("discord-channel-policy-router-v1:noncanonical_storage")?;
    let subject_id = if agent_id.is_empty() {
        None
    } else {
        Some(
            *agents
                .get(agent_id)
                .ok_or("discord-channel-policy-router-v1:unknown_owner")?,
        )
    };
    let boolean = |column| {
        table
            .integer(row, column)
            .map(|value| value != 0)
            .ok_or("discord-channel-policy-router-v1:noncanonical_storage")
    };
    let readable = boolean("readable")?;
    let writable = boolean("writable")?;
    let whitelisted = boolean("whitelisted")?;
    let heartbeat_enabled = boolean("heartbeat_enabled")?;
    let heartbeat_interval_secs = table
        .nullable_integer(row, "heartbeat_interval_secs")
        .ok_or("discord-channel-policy-router-v1:noncanonical_storage")?;
    let heartbeat_instructions = table
        .text(row, "heartbeat_instructions")
        .ok_or("discord-channel-policy-router-v1:noncanonical_storage")?
        .to_owned();
    let source_updated_at = table
        .text(row, "updated_at")
        .and_then(parse_utc_nanos)
        .unwrap_or(captured_at);
    let signature = serde_json::to_vec(&serde_json::json!({
        "heartbeat_enabled": heartbeat_enabled,
        "heartbeat_instructions": heartbeat_instructions,
        "heartbeat_interval_secs": heartbeat_interval_secs,
        "readable": readable,
        "whitelisted": whitelisted,
        "writable": writable,
    }))
    .map_err(|_| "discord-channel-policy-router-v1:policy_encoding")?;
    Ok(ChannelPolicy {
        row: row.clone(),
        place_id,
        subject_id,
        readable,
        writable,
        whitelisted,
        heartbeat_enabled,
        heartbeat_interval_secs,
        heartbeat_instructions,
        source_updated_at,
        signature,
    })
}

pub(crate) fn next_integer_id(target: &Transaction<'_>, table: &str) -> Result<i64> {
    let current = target.query_row(
        &format!("SELECT COALESCE(MAX(id),0) FROM {table}"),
        [],
        |row| row.get::<_, i64>(0),
    )?;
    current
        .checked_add(1)
        .ok_or_else(|| ConverterError::Accounting(format!("{table} id overflow")))
}

#[derive(Clone, Debug)]
struct LiveHistoryPlace {
    place_id: i64,
}

#[allow(clippy::too_many_arguments)]
fn assemble_history_and_events(
    source: &Connection,
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    migration_epoch: i64,
    agents: &BTreeMap<String, i64>,
    instances: &MigrationInstanceSet,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<()> {
    let mut live_sessions = BTreeMap::<(String, String), LiveHistoryPlace>::new();
    assemble_sessions(
        source,
        target,
        snapshot_digest,
        migration_epoch,
        agents,
        instances,
        &mut live_sessions,
        raw,
        report,
    )?;
    seed_memory_live_bindings(
        source,
        target,
        snapshot_digest,
        migration_epoch,
        agents,
        instances,
        &mut live_sessions,
    )?;
    assemble_pending_interactions(source, target, agents, instances, raw, report)?;
    assemble_memory_history(
        source,
        target,
        snapshot_digest,
        migration_epoch,
        agents,
        &live_sessions,
        raw,
        report,
    )?;
    reconcile_migrated_routes(target)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn assemble_sessions(
    source: &Connection,
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    migration_epoch: i64,
    agents: &BTreeMap<String, i64>,
    instances: &MigrationInstanceSet,
    live_sessions: &mut BTreeMap<(String, String), LiveHistoryPlace>,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "sessions")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "sessions")?;
    table.require_exact_columns(&[
        "id",
        "mode",
        "theme",
        "phase",
        "turn_number",
        "status",
        "participant_ids_json",
        "facilitator_id",
        "done_count",
        "max_turns",
        "metadata_json",
        "created_at",
        "updated_at",
    ])?;
    let mut accounting = ClassAccounting::streaming(table.name, "session_place", BTreeMap::new());
    let mut place_rows = 0_u64;
    let mut membership_rows = 0_u64;
    let mut child_parents = Vec::<(i64, i64, String)>::new();
    let subject_public_ids = agents
        .iter()
        .map(|(public_id, subject_id)| (*subject_id, public_id.clone()))
        .collect::<BTreeMap<_, _>>();
    table.for_each_row(source, "id COLLATE BINARY,rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        let result = parse_session_row(&table, row, agents);
        let parsed = match result {
            Ok(parsed) => parsed,
            Err(reason) => {
                raw.add(&table, row, reason)?;
                row_accounting.raw();
                accounting.finish_streamed_row(row_accounting)?;
                return Ok(());
            }
        };
        let live = session_live_address(&parsed.id, parsed.metadata.as_deref(), None);
        let child_parent = (parsed.mode == "subtask")
            .then(|| {
                parsed
                    .metadata
                    .as_deref()
                    .and_then(|metadata| serde_json::from_str::<serde_json::Value>(metadata).ok())
                    .and_then(|metadata| {
                        metadata
                            .get("parent_session_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .zip(parsed.participant_subject_ids.first().copied())
            })
            .flatten();
        let place = if let Some((kind, address, address_kind, guild_id)) = live {
            let place = ensure_live_place_and_bindings(
                target,
                snapshot_digest,
                migration_epoch,
                &kind,
                &address,
                &address_kind,
                guild_id.as_deref(),
                parsed.facilitator_subject_id,
                instances,
            )?;
            live_sessions.insert(
                (parsed.facilitator_public_id.clone(), parsed.id.clone()),
                place.clone(),
            );
            place
        } else {
            let place_id = next_integer_id(target, "places")?;
            let is_child = parsed.mode == "subtask";
            let classification = if is_child { "child" } else { "legacy_general" };
            let public_key = if is_child {
                format!(
                    "child:opencrab:{}",
                    URL_SAFE_NO_PAD.encode(parsed.id.as_bytes())
                )
            } else {
                format!(
                    "legacy:opencrab:{}",
                    URL_SAFE_NO_PAD.encode(parsed.id.as_bytes())
                )
            };
            target.execute(
                "INSERT INTO places(
                   id,address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,
                   created_at,closed_at,close_reason
                 ) VALUES(?1,?2,NULL,?3,NULL,NULL,?4,?5,?6)",
                params![
                    place_id,
                    public_key,
                    default_place_policy_json(),
                    parsed.created_at,
                    parsed.closed_at,
                    parsed.close_reason,
                ],
            )?;
            target.execute(
                "INSERT INTO place_source_refs(
                   place_id,classification,source_system,source_address,source_id,
                   source_record_digest,mode,theme,phase,source_turn_number,source_status,
                   participant_public_ids,facilitator_subject_id,source_done_count,
                   source_max_turns,metadata,updated_at
                 ) VALUES(?1,?2,'opencrab',?3,?4,?5,?6,?7,?8,?9,?10,?11,
                          ?12,?13,?14,?15,?16)",
                params![
                    place_id,
                    classification,
                    parsed.id,
                    parsed.id.as_bytes(),
                    row.row_digest.as_slice(),
                    parsed.mode,
                    parsed.theme,
                    parsed.phase,
                    parsed.turn_number,
                    parsed.status,
                    parsed.participant_json,
                    parsed.facilitator_subject_id,
                    parsed.done_count,
                    parsed.max_turns,
                    parsed.metadata,
                    parsed.updated_at,
                ],
            )?;
            place_rows += 1;
            LiveHistoryPlace {
                place_id,
            }
        };
        if let Some((parent_session_id, child_subject_id)) = child_parent {
            child_parents.push((place.place_id, child_subject_id, parent_session_id));
        }
        for subject_id in parsed.participant_subject_ids {
            membership_rows += target.execute(
                "INSERT OR IGNORE INTO memberships(place_id,subject_id,role,joined_at,shared_seen_seq)
                 VALUES(?1,?2,'participant',?3,0)",
                params![place.place_id, subject_id, parsed.created_at],
            )? as u64;
        }
        row_accounting.canonical();
        accounting.finish_streamed_row(row_accounting)?;
        Ok(())
    })?;
    for (child_place_id, child_subject_id, parent_session_id) in child_parents {
        let public_id = subject_public_ids.get(&child_subject_id).ok_or_else(|| {
            ConverterError::Accounting(format!(
                "child session subject {child_subject_id} has no in-memory public_id"
            ))
        })?;
        let parent_place_id = live_sessions
            .get(&(public_id.clone(), parent_session_id.clone()))
            .map(|place| place.place_id)
            .or_else(|| {
                target
                    .query_row(
                        "SELECT place_id FROM place_source_refs
                         WHERE source_system='opencrab' AND source_address=?1",
                        [&parent_session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .ok()
                    .flatten()
            });
        if let Some(parent_place_id) = parent_place_id {
            target.execute(
                "UPDATE places SET parent_id=?1 WHERE id=?2",
                params![parent_place_id, child_place_id],
            )?;
        }
    }
    accounting.physical_rows = BTreeMap::from([
        ("places".into(), place_rows),
        ("place_source_refs".into(), place_rows),
        ("memberships".into(), membership_rows),
    ]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedSession {
    id: String,
    mode: String,
    theme: String,
    phase: String,
    turn_number: i64,
    status: String,
    participant_json: String,
    participant_subject_ids: Vec<i64>,
    facilitator_subject_id: Option<i64>,
    facilitator_public_id: String,
    done_count: i64,
    max_turns: Option<i64>,
    metadata: Option<String>,
    created_at: i64,
    updated_at: i64,
    closed_at: Option<i64>,
    close_reason: Option<String>,
}

fn parse_session_row(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
) -> std::result::Result<ParsedSession, &'static str> {
    let text = |column| {
        table
            .text(row, column)
            .map(str::to_owned)
            .ok_or("sessions:noncanonical_storage")
    };
    let id = text("id")?;
    let participant_json = text("participant_ids_json")?;
    let participants = serde_json::from_str::<serde_json::Value>(&participant_json)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .ok_or("sessions:invalid_participant_ids_json")?;
    let mut participant_subject_ids = Vec::new();
    for participant in participants {
        let public_id = participant
            .as_str()
            .ok_or("sessions:invalid_participant_ids_json")?;
        if let Some(subject) = agents.get(public_id) {
            if participant_subject_ids.contains(subject) {
                return Err("sessions:duplicate_membership");
            }
            participant_subject_ids.push(*subject);
        }
    }
    let facilitator_public_id = table
        .nullable_text(row, "facilitator_id")
        .ok_or("sessions:noncanonical_storage")?
        .unwrap_or("")
        .to_owned();
    let facilitator_subject_id = agents.get(&facilitator_public_id).copied();
    let metadata = table
        .nullable_text(row, "metadata_json")
        .ok_or("sessions:noncanonical_storage")?
        .map(str::to_owned);
    if let Some(value) = &metadata {
        serde_json::from_str::<serde_json::Value>(value)
            .map_err(|_| "sessions:invalid_metadata_json")?;
    }
    let created_at = table
        .text(row, "created_at")
        .and_then(parse_utc_nanos)
        .ok_or("sessions:invalid_created_at")?;
    let updated_at = table
        .text(row, "updated_at")
        .and_then(parse_utc_nanos)
        .ok_or("sessions:invalid_updated_at")?;
    let status = text("status")?;
    let (closed_at, close_reason) = if matches!(
        status.as_str(),
        "closed" | "completed" | "done" | "archived"
    ) {
        (Some(updated_at), Some(format!("legacy-session-{status}")))
    } else {
        (None, None)
    };
    Ok(ParsedSession {
        id,
        mode: text("mode")?,
        theme: text("theme")?,
        phase: text("phase")?,
        turn_number: table
            .integer(row, "turn_number")
            .ok_or("sessions:noncanonical_storage")?,
        status,
        participant_json,
        participant_subject_ids,
        facilitator_subject_id,
        facilitator_public_id,
        done_count: table
            .integer(row, "done_count")
            .ok_or("sessions:noncanonical_storage")?,
        max_turns: table
            .nullable_integer(row, "max_turns")
            .ok_or("sessions:noncanonical_storage")?,
        metadata,
        created_at,
        updated_at,
        closed_at,
        close_reason,
    })
}

fn session_live_address(
    session_id: &str,
    metadata: Option<&str>,
    agent_id: Option<&str>,
) -> Option<(String, String, String, Option<String>)> {
    if let Some(object) = metadata
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .and_then(|value| value.as_object().cloned())
    {
        let source = object.get("source").and_then(serde_json::Value::as_str);
        let address = object.get("channel_id").and_then(serde_json::Value::as_str);
        if let (Some(source @ ("discord" | "nostr")), Some(address)) = (source, address) {
            let is_dm = object
                .get("is_dm")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let guild = object
                .get("guild_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            return Some((
                source.to_owned(),
                address.to_owned(),
                if source == "discord" {
                    if is_dm {
                        "dm".into()
                    } else if guild.is_some() {
                        "guild".into()
                    } else {
                        "unknown".into()
                    }
                } else {
                    "timeline".into()
                },
                guild,
            ));
        }
    }
    let agent = agent_id?;
    let rest = session_id.strip_prefix(&format!("discord-{agent}-"))?;
    let (guild, channel) = rest.split_once('-')?;
    if channel.is_empty() || !channel.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if guild.is_empty() {
        Some(("discord".into(), channel.into(), "dm".into(), None))
    } else if guild.bytes().all(|byte| byte.is_ascii_digit()) {
        Some((
            "discord".into(),
            channel.into(),
            "guild".into(),
            Some(guild.into()),
        ))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn seed_memory_live_bindings(
    source: &Connection,
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    migration_epoch: i64,
    agents: &BTreeMap<String, i64>,
    instances: &MigrationInstanceSet,
    live_sessions: &mut BTreeMap<(String, String), LiveHistoryPlace>,
) -> Result<()> {
    if !SourceTable::exists(source, "memory_sessions")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "memory_sessions")?;
    table.require_exact_columns(&[
        "id",
        "agent_id",
        "session_id",
        "log_type",
        "content",
        "speaker_id",
        "turn_number",
        "metadata_json",
        "created_at",
    ])?;
    table.for_each_row(source, "id,rowid", |row| {
        let (Some(agent_id), Some(session_id)) = (
            table.text(row, "agent_id"),
            table.text(row, "session_id"),
        ) else {
            return Ok(());
        };
        let Some(subject_id) = agents.get(agent_id).copied() else {
            return Ok(());
        };
        let metadata = table.nullable_text(row, "metadata_json").flatten();
        let live = session_live_address(session_id, metadata, Some(agent_id)).or_else(|| {
            (session_id == format!("nostr-{agent_id}"))
                .then(|| {
                    instances
                        .0
                        .iter()
                        .find(|instance| {
                            instance.kind_id == "nostr"
                                && target
                                    .query_row(
                                        "SELECT owner_subject_id FROM gate_instances WHERE instance_id=?1",
                                        [&instance.instance_id],
                                        |result| result.get::<_, Option<i64>>(0),
                                    )
                                    .ok()
                                    .flatten()
                                    == Some(subject_id)
                        })
                        .map(|instance| {
                            (
                                "nostr".into(),
                                format!("timeline:{}", instance.instance_id),
                                "timeline".into(),
                                None,
                            )
                        })
                })
                .flatten()
        });
        let Some((kind, address, address_kind, guild_id)) = live else {
            return Ok(());
        };
        let place = ensure_live_place_and_bindings(
            target,
            snapshot_digest,
            migration_epoch,
            &kind,
            &address,
            &address_kind,
            guild_id.as_deref(),
            Some(subject_id),
            instances,
        )?;
        target.execute(
            "INSERT OR IGNORE INTO memberships(place_id,subject_id,role,joined_at,shared_seen_seq)
             VALUES(?1,?2,'participant',?3,0)",
            params![place.place_id, subject_id, migration_epoch],
        )?;
        let key = (agent_id.to_owned(), session_id.to_owned());
        if let Some(existing) = live_sessions.insert(key.clone(), place.clone()) {
            if existing.place_id != place.place_id {
                return Err(ConverterError::Accounting(format!(
                    "history live session {key:?} resolves to multiple places"
                )));
            }
        }
        Ok(())
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ensure_live_place_and_bindings(
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    created_at: i64,
    kind: &str,
    address: &str,
    address_kind: &str,
    guild_id: Option<&str>,
    subject_id: Option<i64>,
    instances: &MigrationInstanceSet,
) -> Result<LiveHistoryPlace> {
    let matches = target.query_row(
        "SELECT COUNT(*),MIN(place_id),MIN(classification) FROM place_source_refs
         WHERE source_system=?1 AND source_address=?2",
        params![kind, address],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    )?;
    let place_id = match matches {
        (0, _, _) => {
            let place_id = next_integer_id(target, "places")?;
            target.execute(
                "INSERT INTO places(
                   id,address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,
                   created_at,closed_at,close_reason
                 ) VALUES(?1,?2,NULL,?3,NULL,NULL,?4,NULL,NULL)",
                params![
                    place_id,
                    format!("live:{kind}:{}", URL_SAFE_NO_PAD.encode(address.as_bytes())),
                    default_place_policy_json(),
                    created_at,
                ],
            )?;
            target.execute(
                "INSERT INTO place_source_refs(
                   place_id,classification,source_system,source_address,source_id,
                   source_record_digest,mode,theme,phase,source_turn_number,source_status,
                   participant_public_ids,facilitator_subject_id,source_done_count,
                   source_max_turns,metadata,updated_at
                 ) VALUES(?1,'live',?2,?3,?4,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,?5)",
                params![place_id, kind, address, address.as_bytes(), created_at],
            )?;
            place_id
        }
        (1, Some(place_id), Some(classification)) => {
            if classification == "config_only" {
                target.execute(
                    "UPDATE place_source_refs SET classification='live' WHERE place_id=?1",
                    [place_id],
                )?;
            }
            place_id
        }
        _ => {
            return Err(ConverterError::Accounting(format!(
                "live place {kind}:{address} does not resolve exactly once"
            )))
        }
    };
    let scope_id = deterministic_uuid(
        snapshot_digest,
        format!("origin-scope\0{kind}\0{address}").as_bytes(),
    );
    target.execute(
        "INSERT OR IGNORE INTO external_origin_scopes(
           scope_id,kind_id,address,mode,instance_id,place_id
         ) VALUES(?1,?2,?3,'kind_address',NULL,?4)",
        params![scope_id, kind, address, place_id],
    )?;
    let metadata = if kind == "discord" {
        serde_json::to_vec(&serde_json::json!({
            "address_kind": address_kind,
            "guild_id": guild_id,
        }))?
    } else {
        serde_json::to_vec(&serde_json::json!({"mode":"timeline"}))?
    };
    let metadata_schema = if kind == "discord" {
        "gate-binding/discord/v1"
    } else {
        "gate-binding/nostr/v1"
    };
    for instance in instances
        .0
        .iter()
        .filter(|instance| instance.kind_id == kind)
    {
        let owner = target.query_row(
            "SELECT owner_subject_id FROM gate_instances WHERE instance_id=?1",
            [&instance.instance_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        if owner.is_some() && owner != subject_id {
            continue;
        }
        let binding_id = deterministic_uuid(
            snapshot_digest,
            [
                b"binding\0".as_slice(),
                instance.uuid_bytes.as_slice(),
                b"\0",
                address.as_bytes(),
            ]
            .concat()
            .as_slice(),
        );
        let digest = Sha256::digest(&metadata);
        target.execute(
            "INSERT OR IGNORE INTO gate_bindings(
               binding_id,place_id,instance_id,address,label,origin_scope_id,
               binding_metadata_schema_id,binding_metadata_bytes,binding_metadata_digest
             ) VALUES(?1,?2,?3,?4,NULL,?5,?6,?7,?8)",
            params![
                binding_id,
                place_id,
                instance.instance_id,
                address,
                scope_id,
                metadata_schema,
                metadata,
                digest.as_slice(),
            ],
        )?;
    }
    Ok(LiveHistoryPlace { place_id })
}

fn assemble_pending_interactions(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &BTreeMap<String, i64>,
    instances: &MigrationInstanceSet,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "pending_interactions")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "pending_interactions")?;
    table.require_exact_columns(&[
        "id",
        "agent_id",
        "session_id",
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
    ])?;
    let mut accounting =
        ClassAccounting::streaming(table.name, "pending_interaction", BTreeMap::new());
    let mut next_id = 1_i64;
    let mut response_rows = 0_u64;
    table.for_each_row(source, "id COLLATE BINARY,rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        let source_id = match table.nullable_text(row, "id") {
            Some(Some(value)) => value,
            Some(None) => {
                raw.add(&table, row, "pending-interaction-router-v2:null_source_key")?;
                row_accounting.raw();
                accounting.finish_streamed_row(row_accounting)?;
                return Ok(());
            }
            None => {
                raw.add(
                    &table,
                    row,
                    "pending-interaction-router-v2:noncanonical_storage",
                )?;
                row_accounting.raw();
                accounting.finish_streamed_row(row_accounting)?;
                return Ok(());
            }
        };
        let parsed = parse_pending_interaction(&table, row, source_id, agents, target);
        let parsed = match parsed {
            Ok(value) => value,
            Err(reason) => {
                raw.add(&table, row, reason)?;
                row_accounting.raw();
                accounting.finish_streamed_row(row_accounting)?;
                return Ok(());
            }
        };
        let response = if let Some(response) = parsed.response {
            match classify_interaction_responder(
                target,
                instances,
                &parsed.surface,
                &response.responder_external_id,
            ) {
                Ok((kind, subject)) => Some((response, kind, subject)),
                Err(reason) => {
                    raw.add(&table, row, reason)?;
                    row_accounting.raw();
                    accounting.finish_streamed_row(row_accounting)?;
                    return Ok(());
                }
            }
        } else {
            None
        };
        let interaction_id = next_id;
        next_id = next_id
            .checked_add(1)
            .ok_or_else(|| ConverterError::Accounting("pending interaction id overflow".into()))?;
        target.execute(
            "INSERT INTO interactions(
               id,owner_subject_id,place_id,binding_id,surface,source_address,
               source_message_id,surface_id,surface_payload,payload,owner_only,timeout_secs,
               state,source_record_key,created_at,updated_at,deadline
             ) VALUES(?1,?2,?3,NULL,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                interaction_id,
                parsed.owner_subject_id,
                parsed.place_id,
                parsed.surface,
                parsed.source_address,
                parsed.source_message_id,
                parsed.surface_id,
                parsed.surface_payload,
                parsed.payload,
                parsed.owner_only,
                parsed.timeout_secs,
                parsed.state,
                source_id,
                parsed.created_at,
                parsed.updated_at,
                parsed.deadline,
            ],
        )?;
        if let Some((response, responder_kind, responder_subject_id)) = response {
            target.execute(
                "INSERT INTO interaction_responses(
                   interaction_id,interaction_source_key,response,responder_kind,
                   responder_subject_id,responder_external_id,responded_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    interaction_id,
                    source_id,
                    response.response,
                    responder_kind,
                    responder_subject_id,
                    response.responder_external_id,
                    response.responded_at,
                ],
            )?;
            response_rows += 1;
        }
        row_accounting.canonical();
        accounting.finish_streamed_row(row_accounting)?;
        Ok(())
    })?;
    accounting.physical_rows = BTreeMap::from([
        ("interactions".into(), accounting.canonical_outcomes),
        ("interaction_responses".into(), response_rows),
    ]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedInteractionResponse {
    response: String,
    responder_external_id: String,
    responded_at: i64,
}

struct ParsedInteraction {
    owner_subject_id: i64,
    place_id: i64,
    surface: String,
    source_address: String,
    source_message_id: Option<String>,
    surface_id: String,
    surface_payload: String,
    payload: String,
    owner_only: bool,
    timeout_secs: i64,
    state: &'static str,
    created_at: i64,
    updated_at: i64,
    deadline: i64,
    response: Option<ParsedInteractionResponse>,
}

fn parse_pending_interaction(
    table: &SourceTable,
    row: &SourceRow,
    _source_id: &str,
    agents: &BTreeMap<String, i64>,
    target: &Transaction<'_>,
) -> std::result::Result<ParsedInteraction, &'static str> {
    let required = |column| {
        table
            .text(row, column)
            .map(str::to_owned)
            .ok_or("pending-interaction-router-v2:noncanonical_storage")
    };
    let owner = required("agent_id")?;
    let owner_subject_id = *agents
        .get(&owner)
        .ok_or("pending-interaction-router-v2:unresolved_owner")?;
    let surface = required("platform")?;
    if !matches!(surface.as_str(), "discord" | "nostr" | "web" | "rest") {
        return Err("pending-interaction-router-v2:unknown_surface");
    }
    let source_address = required("channel_id")?;
    let place_matches = target
        .query_row(
            "SELECT COUNT(*),MIN(place_id) FROM place_source_refs
             WHERE source_system=?1 AND source_address=?2",
            params![surface, source_address],
            |result| Ok((result.get::<_, i64>(0)?, result.get::<_, Option<i64>>(1)?)),
        )
        .map_err(|_| "pending-interaction-router-v2:place_query_failed")?;
    let place_id = match place_matches {
        (1, Some(value)) => value,
        _ => return Err("pending-interaction-router-v2:unresolved_place"),
    };
    let surface_id = required("surface_id")?;
    let surface_payload = required("a2ui_components_json")?;
    let components: serde_json::Value = serde_json::from_str(&surface_payload)
        .map_err(|_| "pending-interaction-router-v2:invalid_components")?;
    let owner_only = table
        .integer(row, "owner_only")
        .map(|value| value != 0)
        .ok_or("pending-interaction-router-v2:noncanonical_storage")?;
    let timeout_secs = table
        .integer(row, "timeout_secs")
        .ok_or("pending-interaction-router-v2:noncanonical_storage")?;
    let created_at = table
        .text(row, "created_at")
        .and_then(parse_utc_nanos)
        .ok_or("pending-interaction-router-v2:invalid_created_at")?;
    let updated_at = table
        .text(row, "updated_at")
        .and_then(parse_utc_nanos)
        .ok_or("pending-interaction-router-v2:invalid_updated_at")?;
    let deadline = timeout_secs
        .checked_mul(1_000_000_000)
        .and_then(|delta| created_at.checked_add(delta))
        .ok_or("pending-interaction-router-v2:deadline_overflow")?;
    let payload = serde_json::to_string(&serde_json::json!({
        "components": components,
        "owner_only": owner_only,
        "surface_id": surface_id,
    }))
    .map_err(|_| "pending-interaction-router-v2:payload_encoding")?;
    let status = required("status")?;
    let response_json = table
        .nullable_text(row, "response_json")
        .ok_or("pending-interaction-router-v2:noncanonical_storage")?;
    let responder_id = table
        .nullable_text(row, "responder_id")
        .ok_or("pending-interaction-router-v2:noncanonical_storage")?;
    let responded_at = table
        .nullable_text(row, "responded_at")
        .ok_or("pending-interaction-router-v2:noncanonical_storage")?;
    let all_null = response_json.is_none() && responder_id.is_none() && responded_at.is_none();
    let all_present = response_json.is_some() && responder_id.is_some() && responded_at.is_some();
    let state = match (status.as_str(), all_null, all_present) {
        ("pending", true, false) => "pending",
        ("responded", false, true) => "responded",
        ("timeout" | "expired", true, false) | ("timeout" | "expired", false, true) => "expired",
        _ => return Err("pending-interaction-router-v2:noncanonical_interaction"),
    };
    let response = if all_present {
        let response = response_json.expect("all_present").to_owned();
        serde_json::from_str::<serde_json::Value>(&response)
            .map_err(|_| "pending-interaction-router-v2:invalid_response")?;
        Some(ParsedInteractionResponse {
            response,
            responder_external_id: responder_id.expect("all_present").to_owned(),
            responded_at: responded_at
                .and_then(parse_utc_nanos)
                .ok_or("pending-interaction-router-v2:invalid_responded_at")?,
        })
    } else {
        None
    };
    Ok(ParsedInteraction {
        owner_subject_id,
        place_id,
        surface,
        source_address,
        source_message_id: table
            .nullable_text(row, "message_id")
            .ok_or("pending-interaction-router-v2:noncanonical_storage")?
            .map(str::to_owned),
        surface_id,
        surface_payload,
        payload,
        owner_only,
        timeout_secs,
        state,
        created_at,
        updated_at,
        deadline,
        response,
    })
}

fn classify_interaction_responder(
    target: &Transaction<'_>,
    instances: &MigrationInstanceSet,
    surface: &str,
    responder: &str,
) -> std::result::Result<(&'static str, Option<i64>), &'static str> {
    if responder == "system" {
        return Ok(("system", None));
    }
    let mut subjects = BTreeSet::new();
    for instance in instances
        .0
        .iter()
        .filter(|instance| instance.kind_id == surface)
    {
        let mut statement = target
            .prepare(
                "SELECT subject_id FROM gate_subject_identities
                 WHERE instance_id=?1 AND external_id=?2",
            )
            .map_err(|_| "pending-interaction-router-v2:responder_query_failed")?;
        let rows = statement
            .query_map(params![instance.instance_id, responder], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|_| "pending-interaction-router-v2:responder_query_failed")?;
        for subject in rows {
            subjects.insert(
                subject.map_err(|_| "pending-interaction-router-v2:responder_query_failed")?,
            );
        }
    }
    match subjects.len() {
        0 => Ok(("unknown", None)),
        1 => Ok(("subject", subjects.into_iter().next())),
        _ => Err("pending-interaction-router-v2:responder_subject_conflict"),
    }
}

struct HistoryPlaceState {
    place_id: i64,
    subject_id: i64,
    session_id: String,
    event_seq: i64,
    live_place_id: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
fn assemble_memory_history(
    source: &Connection,
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    migration_epoch: i64,
    agents: &BTreeMap<String, i64>,
    live_sessions: &BTreeMap<(String, String), LiveHistoryPlace>,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "memory_sessions")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "memory_sessions")?;
    table.require_exact_columns(&[
        "id",
        "agent_id",
        "session_id",
        "log_type",
        "content",
        "speaker_id",
        "turn_number",
        "metadata_json",
        "created_at",
    ])?;
    let mut accounting = ClassAccounting::streaming(table.name, "history_event", BTreeMap::new());
    let mut orphan_accounting =
        ClassAccounting::streaming(table.name, "orphan_history_raw", BTreeMap::new());
    let mut histories = BTreeMap::<(String, String), HistoryPlaceState>::new();
    let mut archive_rows = 0_u64;
    let mut event_rows = 0_u64;
    let mut journal_rows = 0_u64;
    let mut audit_rows = 0_u64;
    let mut place_rows = 0_u64;
    let mut orphan_raw_rows = 0_u64;
    let mut journal_id = target
        .query_row(
            "SELECT COALESCE(MAX(journal_id),0) FROM private_journal",
            [],
            |row| row.get::<_, i64>(0),
        )?
        .checked_add(1)
        .ok_or_else(|| ConverterError::Accounting("private journal id overflow".into()))?;
    let mut previous_source_id = None;
    table.for_each_row(source, "id,rowid", |row| {
        let source_row_id = table.integer(row, "id").ok_or_else(|| {
            ConverterError::Accounting("history row has missing/non-integer source id".into())
        })?;
        if previous_source_id == Some(source_row_id) {
            return Err(ConverterError::Accounting(format!(
                "history source id {source_row_id} is duplicated"
            )));
        }
        previous_source_id = Some(source_row_id);
        let log_kind = table.text(row, "log_type").ok_or_else(|| {
            ConverterError::Accounting(format!(
                "history row {source_row_id} has non-text log_type"
            ))
        })?;
        if matches!(log_kind, "tool_call" | "system") {
            let mut row_accounting = accounting.start_streamed_row(&table, row);
            row_accounting.dropped("agreed-drop-tool-call-system");
            accounting.finish_streamed_row(row_accounting)?;
            return Ok(());
        }
        let agent_id = table.text(row, "agent_id").ok_or_else(|| {
            ConverterError::Accounting(format!(
                "history row {source_row_id} has non-text agent_id"
            ))
        })?;
        let Some(subject_id) = agents.get(agent_id).copied() else {
            raw.add(&table, row, "history-per-agent-router-v2:orphan_agent_id")?;
            let mut row_accounting = orphan_accounting.start_streamed_row(&table, row);
            row_accounting.raw();
            orphan_accounting.finish_streamed_row(row_accounting)?;
            orphan_raw_rows += 1;
            return Ok(());
        };
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        let session_id = table.text(row, "session_id").ok_or_else(|| {
            ConverterError::Accounting(format!(
                "history row {source_row_id} has non-text session_id"
            ))
        })?;
        let content = table.bytes(row, "content").ok_or_else(|| {
            ConverterError::Accounting(format!(
                "history row {source_row_id} has non-text/blob content"
            ))
        })?;
        let created_at = table
            .text(row, "created_at")
            .and_then(parse_utc_nanos)
            .ok_or_else(|| {
                ConverterError::Accounting(format!(
                    "history row {source_row_id} has invalid created_at"
                ))
            })?;
        let speaker = table.nullable_bytes(row, "speaker_id").ok_or_else(|| {
            ConverterError::Accounting(format!(
                "history row {source_row_id} has invalid speaker storage"
            ))
        })?;
        let turn_number = table.nullable_integer(row, "turn_number").ok_or_else(|| {
            ConverterError::Accounting(format!(
                "history row {source_row_id} has invalid turn_number storage"
            ))
        })?;
        let metadata = table.nullable_bytes(row, "metadata_json").ok_or_else(|| {
            ConverterError::Accounting(format!(
                "history row {source_row_id} has invalid metadata storage"
            ))
        })?;
        let history_key = (agent_id.to_owned(), session_id.to_owned());
        if !histories.contains_key(&history_key) {
            let place_id = next_integer_id(target, "places")?;
            target.execute(
                "INSERT INTO places(
                   id,address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,
                   created_at,closed_at,close_reason
                 ) VALUES(?1,?2,NULL,?3,NULL,NULL,?4,?5,'legacy-history-import')",
                params![
                    place_id,
                    format!(
                        "legacy-agent:opencrab:{}:{}",
                        URL_SAFE_NO_PAD.encode(agent_id.as_bytes()),
                        URL_SAFE_NO_PAD.encode(session_id.as_bytes())
                    ),
                    default_place_policy_json(),
                    created_at,
                    migration_epoch,
                ],
            )?;
            target.execute(
                "INSERT INTO memberships(place_id,subject_id,role,joined_at,shared_seen_seq)
                 VALUES(?1,?2,'participant',?3,0)",
                params![place_id, subject_id, created_at],
            )?;
            histories.insert(
                history_key.clone(),
                HistoryPlaceState {
                    place_id,
                    subject_id,
                    session_id: session_id.into(),
                    event_seq: 0,
                    live_place_id: live_sessions
                        .get(&history_key)
                        .map(|place| place.place_id),
                },
            );
            place_rows += 1;
        }
        let state = histories.get_mut(&history_key).expect("inserted above");
        if state.subject_id != subject_id {
            return Err(ConverterError::Accounting(format!(
                "history place for row {source_row_id} crosses subjects"
            )));
        }
        target.execute(
            "INSERT INTO legacy_history_archive(
               source_db_digest,source_row_id,source_agent_id,source_session_id,log_kind,
               content,speaker_source_id,source_turn_number,metadata,created_at,
               created_at_source,metadata_source,row_digest,owner_subject_id,proposed_place_id,
               owner_decision_revision
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                snapshot_digest.as_slice(),
                source_row_id,
                agent_id.as_bytes(),
                session_id.as_bytes(),
                log_kind,
                content,
                speaker,
                turn_number,
                metadata,
                created_at,
                table.encoded_value(row, "created_at"),
                table.encoded_value(row, "metadata_json"),
                row.row_digest.as_slice(),
                subject_id,
                state.place_id,
                "design-v22:per-agent-history-v1",
            ],
        )?;
        archive_rows += 1;

        let speaker_is_agent = speaker == Some(agent_id.as_bytes());
        if matches!(log_kind, "speech" | "message") && speaker_is_agent {
            state.event_seq += 1;
            insert_history_event(
                target,
                state.place_id,
                state.event_seq,
                "spoke",
                Some(subject_id),
                None,
                content,
                created_at,
                None,
            )?;
            event_rows += 1;
        } else if matches!(log_kind, "speech" | "message") && speaker.is_none() {
            insert_history_audit(
                target,
                snapshot_digest,
                source_row_id,
                "conversation",
                subject_id,
                state.place_id,
                agent_id,
                content,
                created_at,
                metadata,
                "owner-private source row",
                log_kind,
            )?;
            audit_rows += 1;
        } else {
            match log_kind {
                "speech" | "message" => {
                    let external = std::str::from_utf8(speaker.expect("not null")).ok();
                    let author = external
                        .map(|value| {
                            resolve_external_speaker(
                                target,
                                state.live_place_id,
                                subject_id,
                                value,
                                metadata,
                                source_row_id,
                            )
                        })
                        .transpose()?
                        .flatten();
                    if let (Some(external), Some(author)) = (external, author) {
                        state.event_seq += 1;
                        insert_history_event(
                            target,
                            state.place_id,
                            state.event_seq,
                            "said",
                            Some(author),
                            Some(external),
                            content,
                            created_at,
                            None,
                        )?;
                        event_rows += 1;
                    } else {
                        insert_history_audit(
                            target,
                            snapshot_digest,
                            source_row_id,
                            "conversation",
                            subject_id,
                            state.place_id,
                            agent_id,
                            content,
                            created_at,
                            metadata,
                            "external speaker did not resolve",
                            log_kind,
                        )?;
                        audit_rows += 1;
                    }
                }
                "interaction_response" => {
                    let object = inspected_metadata(metadata, source_row_id)?;
                    let interactions = resolve_history_interaction(
                        target,
                        state.live_place_id,
                        subject_id,
                        &object,
                    )?;
                    match interactions.as_slice() {
                        [_] => {
                            state.event_seq += 1;
                            insert_history_event(
                                target,
                                state.place_id,
                                state.event_seq,
                                "ui_action",
                                None,
                                object
                                    .get("responder_id")
                                    .and_then(serde_json::Value::as_str),
                                content,
                                created_at,
                                None,
                            )?;
                            event_rows += 1;
                        }
                        [_, _, ..] => {
                            return Err(ConverterError::Accounting(format!(
                                "history row {source_row_id} interaction join is multiple"
                            )))
                        }
                        _ => {
                            insert_history_audit(
                                target,
                                snapshot_digest,
                                source_row_id,
                                "activity",
                                subject_id,
                                state.place_id,
                                agent_id,
                                content,
                                created_at,
                                metadata,
                                "interaction did not resolve",
                                log_kind,
                            )?;
                            audit_rows += 1;
                        }
                    }
                }
                "tool_cancelled" => {
                    let object = inspected_metadata(metadata, source_row_id)?;
                    let matches = resolve_subtask_places(
                        source,
                        target,
                        agent_id,
                        session_id,
                        &object,
                        SubtaskJoin::Cancelled,
                    )?;
                    match matches.as_slice() {
                        [_] => {
                            state.event_seq += 1;
                            insert_history_event(
                                target,
                                state.place_id,
                                state.event_seq,
                                "interrupted",
                                None,
                                None,
                                content,
                                created_at,
                                Some(subject_id),
                            )?;
                            event_rows += 1;
                        }
                        [] => {
                            insert_history_audit(
                                target,
                                snapshot_digest,
                                source_row_id,
                                "activity",
                                subject_id,
                                state.place_id,
                                agent_id,
                                content,
                                created_at,
                                metadata,
                                "subtask cancellation did not resolve",
                                log_kind,
                            )?;
                            audit_rows += 1;
                        }
                        _ => {
                            return Err(ConverterError::Accounting(format!(
                                "history row {source_row_id} subtask join is multiple"
                            )))
                        }
                    }
                }
                "steer" => {
                    let object = inspected_metadata(metadata, source_row_id)?;
                    let matches = resolve_subtask_places(
                        source,
                        target,
                        agent_id,
                        session_id,
                        &object,
                        SubtaskJoin::Steer,
                    )?;
                    match matches.as_slice() {
                        [child_place_id] => {
                            let provenance = serde_json::to_vec(&serde_json::json!({
                                "source_db_digest": source::hex(&snapshot_digest),
                                "source_row_id": source_row_id,
                                "subtask_child_place_id": child_place_id,
                            }))?;
                            target.execute(
                                "INSERT INTO private_journal(
                                   journal_id,owner_subject_id,place_id,anchor_seq,content,created_at,provenance
                                 ) VALUES(?1,?2,?3,0,?4,?5,?6)",
                                params![
                                    journal_id,
                                    subject_id,
                                    child_place_id,
                                    content,
                                    created_at,
                                    provenance,
                                ],
                            )?;
                            journal_id = journal_id.checked_add(1).ok_or_else(|| {
                                ConverterError::Accounting("private journal id overflow".into())
                            })?;
                            journal_rows += 1;
                        }
                        [] => {
                            insert_history_audit(
                                target,
                                snapshot_digest,
                                source_row_id,
                                "task",
                                subject_id,
                                state.place_id,
                                agent_id,
                                content,
                                created_at,
                                metadata,
                                "child place did not resolve",
                                log_kind,
                            )?;
                            audit_rows += 1;
                        }
                        _ => {
                            return Err(ConverterError::Accounting(format!(
                                "history row {source_row_id} child place join is multiple"
                            )))
                        }
                    }
                }
                "task_event" => {
                    let object = inspected_metadata(metadata, source_row_id)?;
                    let matches = resolve_task_references(source, agent_id, session_id, &object)?;
                    match matches.as_slice() {
                        [task_id] => {
                            insert_history_audit_with_activity(
                                target,
                                snapshot_digest,
                                source_row_id,
                                "task",
                                subject_id,
                                state.place_id,
                                Some(*task_id),
                                agent_id,
                                content,
                                created_at,
                                metadata,
                                "task reference resolved",
                                log_kind,
                            )?;
                            audit_rows += 1;
                        }
                        [] => {
                            insert_history_audit(
                                target,
                                snapshot_digest,
                                source_row_id,
                                "task",
                                subject_id,
                                state.place_id,
                                agent_id,
                                content,
                                created_at,
                                metadata,
                                "task reference did not resolve",
                                log_kind,
                            )?;
                            audit_rows += 1;
                        }
                        _ => {
                            return Err(ConverterError::Accounting(format!(
                                "history row {source_row_id} task join is multiple"
                            )))
                        }
                    }
                }
                "tool_result" | "evaluation" => {
                    insert_history_audit(
                        target,
                        snapshot_digest,
                        source_row_id,
                        if log_kind == "tool_result" {
                            "tool_result"
                        } else {
                            "evaluation"
                        },
                        subject_id,
                        state.place_id,
                        agent_id,
                        content,
                        created_at,
                        metadata,
                        "typed legacy audit",
                        log_kind,
                    )?;
                    audit_rows += 1;
                }
                "inner_voice" => {
                    let provenance = serde_json::to_vec(&serde_json::json!({
                        "source_db_digest": source::hex(&snapshot_digest),
                        "source_row_id": source_row_id,
                    }))?;
                    target.execute(
                        "INSERT INTO private_journal(
                           journal_id,owner_subject_id,place_id,anchor_seq,content,created_at,provenance
                         ) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        params![
                            journal_id,
                            subject_id,
                            state.place_id,
                            state.event_seq,
                            content,
                            created_at,
                            provenance,
                        ],
                    )?;
                    journal_id = journal_id.checked_add(1).ok_or_else(|| {
                        ConverterError::Accounting("private journal id overflow".into())
                    })?;
                    journal_rows += 1;
                }
                _ => {
                    return Err(ConverterError::Accounting(format!(
                        "history row {source_row_id} has unknown non-drop log_type {log_kind:?}"
                    )))
                }
            }
        }
        row_accounting.canonical();
        accounting.finish_streamed_row(row_accounting)?;
        Ok(())
    })?;

    let mut link_groups = BTreeMap::<(i64, i64), Vec<&HistoryPlaceState>>::new();
    for state in histories
        .values()
        .filter(|state| state.live_place_id.is_some())
    {
        link_groups
            .entry((state.subject_id, state.live_place_id.expect("filtered")))
            .or_default()
            .push(state);
    }
    let mut history_source_rows = 0_u64;
    for ((subject_id, live_place_id), mut states) in link_groups {
        states.sort_by(|left, right| left.session_id.as_bytes().cmp(right.session_id.as_bytes()));
        for (ordinal, state) in states.into_iter().enumerate() {
            target.execute(
                "INSERT INTO subject_history_sources(
                   subject_id,live_place_id,history_place_id,ordinal,history_max_seq
                 ) VALUES(?1,?2,?3,?4,?5)",
                params![
                    subject_id,
                    live_place_id,
                    state.place_id,
                    ordinal as i64,
                    state.event_seq,
                ],
            )?;
            history_source_rows += 1;
        }
    }
    accounting.physical_rows = BTreeMap::from([
        ("legacy_history_archive".into(), archive_rows),
        ("places".into(), place_rows),
        ("memberships".into(), place_rows),
        ("events".into(), event_rows),
        ("private_journal".into(), journal_rows),
        ("legacy_audit_records".into(), audit_rows),
        ("subject_history_sources".into(), history_source_rows),
    ]);
    report.classes.push(accounting);
    if orphan_raw_rows != 0 {
        orphan_accounting.physical_rows =
            BTreeMap::from([("legacy_unowned_source_rows".into(), orphan_raw_rows)]);
        report.classes.push(orphan_accounting);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_history_event(
    target: &Transaction<'_>,
    place_id: i64,
    seq: i64,
    kind: &str,
    author_subject_id: Option<i64>,
    author_external_id: Option<&str>,
    content: &[u8],
    created_at: i64,
    for_subject_id: Option<i64>,
) -> Result<()> {
    let content_json = std::str::from_utf8(content)
        .map_err(|_| ConverterError::Accounting("history event content is not UTF-8".into()))?;
    target.execute(
        "INSERT INTO events(
           place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,
           reply_to_seq,target_seq,for_subject_id,created_at,attachments_json
         ) VALUES(?1,?2,?3,?4,?5,?6,'[]',NULL,NULL,?7,?8,'[]')",
        params![
            place_id,
            seq,
            kind,
            author_subject_id,
            author_external_id,
            content_json,
            for_subject_id,
            created_at,
        ],
    )?;
    Ok(())
}

fn resolve_external_speaker(
    target: &Transaction<'_>,
    live_place_id: Option<i64>,
    owner_subject_id: i64,
    external: &str,
    metadata: Option<&[u8]>,
    source_row_id: i64,
) -> Result<Option<i64>> {
    let object = inspected_metadata(metadata, source_row_id)?;
    let Some(source_kind @ ("discord" | "nostr")) =
        object.get("source").and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    let Some(place_id) = live_place_id else {
        return Ok(None);
    };
    let address_matches = if source_kind == "discord" {
        let Some(channel_id) = object.get("channel_id").and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        target.query_row(
            "SELECT COUNT(*) FROM place_source_refs
             WHERE place_id=?1 AND source_system='discord' AND source_address=?2",
            params![place_id, channel_id],
            |row| row.get::<_, i64>(0),
        )?
    } else {
        if object.get("pubkey").and_then(serde_json::Value::as_str) != Some(external) {
            return Ok(None);
        }
        target.query_row(
            "SELECT COUNT(*) FROM place_source_refs
             WHERE place_id=?1 AND source_system='nostr'",
            [place_id],
            |row| row.get::<_, i64>(0),
        )?
    };
    if address_matches != 1 {
        return Ok(None);
    }
    let mut statement = target.prepare(
        "SELECT i.subject_id
         FROM gate_bindings b
         JOIN gate_instances gi ON gi.instance_id=b.instance_id
         JOIN gate_subject_identities i ON i.instance_id=gi.instance_id
         WHERE b.place_id=?1 AND gi.kind_id=?2
           AND (gi.owner_subject_id=?3 OR gi.owner_subject_id IS NULL)
           AND i.external_id=?4
         ORDER BY gi.instance_id,i.subject_id",
    )?;
    let subjects = statement
        .query_map(
            params![place_id, source_kind, owner_subject_id, external],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let distinct = subjects.into_iter().collect::<BTreeSet<_>>();
    match distinct.len() {
        0 => Ok(None),
        1 => Ok(distinct.into_iter().next()),
        _ => Err(ConverterError::Accounting(format!(
            "external speaker {external:?} resolves to multiple subjects"
        ))),
    }
}

fn resolve_history_interaction(
    target: &Transaction<'_>,
    live_place_id: Option<i64>,
    owner_subject_id: i64,
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<i64>> {
    let Some(interaction_key) = metadata
        .get("interaction_id")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    let Some(surface_id) = metadata
        .get("surface_id")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    let Some(action_name) = metadata
        .get("action_name")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    let Some(component_id) = metadata
        .get("component_id")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    let Some(responder_id) = metadata
        .get("responder_id")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    let Some(place_id) = live_place_id else {
        return Ok(Vec::new());
    };
    let mut statement = target.prepare(
        "SELECT i.id,i.surface_payload,r.response
         FROM interactions i
         JOIN interaction_responses r ON r.interaction_id=i.id
         JOIN place_source_refs p ON p.place_id=i.place_id
           AND p.classification='live' AND p.source_system=i.surface
           AND p.source_address=i.source_address
         WHERE i.source_record_key=?1 AND i.owner_subject_id=?2 AND i.place_id=?3
           AND i.surface_id=?4 AND i.state IN ('responded','expired')
           AND r.interaction_source_key=?1 AND r.responder_external_id=?5
         ORDER BY i.id",
    )?;
    let candidates = statement
        .query_map(
            params![
                interaction_key,
                owner_subject_id,
                place_id,
                surface_id,
                responder_id
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut matches = Vec::new();
    for (id, components, response) in candidates {
        let components = parse_json_without_duplicate_keys(components.as_bytes())?;
        let response = parse_json_without_duplicate_keys(response.as_bytes())?;
        let response_matches = response.as_object().is_some_and(|object| {
            object.get("surface_id").and_then(serde_json::Value::as_str) == Some(surface_id)
                && object
                    .get("action_name")
                    .and_then(serde_json::Value::as_str)
                    == Some(action_name)
                && object
                    .get("component_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(component_id)
                && object
                    .get("responder_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(responder_id)
        });
        if response_matches && json_contains_component_id(&components, component_id) {
            matches.push(id);
        }
    }
    Ok(matches)
}

fn json_contains_component_id(value: &serde_json::Value, component_id: &str) -> bool {
    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_contains_component_id(value, component_id)),
        serde_json::Value::Object(object) => {
            object.get("id").and_then(serde_json::Value::as_str) == Some(component_id)
                || object
                    .values()
                    .any(|value| json_contains_component_id(value, component_id))
        }
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum SubtaskJoin {
    Cancelled,
    Steer,
}

fn resolve_subtask_places(
    source: &Connection,
    target: &Transaction<'_>,
    agent_id: &str,
    history_session_id: &str,
    history_metadata: &serde_json::Map<String, serde_json::Value>,
    join: SubtaskJoin,
) -> Result<Vec<i64>> {
    if !SourceTable::exists(source, "sessions")? {
        return Ok(Vec::new());
    }
    let Some(subtask_id) = history_metadata
        .get(match join {
            SubtaskJoin::Cancelled => "tool_call_id",
            SubtaskJoin::Steer => "subtask_id",
        })
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    let expected_session_id = format!("subtask-{subtask_id}");
    if matches!(join, SubtaskJoin::Steer) && history_session_id != expected_session_id {
        return Ok(Vec::new());
    }
    let mut statement = source.prepare(
        "SELECT id,mode,participant_ids_json,metadata_json
         FROM sessions WHERE id=?1 ORDER BY rowid",
    )?;
    let rows = statement
        .query_map([&expected_session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut matches = Vec::new();
    for (session_id, mode, participants, metadata) in rows {
        if mode != "subtask" {
            continue;
        }
        let participants = parse_json_without_duplicate_keys(participants.as_bytes())?;
        let participant_matches = participants
            .as_array()
            .is_some_and(|values| values.len() == 1 && values[0].as_str() == Some(agent_id));
        let Some(metadata) = metadata else {
            continue;
        };
        let session_metadata = parse_json_without_duplicate_keys(metadata.as_bytes())?;
        let session_metadata = session_metadata.as_object();
        let metadata_matches = session_metadata.is_some_and(|object| {
            object.get("subtask_id").and_then(serde_json::Value::as_str) == Some(subtask_id)
                && match join {
                    SubtaskJoin::Cancelled => {
                        object
                            .get("parent_session_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(history_session_id)
                    }
                    SubtaskJoin::Steer => {
                        history_metadata
                            .get("from_session")
                            .and_then(serde_json::Value::as_str)
                            == object
                                .get("parent_session_id")
                                .and_then(serde_json::Value::as_str)
                    }
                }
        });
        if !participant_matches || !metadata_matches {
            continue;
        }
        let mut place_statement = target.prepare(
            "SELECT place_id FROM place_source_refs
             WHERE classification='child' AND source_system='opencrab' AND source_address=?1
             ORDER BY place_id",
        )?;
        let places = place_statement
            .query_map([session_id], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        matches.extend(places);
    }
    Ok(matches)
}

fn resolve_task_references(
    source: &Connection,
    agent_id: &str,
    session_id: &str,
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<i64>> {
    if !SourceTable::exists(source, "task_ledger")? {
        return Ok(Vec::new());
    }
    let Some(task_id) = metadata.get("task_id").and_then(serde_json::Value::as_i64) else {
        return Ok(Vec::new());
    };
    if task_id <= 0 {
        return Ok(Vec::new());
    }
    let mut statement = source.prepare(
        "SELECT id FROM task_ledger
         WHERE id=?1 AND agent_id=?2 AND session_id=?3 ORDER BY rowid",
    )?;
    let matches = statement
        .query_map(params![task_id, agent_id, session_id], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(matches)
}

fn inspected_metadata(
    metadata: Option<&[u8]>,
    source_row_id: i64,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let Some(bytes) = metadata else {
        return Ok(serde_json::Map::new());
    };
    let value = parse_json_without_duplicate_keys(bytes).map_err(|_| {
        ConverterError::Accounting(format!(
            "history row {source_row_id} has malformed inspected metadata"
        ))
    })?;
    value.as_object().cloned().ok_or_else(|| {
        ConverterError::Accounting(format!(
            "history row {source_row_id} inspected metadata is not an object"
        ))
    })
}

struct UniqueJsonSeed;

impl<'de> DeserializeSeed<'de> for UniqueJsonSeed {
    type Value = serde_json::Value;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> Visitor<'de> for UniqueJsonVisitor {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("valid JSON without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> std::result::Result<Self::Value, E> {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        UniqueJsonSeed.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(UniqueJsonSeed)? {
            values.push(value);
        }
        Ok(serde_json::Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom(format!("duplicate JSON key {key:?}")));
            }
            values.insert(key, map.next_value_seed(UniqueJsonSeed)?);
        }
        Ok(serde_json::Value::Object(values))
    }
}

pub(crate) fn parse_json_without_duplicate_keys(
    bytes: &[u8],
) -> serde_json::Result<serde_json::Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = UniqueJsonSeed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
fn insert_history_audit(
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    source_row_id: i64,
    audit_kind: &str,
    owner_subject_id: i64,
    place_id: i64,
    caller_identity: &str,
    content: &[u8],
    created_at: i64,
    metadata: Option<&[u8]>,
    reason: &str,
    scope: &str,
) -> Result<()> {
    insert_history_audit_with_activity(
        target,
        snapshot_digest,
        source_row_id,
        audit_kind,
        owner_subject_id,
        place_id,
        None,
        caller_identity,
        content,
        created_at,
        metadata,
        reason,
        scope,
    )
}

#[allow(clippy::too_many_arguments)]
fn insert_history_audit_with_activity(
    target: &Transaction<'_>,
    snapshot_digest: [u8; 32],
    source_row_id: i64,
    audit_kind: &str,
    owner_subject_id: i64,
    place_id: i64,
    activity_id: Option<i64>,
    caller_identity: &str,
    content: &[u8],
    created_at: i64,
    metadata: Option<&[u8]>,
    reason: &str,
    scope: &str,
) -> Result<()> {
    let provenance = serde_json::to_vec(&serde_json::json!({
        "source_db_digest": source::hex(&snapshot_digest),
        "source_row_id": source_row_id,
    }))?;
    target.execute(
        "INSERT INTO legacy_audit_records(
           source_db_digest,source_row_id,audit_kind,owner_subject_id,place_id,activity_id,
           caller_discord_id,caller_identity,content,created_at,metadata,new_value,old_value,
           provenance,reason,scope,source_channel_id
         ) VALUES(?1,?2,?3,?4,?5,?6,NULL,?7,?8,?9,?10,NULL,NULL,?11,?12,?13,NULL)",
        params![
            snapshot_digest.as_slice(),
            source_row_id,
            audit_kind,
            owner_subject_id,
            place_id,
            activity_id,
            caller_identity,
            content,
            created_at,
            metadata,
            provenance,
            reason,
            scope,
        ],
    )?;
    Ok(())
}

fn reconcile_migrated_routes(target: &Transaction<'_>) -> Result<()> {
    let mut statement = target.prepare(
        "SELECT DISTINCT m.subject_id,m.place_id,gi.kind_id
         FROM memberships m
         JOIN gate_bindings b ON b.place_id=m.place_id
         JOIN gate_instances gi ON gi.instance_id=b.instance_id
         ORDER BY m.subject_id,m.place_id,gi.kind_id",
    )?;
    let tuples = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    for (subject_id, place_id, kind) in tuples {
        let subject_policy = target.query_row(
            "SELECT COUNT(*),MAX(whitelisted),MAX(heartbeat_enabled)
             FROM place_subject_policies
             WHERE place_id=?1 AND kind_id=?2 AND subject_id=?3",
            params![place_id, kind, subject_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<bool>>(1)?,
                    row.get::<_, Option<bool>>(2)?,
                ))
            },
        )?;
        let policy = subject_policy;
        let metadata = target.query_row(
            "SELECT binding_metadata_bytes FROM gate_bindings b
             JOIN gate_instances gi ON gi.instance_id=b.instance_id
             WHERE b.place_id=?1 AND gi.kind_id=?2 LIMIT 1",
            params![place_id, kind],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        let address_kind = serde_json::from_slice::<serde_json::Value>(&metadata)
            .ok()
            .and_then(|value| {
                value
                    .get("address_kind")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| {
                if kind == "nostr" {
                    "timeline".into()
                } else {
                    "unknown".into()
                }
            });
        let eligible = match address_kind.as_str() {
            "guild" => policy.0 == 1 && policy.1 == Some(true),
            "dm" | "timeline" => true,
            _ => false,
        };
        if !eligible {
            continue;
        }
        let dedicated = query_route_bindings(target, subject_id, place_id, &kind, true)?;
        let shared = query_route_bindings(target, subject_id, place_id, &kind, false)?;
        let candidates = if !dedicated.is_empty() {
            dedicated
        } else {
            shared
        };
        if candidates.len() > 1 {
            return Err(ConverterError::Accounting(format!(
                "route anchor is ambiguous for subject={subject_id},place={place_id},kind={kind}"
            )));
        }
        let Some(binding_id) = candidates.first() else {
            continue;
        };
        let mut purposes = vec!["inbound", "outbound"];
        if policy.2 == Some(true) {
            purposes.push("timed");
        }
        for purpose in purposes {
            target.execute(
                "INSERT INTO subject_routes(subject_id,place_id,kind_id,purpose,binding_id)
                 VALUES(?1,?2,?3,?4,?5)",
                params![subject_id, place_id, kind, purpose, binding_id],
            )?;
        }
    }
    Ok(())
}

fn query_route_bindings(
    target: &Transaction<'_>,
    subject_id: i64,
    place_id: i64,
    kind: &str,
    dedicated: bool,
) -> Result<Vec<String>> {
    let predicate = if dedicated {
        "gi.owner_subject_id=?1"
    } else {
        "gi.owner_subject_id IS NULL"
    };
    let sql = format!(
        "SELECT b.binding_id FROM gate_bindings b
         JOIN gate_instances gi ON gi.instance_id=b.instance_id
         JOIN gate_instance_revisions r ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
         WHERE {predicate} AND b.place_id=?2 AND gi.kind_id=?3
           AND r.present=1 AND r.enabled=1
         ORDER BY b.binding_id"
    );
    let mut statement = target.prepare(&sql)?;
    let rows = if dedicated {
        statement
            .query_map(params![subject_id, place_id, kind], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        statement
            .query_map(params![subject_id, place_id, kind], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    Ok(rows)
}

pub const IN_PLACE_MIGRATION_ID: &str = "inplace-v1";

/// Additive in-place migration: apply store schema, then fill new tables from old ones.
///
/// Phase S+D run in one transaction. A present `schema_migration_state` marker fails loud.
pub fn migrate_in_place(
    conn: &mut Connection,
    config: impl AsRef<Path>,
    environment: impl AsRef<Path>,
    captured_at: i64,
) -> Result<ConversionReport> {
    migrate_in_place_with(
        conn,
        config.as_ref(),
        environment.as_ref(),
        captured_at,
        &NoMigrationInstances,
    )
}

pub fn migrate_in_place_with(
    conn: &mut Connection,
    config: &Path,
    environment: &Path,
    captured_at: i64,
    instance_assembler: &dyn MigrationInstanceAssembler,
) -> Result<ConversionReport> {
    let already = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migration_state'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    if already > 0 {
        let marked: i64 = conn.query_row(
            "SELECT COUNT(*) FROM schema_migration_state WHERE migration_id=?1",
            [IN_PLACE_MIGRATION_ID],
            |row| row.get(0),
        )?;
        if marked > 0 {
            return Err(ConverterError::AlreadyApplied);
        }
    }

    let input_digest = inplace_input_digest(config, environment, captured_at)?;
    let effective_config_snapshot =
        load_effective_config(input_digest, captured_at, config, environment)?;
    let snapshot_digest = effective_config_snapshot.digest;

    let transaction = conn.transaction()?;
    opencrab_store::apply_schema(&transaction)?;
    create_migration_owned_schema(&transaction)?;

    let agents = load_optional_schema(
        &transaction,
        "agents",
        &[
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
        ],
    )?;
    let model_pricing = load_optional_rows(
        &transaction,
        "model_pricing",
        &[
            "provider",
            "model",
            "input_price_per_1m",
            "output_price_per_1m",
            "context_window",
            "updated_at",
            "max_output_tokens",
        ],
    )?;
    let trusted_users = load_optional_rows(
        &transaction,
        "trusted_users",
        &[
            "id",
            "user_id",
            "agent_id",
            "permission",
            "created_by",
            "created_at",
            "display_name",
            "platform",
        ],
    )?;
    let trusted_co_agents = load_optional_rows(
        &transaction,
        "trusted_co_agents",
        &[
            "id",
            "agent_id",
            "co_agent_id",
            "allowed_actions",
            "created_by",
            "created_at",
        ],
    )?;
    let agents = agents.unwrap_or_else(|| SourceTable::empty("agents"));
    let model_pricing = model_pricing.unwrap_or_else(|| SourceTable::empty("model_pricing"));
    let trusted_users = trusted_users.unwrap_or_else(|| SourceTable::empty("trusted_users"));
    let trusted_co_agents =
        trusted_co_agents.unwrap_or_else(|| SourceTable::empty("trusted_co_agents"));
    let mut raw = RawCollector::new(&transaction);
    let mut report = ConversionReport {
        input_snapshot_digest: source::hex(&snapshot_digest),
        ..ConversionReport::default()
    };
    let provenance = MigrationProvenance::new(snapshot_digest);

    let effective_config = effective_config_snapshot.config;
    let agent_subjects = assemble_agents(
        &transaction,
        &transaction,
        &agents,
        &model_pricing,
        &effective_config,
        &provenance,
        &mut raw,
        &mut report,
    )?;

    assemble_gate_configs(
        &transaction,
        &transaction,
        snapshot_digest,
        captured_at,
        &effective_config,
        &agent_subjects,
        &provenance,
        &mut raw,
        &mut report,
    )?;

    // The coordinator owns this transaction across both phases. Instance assembly runs after the
    // canonical agent family so dedicated/shared rows can resolve owners in this same snapshot.
    instance_assembler.assemble(
        &transaction,
        &MigrationInstanceTarget {
            transaction: &transaction,
        },
    )?;
    let migration_instances = load_migration_instances(&transaction)?;

    assemble_discord_channel_policies(
        &transaction,
        &transaction,
        snapshot_digest,
        captured_at,
        &agent_subjects,
        &provenance,
        &mut raw,
        &mut report,
    )?;

    let principals = assemble_principals(
        &transaction,
        &trusted_users,
        &migration_instances,
        &provenance,
        &mut raw,
        &mut report,
        agent_subjects.len() as i64,
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
        &provenance,
        &mut raw,
        &mut report,
    )?;
    assemble_history_and_events(
        &transaction,
        &transaction,
        snapshot_digest,
        captured_at,
        &agent_subjects,
        &migration_instances,
        &mut raw,
        &mut report,
    )?;
    assemble_phase3(
        &transaction,
        &transaction,
        &agent_subjects,
        &provenance,
        &mut raw,
        &mut report,
    )?;
    raw.write(&transaction)?;
    transaction.execute(
        "INSERT INTO schema_migration_state(migration_id, applied_at, source_row_digest)
         VALUES(?1, ?2, ?3)",
        params![
            IN_PLACE_MIGRATION_ID,
            captured_at,
            snapshot_digest.as_slice()
        ],
    )?;

    for table in [
        "gate_instances",
        "subjects",
        "subject_profiles",
        "subject_runtime_configs",
        "gate_subject_identities",
        "grant_sets",
        "agent_grants",
        "grant_actions",
        "grant_source_provenance",
        "gate_instance_revisions",
        "secret_sets",
        "secret_values",
        "places",
        "place_source_refs",
        "place_default_policies",
        "place_subject_policies",
        "memberships",
        "external_origin_scopes",
        "gate_bindings",
        "subject_routes",
        "events",
        "private_journal",
        "legacy_history_archive",
        "legacy_audit_records",
        "subject_history_sources",
        "interactions",
        "interaction_responses",
        "migration_provenance",
        "legacy_unowned_source_rows",
        "schedule_source_state",
        "webhook_endpoints",
        "model_observations",
        "tasks",
        "schedules",
        "subject_allowed_commands",
        "schema_migration_state",
    ] {
        let count = transaction.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, u64>(0)
        })?;
        report.physical_rows.insert(table.into(), count);
    }
    report.verify()?;
    transaction.commit()?;
    Ok(report)
}

fn load_optional_schema(
    conn: &Connection,
    name: &'static str,
    columns: &[&str],
) -> Result<Option<SourceTable>> {
    if !SourceTable::exists(conn, name)? {
        return Ok(None);
    }
    let table = SourceTable::load_schema(conn, name)?;
    table.require_exact_columns(columns)?;
    Ok(Some(table))
}

fn load_optional_rows(
    conn: &Connection,
    name: &'static str,
    columns: &[&str],
) -> Result<Option<SourceTable>> {
    if !SourceTable::exists(conn, name)? {
        return Ok(None);
    }
    let table = SourceTable::load(conn, name)?;
    table.require_exact_columns(columns)?;
    Ok(Some(table))
}

fn inplace_input_digest(config: &Path, environment: &Path, captured_at: i64) -> Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(b"opencrab-inplace-input-snapshot-v1\0");
    hasher.update(digest_file(config)?);
    hasher.update(digest_file(environment)?);
    hasher.update(captured_at.to_be_bytes());
    Ok(hasher.finalize().into())
}

#[cfg(test)]
fn create_phase1_schema(conn: &Connection) -> Result<()> {
    opencrab_store::apply_schema(conn)?;
    create_migration_owned_schema(conn)
}

#[allow(clippy::too_many_arguments)]
fn assemble_agents(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &SourceTable,
    model_pricing: &SourceTable,
    config: &EffectiveConfig,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<BTreeMap<String, i64>> {
    let mut accounting =
        ClassAccounting::streaming(agents.name, "agent_aggregate", BTreeMap::new());

    let mut subjects = BTreeMap::new();
    let mut next_id = 1_i64;
    let mut profile_rows = 0_u64;
    let mut runtime_rows = 0_u64;
    let mut provenance_rows = 0_u64;
    agents.for_each_row(source, "agent_id COLLATE BINARY,rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(agents, row);
        let parsed = parse_agent_aggregate(agents, row, model_pricing, config);
        let parsed = match parsed {
            Ok(parsed) => {
                let matches = source.query_row(
                    "SELECT COUNT(*) FROM agents WHERE agent_id=?1",
                    [&parsed.agent_id],
                    |result| result.get::<_, i64>(0),
                )?;
                if matches == 1 {
                    parsed
                } else {
                    raw.add(
                        agents,
                        row,
                        "create-subject-public-id-v1:duplicate_public_id",
                    )?;
                    row_accounting.raw();
                    accounting.finish_streamed_row(row_accounting)?;
                    return Ok(());
                }
            }
            Err(reason) => {
                raw.add(agents, row, reason)?;
                row_accounting.raw();
                accounting.finish_streamed_row(row_accounting)?;
                return Ok(());
            }
        };
        let subject_id = next_id;
        next_id = next_id
            .checked_add(1)
            .ok_or_else(|| ConverterError::Accounting("subject id overflow".into()))?;
        target.execute(
            "INSERT INTO subjects(id,kind,name,persona,turn_runner,standing,created_at)
             VALUES(?1,'agent',?2,?3,?4,'trusted',?5)",
            params![
                subject_id,
                parsed.name,
                parsed.persona_name,
                parsed.turn_runner,
                parsed.created_at
            ],
        )?;
        target.execute(
            "INSERT INTO subject_profiles(
               subject_id,revision,persona_name,persona,instructions,
               default_heartbeat_instructions,job_title,organization,image_url,metadata,updated_at
             ) VALUES(?1,1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                subject_id,
                parsed.persona_name,
                parsed.persona,
                parsed.instructions,
                parsed.heartbeat_instructions,
                parsed.job_title,
                parsed.organization,
                parsed.image_url,
                parsed.metadata,
                parsed.updated_at,
            ],
        )?;
        target.execute(
            "INSERT INTO subject_runtime_configs(
               subject_id,revision,created_at,model_alias,reasoning_effort,web_search_enabled,
               history_policy,output_policy,model_route_id,source_config
             ) VALUES(?1,1,?2,?3,?4,?5,?6,?7,NULL,?8)",
            params![
                subject_id,
                parsed.created_at,
                parsed.model_alias,
                parsed.reasoning_effort,
                parsed.web_search_enabled,
                parsed.history_policy,
                parsed.output_policy,
                config.source_config,
            ],
        )?;
        let subject_key = integer_key(subject_id);
        provenance.write(target, "subjects", &subject_key, agents, row)?;
        provenance.write(
            target,
            "subject_profiles",
            &composite_key(&[
                source::SqliteValue::Integer(subject_id),
                source::SqliteValue::Integer(1),
            ]),
            agents,
            row,
        )?;
        provenance.write(
            target,
            "subject_runtime_configs",
            &composite_key(&[
                source::SqliteValue::Integer(subject_id),
                source::SqliteValue::Integer(1),
            ]),
            agents,
            row,
        )?;
        provenance_rows += 3;
        profile_rows += 1;
        runtime_rows += 1;
        subjects.insert(parsed.agent_id, subject_id);
        row_accounting.canonical();
        accounting.finish_streamed_row(row_accounting)?;
        Ok(())
    })?;
    accounting.physical_rows = BTreeMap::from([
        ("subjects".into(), subjects.len() as u64),
        ("subject_profiles".into(), profile_rows),
        ("subject_runtime_configs".into(), runtime_rows),
        ("migration_provenance".into(), provenance_rows),
    ]);
    report.classes.push(accounting);
    Ok(subjects)
}

struct ParsedAgent {
    agent_id: String,
    name: String,
    persona_name: String,
    persona: Option<String>,
    instructions: String,
    heartbeat_instructions: String,
    job_title: Option<String>,
    organization: Option<String>,
    image_url: Option<String>,
    metadata: Option<String>,
    created_at: i64,
    updated_at: i64,
    turn_runner: String,
    model_alias: Option<String>,
    reasoning_effort: Option<String>,
    web_search_enabled: Option<bool>,
    history_policy: String,
    output_policy: String,
}

fn parse_agent_aggregate(
    agents: &SourceTable,
    row: &SourceRow,
    model_pricing: &SourceTable,
    config: &EffectiveConfig,
) -> std::result::Result<ParsedAgent, &'static str> {
    let required = |column| {
        agents
            .text(row, column)
            .map(str::to_owned)
            .ok_or("create-subject-public-id-v1:noncanonical_storage")
    };
    let nullable = |column| {
        agents
            .nullable_text(row, column)
            .map(|value| value.map(str::to_owned))
            .ok_or("create-subject-public-id-v1:noncanonical_storage")
    };
    let agent_id = required("agent_id")?;
    if agent_id.is_empty()
        || agent_id
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    {
        return Err("create-subject-public-id-v1:invalid_public_id");
    }
    let name = required("name")?;
    let persona_name = required("persona_name")?;
    let persona = nullable("personality")?;
    let instructions = required("instructions")?;
    let heartbeat_instructions = required("heartbeat_instructions")?;
    let metadata = nullable("metadata_json")?;
    if let Some(metadata) = &metadata {
        serde_json::from_str::<serde_json::Value>(metadata)
            .map_err(|_| "create-subject-public-id-v1:invalid_metadata")?;
    }
    let created_at = agents
        .text(row, "created_at")
        .and_then(parse_utc_nanos)
        .ok_or("create-subject-public-id-v1:invalid_created_at")?;
    let updated_at = agents
        .text(row, "updated_at")
        .and_then(parse_utc_nanos)
        .ok_or("create-subject-public-id-v1:invalid_updated_at")?;
    let model_alias = nullable("model")?;
    let effective_model = model_alias
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(config.default_model.as_deref())
        .ok_or("create-subject-public-id-v1:missing_effective_model")?;
    let (provider, model) = effective_model
        .split_once(':')
        .map_or(("", effective_model), |(provider, model)| (provider, model));
    let provider = provider.trim();
    let model = model.trim();
    let matches = model_pricing
        .rows
        .iter()
        .filter(|pricing| {
            model_pricing.text(pricing, "provider") == Some(provider)
                && model_pricing.text(pricing, "model") == Some(model)
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err("create-subject-public-id-v1:effective_model_pricing_not_exact_one");
    }
    let pricing = matches[0];
    let context_window = model_pricing
        .integer(pricing, "context_window")
        .filter(|value| *value > 0)
        .ok_or("create-subject-public-id-v1:invalid_context_window")?;
    let budget = (context_window as f64 * config.compaction_ratio).floor();
    if !budget.is_finite() || budget < 0.0 || budget > i64::MAX as f64 {
        return Err("create-subject-public-id-v1:invalid_history_budget");
    }
    let mut budget = budget as i64;
    if provider == "chatgpt" {
        budget = budget.min(305_000);
    }
    let max_output = model_pricing
        .nullable_integer(pricing, "max_output_tokens")
        .ok_or("create-subject-public-id-v1:invalid_max_output_tokens")?;
    let history_policy = serde_json::to_string(&serde_json::json!({"budget_tokens": budget}))
        .map_err(|_| "create-subject-public-id-v1:history_policy_encoding")?;
    let output_policy =
        serde_json::to_string(&serde_json::json!({"max_output_tokens": max_output}))
            .map_err(|_| "create-subject-public-id-v1:output_policy_encoding")?;
    let web_search_enabled = agents
        .nullable_integer(row, "web_search")
        .ok_or("create-subject-public-id-v1:noncanonical_storage")?
        .map(|value| value != 0);
    Ok(ParsedAgent {
        agent_id,
        name,
        persona_name,
        persona,
        instructions,
        heartbeat_instructions,
        job_title: nullable("job_title")?,
        organization: nullable("organization")?,
        image_url: nullable("image_url")?,
        metadata,
        created_at,
        updated_at,
        turn_runner: effective_model.to_owned(),
        model_alias,
        reasoning_effort: nullable("reasoning_effort")?,
        web_search_enabled,
        history_policy,
        output_policy,
    })
}

fn assemble_principals(
    target: &Transaction<'_>,
    trusted_users: &SourceTable,
    instances: &MigrationInstanceSet,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
    subject_id_offset: i64,
) -> Result<Vec<Principal>> {
    let mut groups = BTreeMap::<(String, String), Vec<usize>>::new();
    let mut accounting = ClassAccounting::new(
        trusted_users.name,
        "external_principal",
        trusted_users
            .rows
            .iter()
            .map(|row| ContributionKey::new(trusted_users, row)),
        BTreeMap::new(),
    );
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
            accounting.raw(ContributionKey::new(trusted_users, row));
            continue;
        };
        if !matches!(platform, "discord" | "nostr" | "web" | "rest") {
            raw.add(
                trusted_users,
                row,
                "resolve-external-principal-v1:unknown_platform",
            )?;
            accounting.raw(ContributionKey::new(trusted_users, row));
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
            .0
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
        let reason = if external_id.is_empty() {
            Some("resolve-external-principal-v1:invalid_external_id")
        } else if matching_instances.is_empty() {
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
                accounting.raw(ContributionKey::new(
                    trusted_users,
                    &trusted_users.rows[index],
                ));
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
    let mut identity_rows = 0_u64;
    let mut provenance_rows = 0_u64;
    for (index, principal) in candidates.iter_mut().enumerate() {
        principal.subject_id = subject_id_offset
            .checked_add(
                i64::try_from(index + 1)
                    .map_err(|_| ConverterError::Accounting("subject id overflow".into()))?,
            )
            .ok_or_else(|| ConverterError::Accounting("subject id overflow".into()))?;
        // turn_runner='engine' is what oc2 `create_subject` writes (DESIGN-DB-MIGRATION §12.8.4).
        target.execute(
            "INSERT INTO subjects(id,kind,name,persona,turn_runner,standing,created_at)
             VALUES(?1,'human',?2,?2,'engine','unknown',?3)",
            params![
                principal.subject_id,
                principal.display_name,
                principal.created_at
            ],
        )?;
        let subject_key = integer_key(principal.subject_id);
        for contributor in &principal.contributors {
            let row = &trusted_users.rows[*contributor];
            provenance.write(target, "subjects", &subject_key, trusted_users, row)?;
            provenance_rows += 1;
        }
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
            let identity_key = composite_key(&[text(instance), text(&principal.external_id)]);
            for contributor in &principal.contributors {
                let row = &trusted_users.rows[*contributor];
                provenance.write(
                    target,
                    "gate_subject_identities",
                    &identity_key,
                    trusted_users,
                    row,
                )?;
                provenance_rows += 1;
            }
        }
        for contributor in &principal.contributors {
            accounting.canonical(ContributionKey::new(
                trusted_users,
                &trusted_users.rows[*contributor],
            ));
        }
    }
    accounting.physical_rows = BTreeMap::from([
        ("subjects".into(), candidates.len() as u64),
        ("gate_subject_identities".into(), identity_rows),
        ("migration_provenance".into(), provenance_rows),
    ]);
    report.classes.push(accounting);
    Ok(candidates)
}

#[allow(clippy::too_many_arguments)]
fn assemble_grants(
    target: &Transaction<'_>,
    trusted_users: &SourceTable,
    trusted_co_agents: &SourceTable,
    agent_subjects: &BTreeMap<String, i64>,
    principal_subjects: &BTreeMap<(String, String), i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector,
    report: &mut ConversionReport,
) -> Result<()> {
    let mut contributors = Vec::<GrantContributor>::new();
    let mut accounting = ClassAccounting::new(
        "trusted_users+trusted_co_agents",
        "grant_contributor",
        trusted_users
            .rows
            .iter()
            .map(|row| ContributionKey::new(trusted_users, row))
            .chain(
                trusted_co_agents
                    .rows
                    .iter()
                    .map(|row| ContributionKey::new(trusted_co_agents, row)),
            ),
        BTreeMap::new(),
    );
    for row in &trusted_users.rows {
        match parse_user_grant(trusted_users, row, agent_subjects, principal_subjects) {
            Ok(contributor) => contributors.push(contributor),
            Err(reason) => {
                raw.add(trusted_users, row, reason)?;
                accounting.raw(ContributionKey::new(trusted_users, row));
            }
        }
    }
    for row in &trusted_co_agents.rows {
        match parse_co_agent_grant(trusted_co_agents, row, agent_subjects) {
            Ok(contributor) => contributors.push(contributor),
            Err(reason) => {
                raw.add(trusted_co_agents, row, reason)?;
                accounting.raw(ContributionKey::new(trusted_co_agents, row));
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
    let mut common_provenance_rows = 0_u64;
    for (agent, group) in groups {
        let created_at = group[0].created_at;
        target.execute(
            "INSERT INTO grant_sets(agent_subject_id,revision,created_at) VALUES(?1,1,?2)",
            params![agent, created_at],
        )?;
        let grant_set_key = composite_key(&[
            source::SqliteValue::Integer(agent),
            source::SqliteValue::Integer(1),
        ]);
        for contributor in &group {
            provenance.write_parts(
                target,
                "grant_sets",
                &grant_set_key,
                grant_source_table(contributor.source),
                &contributor.source_key,
                &contributor.row_digest,
            )?;
            common_provenance_rows += 1;
        }
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
            let provenance_key = composite_key(&[
                source::SqliteValue::Integer(contributor.agent_subject_id),
                source::SqliteValue::Integer(contributor.principal_subject_id),
                text(&contributor.gate_kind),
                text(&contributor.external_id),
                text(&contributor.permission),
                nullable_text(contributor.allowed_actions.as_deref()),
                nullable_text(contributor.source_record_key.as_deref()),
                text(&contributor.created_by),
                source::SqliteValue::Integer(contributor.created_at),
                source::SqliteValue::Blob(contributor.source_key.clone()),
            ]);
            provenance.write_parts(
                target,
                "grant_source_provenance",
                &provenance_key,
                grant_source_table(contributor.source),
                &contributor.source_key,
                &contributor.row_digest,
            )?;
            common_provenance_rows += 1;
        }
        for (principal, role) in selected_roles {
            target.execute(
                "INSERT INTO agent_grants(
                   grant_set_revision,grant_set_subject_id,principal_subject_id,role,scope
                 ) VALUES(1,?1,?2,?3,'agent')",
                params![agent, principal, role],
            )?;
            grant_rows += 1;
            let grant_key = composite_key(&[
                source::SqliteValue::Integer(agent),
                source::SqliteValue::Integer(1),
                source::SqliteValue::Integer(principal),
                text(role),
                text("agent"),
            ]);
            for contributor in group
                .iter()
                .filter(|contributor| contributor.principal_subject_id == principal)
            {
                provenance.write_parts(
                    target,
                    "agent_grants",
                    &grant_key,
                    grant_source_table(contributor.source),
                    &contributor.source_key,
                    &contributor.row_digest,
                )?;
                common_provenance_rows += 1;
            }
        }
        for (principal, action) in actions {
            target.execute(
                "INSERT INTO grant_actions(
                   grant_set_revision,grant_set_subject_id,principal_subject_id,action
                 ) VALUES(1,?1,?2,?3)",
                params![agent, principal, action],
            )?;
            action_rows += 1;
            let action_key = composite_key(&[
                source::SqliteValue::Integer(agent),
                source::SqliteValue::Integer(1),
                source::SqliteValue::Integer(principal),
                text(&action),
            ]);
            for contributor in group.iter().filter(|contributor| {
                contributor.principal_subject_id == principal
                    && contributor.allowed_actions.as_deref() == Some(action.as_str())
            }) {
                provenance.write_parts(
                    target,
                    "grant_actions",
                    &action_key,
                    grant_source_table(contributor.source),
                    &contributor.source_key,
                    &contributor.row_digest,
                )?;
                common_provenance_rows += 1;
            }
        }
        for contributor in &group {
            accounting.canonical(contributor.contribution.clone());
        }
    }

    accounting.physical_rows = BTreeMap::from([
        ("grant_sets".into(), report_group_count(target)?),
        ("agent_grants".into(), grant_rows),
        ("grant_actions".into(), action_rows),
        ("grant_source_provenance".into(), provenance_rows),
        ("migration_provenance".into(), common_provenance_rows),
    ]);
    report.classes.push(accounting);
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
        gate_kind: platform.into(),
        permission: permission.into(),
        allowed_actions: None,
        source_record_key,
        created_by: created_by.into(),
        created_at,
        source_key: row.source_key.clone(),
        row_digest: row.row_digest,
        contribution: ContributionKey::new(table, row),
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
        gate_kind: "all".into(),
        permission: "co-agent".into(),
        allowed_actions,
        source_record_key,
        created_by: created_by.into(),
        created_at,
        source_key: row.source_key.clone(),
        row_digest: row.row_digest,
        contribution: ContributionKey::new(table, row),
    })
}

fn grant_source_rank(source: GrantSource) -> u8 {
    match source {
        GrantSource::User => 0,
        GrantSource::CoAgent => 1,
    }
}

fn grant_source_table(source: GrantSource) -> &'static str {
    match source {
        GrantSource::User => "trusted_users",
        GrantSource::CoAgent => "trusted_co_agents",
    }
}

fn nullable_text(value: Option<&str>) -> source::SqliteValue {
    value.map(text).unwrap_or(source::SqliteValue::Null)
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

fn load_migration_instances(target: &Transaction<'_>) -> Result<MigrationInstanceSet> {
    let mut statement = target.prepare("SELECT instance_id,kind_id FROM gate_instances")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut instances = rows
        .into_iter()
        .map(|(instance_id, kind_id)| MigrationInstance::new(instance_id, kind_id))
        .collect::<Result<Vec<_>>>()?;
    validate_instances(&instances)?;
    instances.sort_by_key(|instance| instance.uuid_bytes);
    Ok(MigrationInstanceSet(instances))
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
