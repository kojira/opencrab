use crate::provenance::{integer_key, MigrationProvenance};
use crate::report::{ClassAccounting, ConversionReport};
use crate::source::{SourceRow, SourceTable, SqliteValue};
use crate::time::parse_utc_nanos;
use crate::{next_integer_id, RawCollector, Result};
use rusqlite::{params, Connection, Transaction};
use std::collections::BTreeMap;

pub(crate) fn assemble_phase3(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector<'_, '_>,
    report: &mut ConversionReport,
) -> Result<()> {
    assemble_heartbeat_source_state(source, target, agents, provenance, raw, report)?;
    assemble_webhooks(source, target, agents, provenance, raw, report)?;
    assemble_model_observations(source, target, agents, provenance, raw, report)?;
    assemble_tasks(source, target, agents, provenance, raw, report)?;
    assemble_cron_schedules(source, target, agents, provenance, raw, report)?;
    assemble_allowed_commands(source, target, agents, provenance, raw, report)?;
    assemble_curated_memories(source, target, agents, provenance, raw, report)?;
    Ok(())
}

fn assemble_heartbeat_source_state(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector<'_, '_>,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "agent_heartbeat_config")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "agent_heartbeat_config")?;
    table.require_exact_columns(&["agent_id", "enabled", "interval_secs", "updated_at"])?;
    let mut accounting =
        ClassAccounting::streaming(table.name, "heartbeat_source_state", BTreeMap::new());
    let mut rows = 0_u64;
    table.for_each_row(source, "rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        match parse_heartbeat_source(&table, row, agents) {
            Ok(parsed) => {
                target.execute(
                    "INSERT INTO schedule_source_state(
                       owner_subject_id,enabled,raw_interval_secs,source_updated_at
                     ) VALUES(?1,?2,?3,?4)",
                    params![
                        parsed.owner_subject_id,
                        parsed.enabled,
                        parsed.raw_interval_secs,
                        parsed.source_updated_at,
                    ],
                )?;
                provenance.write(
                    target,
                    "schedule_source_state",
                    &integer_key(parsed.owner_subject_id),
                    &table,
                    row,
                )?;
                rows += 1;
                row_accounting.canonical();
            }
            Err(reason) => {
                raw.add(&table, row, reason)?;
                row_accounting.raw();
            }
        }
        accounting.finish_streamed_row(row_accounting)
    })?;
    accounting.physical_rows = BTreeMap::from([("schedule_source_state".into(), rows)]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedHeartbeatSource {
    owner_subject_id: i64,
    enabled: i64,
    raw_interval_secs: Option<i64>,
    source_updated_at: i64,
}

fn parse_heartbeat_source(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
) -> std::result::Result<ParsedHeartbeatSource, &'static str> {
    Ok(ParsedHeartbeatSource {
        owner_subject_id: required_subject(table, row, "agent_id", agents)?,
        enabled: required_bool(table, row, "enabled")?,
        raw_interval_secs: nullable_integer(table, row, "interval_secs")?,
        source_updated_at: required_time(table, row, "updated_at")?,
    })
}

fn assemble_webhooks(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector<'_, '_>,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "agent_webhook_config")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "agent_webhook_config")?;
    table.require_exact_columns(&[
        "scope",
        "agent_id",
        "tool_name",
        "kind",
        "url",
        "events_json",
        "enabled",
        "name",
        "created_by",
        "output_mode",
        "max_chars",
        "updated_at",
    ])?;
    let mut accounting =
        ClassAccounting::streaming(table.name, "webhook_endpoint", BTreeMap::new());
    let mut rows = 0_u64;
    table.for_each_row(source, "rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        match parse_webhook(&table, row, agents) {
            Ok(parsed) => {
                let id = next_integer_id(target, "webhook_endpoints")?;
                target.execute(
                    "INSERT INTO webhook_endpoints(
                       id,created_by,enabled,endpoint,event_filter,kind,maximum_output_chars,
                       name,output_mode,owner_subject_id,scope,tool_name,updated_at
                     ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![
                        id,
                        parsed.created_by,
                        parsed.enabled,
                        parsed.endpoint,
                        parsed.event_filter,
                        parsed.kind,
                        parsed.maximum_output_chars,
                        parsed.name,
                        parsed.output_mode,
                        parsed.owner_subject_id,
                        parsed.scope,
                        parsed.tool_name,
                        parsed.updated_at,
                    ],
                )?;
                provenance.write(target, "webhook_endpoints", &integer_key(id), &table, row)?;
                rows += 1;
                row_accounting.canonical();
            }
            Err(reason) => {
                raw.add(&table, row, reason)?;
                row_accounting.raw();
            }
        }
        accounting.finish_streamed_row(row_accounting)
    })?;
    accounting.physical_rows = BTreeMap::from([("webhook_endpoints".into(), rows)]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedWebhook {
    created_by: Option<String>,
    enabled: i64,
    endpoint: String,
    event_filter: Option<String>,
    kind: String,
    maximum_output_chars: i64,
    name: Option<String>,
    output_mode: String,
    owner_subject_id: i64,
    scope: String,
    tool_name: String,
    updated_at: i64,
}

fn parse_webhook(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
) -> std::result::Result<ParsedWebhook, &'static str> {
    Ok(ParsedWebhook {
        created_by: nullable_text(table, row, "created_by")?,
        enabled: required_bool(table, row, "enabled")?,
        endpoint: required_text(table, row, "url")?,
        event_filter: nullable_json(table, row, "events_json")?,
        kind: required_text(table, row, "kind")?,
        maximum_output_chars: required_integer(table, row, "max_chars")?,
        name: nullable_text(table, row, "name")?,
        output_mode: required_text(table, row, "output_mode")?,
        owner_subject_id: required_subject(table, row, "agent_id", agents)?,
        scope: required_text(table, row, "scope")?,
        tool_name: required_text(table, row, "tool_name")?,
        updated_at: required_time(table, row, "updated_at")?,
    })
}

fn assemble_model_observations(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector<'_, '_>,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "model_experience_notes")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "model_experience_notes")?;
    table.require_exact_columns(&[
        "id",
        "agent_id",
        "provider",
        "model",
        "situation",
        "observation",
        "recommendation",
        "tags",
        "created_at",
    ])?;
    let mut accounting =
        ClassAccounting::streaming(table.name, "model_observation", BTreeMap::new());
    let mut rows = 0_u64;
    table.for_each_row(source, "rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        match parse_model_observation(&table, row, agents) {
            Ok(parsed) => {
                let id = next_integer_id(target, "model_observations")?;
                target.execute(
                    "INSERT INTO model_observations(
                       id,created_at,model,model_id,observation,owner_subject_id,provider,
                       provider_id,recommendation,situation,source_record_key,tags_json
                     ) VALUES(?1,?2,?3,NULL,?4,?5,?6,NULL,?7,?8,?9,?10)",
                    params![
                        id,
                        parsed.created_at,
                        parsed.model,
                        parsed.observation,
                        parsed.owner_subject_id,
                        parsed.provider,
                        parsed.recommendation,
                        parsed.situation,
                        parsed.source_record_key,
                        parsed.tags_json,
                    ],
                )?;
                provenance.write(target, "model_observations", &integer_key(id), &table, row)?;
                rows += 1;
                row_accounting.canonical();
            }
            Err(reason) => {
                raw.add(&table, row, reason)?;
                row_accounting.raw();
            }
        }
        accounting.finish_streamed_row(row_accounting)
    })?;
    accounting.physical_rows = BTreeMap::from([("model_observations".into(), rows)]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedModelObservation {
    created_at: i64,
    model: Option<String>,
    observation: String,
    owner_subject_id: i64,
    provider: Option<String>,
    recommendation: Option<String>,
    situation: String,
    source_record_key: Option<String>,
    tags_json: Option<String>,
}

fn parse_model_observation(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
) -> std::result::Result<ParsedModelObservation, &'static str> {
    Ok(ParsedModelObservation {
        created_at: required_time(table, row, "created_at")?,
        model: nullable_text(table, row, "model")?,
        observation: required_text(table, row, "observation")?,
        owner_subject_id: required_subject(table, row, "agent_id", agents)?,
        provider: nullable_text(table, row, "provider")?,
        recommendation: nullable_text(table, row, "recommendation")?,
        situation: required_text(table, row, "situation")?,
        source_record_key: nullable_text(table, row, "id")?,
        tags_json: nullable_json(table, row, "tags")?,
    })
}

fn assemble_tasks(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector<'_, '_>,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "task_ledger")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "task_ledger")?;
    table.require_exact_columns(&[
        "id",
        "agent_id",
        "session_id",
        "goal",
        "contract",
        "status",
        "created_at",
        "updated_at",
        "restart_count",
    ])?;
    let mut accounting = ClassAccounting::streaming(table.name, "task", BTreeMap::new());
    let mut rows = 0_u64;
    table.for_each_row(source, "rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        match parse_task(&table, row, agents, target) {
            Ok(parsed) => {
                let id = next_integer_id(target, "tasks")?;
                target.execute(
                    "INSERT INTO tasks(
                       id,contract,created_at,goal,owner_subject_id,place_id,restart_count,
                       source_record_key,state,updated_at
                     ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![
                        id,
                        parsed.contract,
                        parsed.created_at,
                        parsed.goal,
                        parsed.owner_subject_id,
                        parsed.place_id,
                        parsed.restart_count,
                        parsed.source_record_key,
                        parsed.state,
                        parsed.updated_at,
                    ],
                )?;
                provenance.write(target, "tasks", &integer_key(id), &table, row)?;
                rows += 1;
                row_accounting.canonical();
            }
            Err(reason) => {
                raw.add(&table, row, reason)?;
                row_accounting.raw();
            }
        }
        accounting.finish_streamed_row(row_accounting)
    })?;
    accounting.physical_rows = BTreeMap::from([("tasks".into(), rows)]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedTask {
    contract: Option<String>,
    created_at: i64,
    goal: String,
    owner_subject_id: i64,
    place_id: i64,
    restart_count: i64,
    source_record_key: Option<i64>,
    state: String,
    updated_at: i64,
}

fn parse_task(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
    target: &Transaction<'_>,
) -> std::result::Result<ParsedTask, &'static str> {
    Ok(ParsedTask {
        contract: nullable_text(table, row, "contract")?,
        created_at: required_time(table, row, "created_at")?,
        goal: required_text(table, row, "goal")?,
        owner_subject_id: required_subject(table, row, "agent_id", agents)?,
        place_id: required_place(table, row, "session_id", target)?,
        restart_count: required_integer(table, row, "restart_count")?,
        source_record_key: nullable_integer(table, row, "id")?,
        state: required_text(table, row, "status")?,
        updated_at: required_time(table, row, "updated_at")?,
    })
}

fn assemble_cron_schedules(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector<'_, '_>,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "agent_schedules")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "agent_schedules")?;
    table.require_exact_columns(&[
        "id",
        "agent_id",
        "session_id",
        "cron_expr",
        "timezone",
        "message",
        "enabled",
        "anchor_at",
        "last_fired_at",
        "created_at",
        "updated_at",
    ])?;
    let mut accounting = ClassAccounting::streaming(table.name, "cron_schedule", BTreeMap::new());
    let mut rows = 0_u64;
    table.for_each_row(source, "id", |row| {
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        match parse_cron_schedule(&table, row, agents, target) {
            Ok(parsed) => {
                let id = next_integer_id(target, "schedules")?;
                target.execute(
                    "INSERT INTO schedules(
                       id,owner_subject_id,place_id,kind,expression,timezone,interval_secs,
                       anchor_at,enabled,instruction,instruction_revision,next_fire,
                       last_fired_at,source_record_key,created_at,updated_at
                     ) VALUES(?1,?2,?3,'cron',?4,?5,NULL,?6,?7,?8,1,NULL,?9,?10,?11,?12)",
                    params![
                        id,
                        parsed.owner_subject_id,
                        parsed.place_id,
                        parsed.expression,
                        parsed.timezone,
                        parsed.anchor_at,
                        parsed.enabled,
                        parsed.instruction,
                        parsed.last_fired_at,
                        parsed.source_record_key,
                        parsed.created_at,
                        parsed.updated_at,
                    ],
                )?;
                provenance.write(target, "schedules", &integer_key(id), &table, row)?;
                rows += 1;
                row_accounting.canonical();
            }
            Err(reason) => {
                raw.add(&table, row, reason)?;
                row_accounting.raw();
            }
        }
        accounting.finish_streamed_row(row_accounting)
    })?;
    accounting.physical_rows = BTreeMap::from([("schedules".into(), rows)]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedCronSchedule {
    owner_subject_id: i64,
    place_id: i64,
    expression: String,
    timezone: String,
    anchor_at: Option<i64>,
    enabled: i64,
    instruction: String,
    last_fired_at: Option<i64>,
    source_record_key: i64,
    created_at: i64,
    updated_at: i64,
}

fn parse_cron_schedule(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
    target: &Transaction<'_>,
) -> std::result::Result<ParsedCronSchedule, &'static str> {
    let source_record_key = table
        .integer(row, "id")
        .ok_or("assemble-cron-schedule-v1:null_or_non_integer_id")?;
    Ok(ParsedCronSchedule {
        owner_subject_id: required_subject(table, row, "agent_id", agents)?,
        place_id: required_place(table, row, "session_id", target)?,
        expression: required_text(table, row, "cron_expr")?,
        timezone: required_text(table, row, "timezone")?,
        anchor_at: nullable_time(table, row, "anchor_at")?,
        enabled: required_bool(table, row, "enabled")?,
        instruction: required_text(table, row, "message")?,
        last_fired_at: nullable_time(table, row, "last_fired_at")?,
        source_record_key,
        created_at: required_time(table, row, "created_at")?,
        updated_at: required_time(table, row, "updated_at")?,
    })
}

fn assemble_allowed_commands(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector<'_, '_>,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "agent_allowed_commands")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "agent_allowed_commands")?;
    table.require_exact_columns(&["id", "agent_id", "command", "added_by", "added_at"])?;
    let mut accounting = ClassAccounting::streaming(table.name, "allowed_command", BTreeMap::new());
    let mut rows = 0_u64;
    table.for_each_row(source, "rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        match parse_allowed_command(&table, row, agents) {
            Ok(parsed) => {
                target.execute(
                    "INSERT INTO subject_allowed_commands(subject_id,command) VALUES(?1,?2)",
                    params![parsed.subject_id, parsed.command],
                )?;
                provenance.write(
                    target,
                    "subject_allowed_commands",
                    &composite_allowed_command_key(parsed.subject_id, &parsed.command),
                    &table,
                    row,
                )?;
                rows += 1;
                row_accounting.canonical();
            }
            Err(reason) => {
                raw.add(&table, row, reason)?;
                row_accounting.raw();
            }
        }
        accounting.finish_streamed_row(row_accounting)
    })?;
    accounting.physical_rows = BTreeMap::from([("subject_allowed_commands".into(), rows)]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedAllowedCommand {
    subject_id: i64,
    command: String,
}

fn parse_allowed_command(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
) -> std::result::Result<ParsedAllowedCommand, &'static str> {
    Ok(ParsedAllowedCommand {
        subject_id: required_subject(table, row, "agent_id", agents)?,
        command: required_text(table, row, "command")?,
    })
}

fn composite_allowed_command_key(subject_id: i64, command: &str) -> Vec<u8> {
    let mut key = integer_key(subject_id);
    key.push(0);
    key.extend_from_slice(command.as_bytes());
    key
}

fn assemble_curated_memories(
    source: &Connection,
    target: &Transaction<'_>,
    agents: &BTreeMap<String, i64>,
    provenance: &MigrationProvenance,
    raw: &mut RawCollector<'_, '_>,
    report: &mut ConversionReport,
) -> Result<()> {
    if !SourceTable::exists(source, "memory_curated")? {
        return Ok(());
    }
    let table = SourceTable::load_schema(source, "memory_curated")?;
    table.require_exact_columns(&[
        "id",
        "agent_id",
        "category",
        "content",
        "updated_at",
        "created_at",
    ])?;
    let mut accounting = ClassAccounting::streaming(table.name, "curated_memory", BTreeMap::new());
    let mut rows = 0_u64;
    table.for_each_row(source, "id COLLATE BINARY,rowid", |row| {
        let mut row_accounting = accounting.start_streamed_row(&table, row);
        match parse_curated_memory(&table, row, agents) {
            Ok(parsed) => {
                let id = next_integer_id(target, "memories")?;
                target.execute(
                    "INSERT INTO memories(
                       id,subject_id,body,origin_place,origin_from_seq,origin_to_seq,
                       written_at,last_read_at
                     ) VALUES(?1,?2,?3,NULL,NULL,NULL,?4,NULL)",
                    params![id, parsed.subject_id, parsed.body, parsed.written_at],
                )?;
                provenance.write(target, "memories", &integer_key(id), &table, row)?;
                rows += 1;
                row_accounting.canonical();
            }
            Err(reason) => {
                raw.add(&table, row, reason)?;
                row_accounting.raw();
            }
        }
        accounting.finish_streamed_row(row_accounting)
    })?;
    accounting.physical_rows = BTreeMap::from([("memories".into(), rows)]);
    report.classes.push(accounting);
    Ok(())
}

struct ParsedCuratedMemory {
    subject_id: i64,
    body: String,
    written_at: Option<i64>,
}

fn parse_curated_memory(
    table: &SourceTable,
    row: &SourceRow,
    agents: &BTreeMap<String, i64>,
) -> std::result::Result<ParsedCuratedMemory, &'static str> {
    let _id = required_text(table, row, "id")?;
    Ok(ParsedCuratedMemory {
        subject_id: required_subject(table, row, "agent_id", agents)?,
        body: required_text(table, row, "content")?,
        written_at: curated_memory_written_at(table, row)?,
    })
}

/// DESIGN-782: byte-exact empty `created_at` is unrecorded (SQL NULL).
/// Non-empty parse success is nanos. Non-empty parse failure stays raw.
/// updated_at / now / captured-at / 0 are not substitutes.
fn curated_memory_written_at(
    table: &SourceTable,
    row: &SourceRow,
) -> std::result::Result<Option<i64>, &'static str> {
    match table.value(row, "created_at") {
        Some(SqliteValue::Text(value)) if value.is_empty() => Ok(None),
        Some(SqliteValue::Text(value)) => {
            let text = std::str::from_utf8(value).map_err(|_| "parse-utc-nanos-v1:invalid_utf8")?;
            parse_utc_nanos(text)
                .map(Some)
                .ok_or("parse-utc-nanos-v1:invalid_timestamp")
        }
        Some(_) => Err("parse-utc-nanos-v1:noncanonical_storage"),
        None => Err("parse-utc-nanos-v1:missing_column"),
    }
}

fn nullable_time(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
) -> std::result::Result<Option<i64>, &'static str> {
    match table.value(row, column) {
        Some(SqliteValue::Null) => Ok(None),
        Some(SqliteValue::Text(value)) => {
            let text = std::str::from_utf8(value).map_err(|_| "parse-utc-nanos-v1:invalid_utf8")?;
            parse_utc_nanos(text)
                .map(Some)
                .ok_or("parse-utc-nanos-v1:invalid_timestamp")
        }
        Some(_) => Err("parse-utc-nanos-v1:noncanonical_storage"),
        None => Err("parse-utc-nanos-v1:missing_column"),
    }
}

fn optional_place(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
    target: &Transaction<'_>,
) -> std::result::Result<Option<i64>, &'static str> {
    match table.value(row, column) {
        Some(SqliteValue::Null) => Ok(None),
        Some(SqliteValue::Text(value) | SqliteValue::Blob(value)) => lookup_place(target, value),
        Some(_) => Err("resolve-place-source-id-v1:noncanonical_storage"),
        None => Err("resolve-place-source-id-v1:missing_column"),
    }
}

fn required_place(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
    target: &Transaction<'_>,
) -> std::result::Result<i64, &'static str> {
    optional_place(table, row, column, target)?.ok_or("resolve-place-source-id-v1:unresolved_place")
}

fn lookup_place(
    target: &Transaction<'_>,
    source_id: &[u8],
) -> std::result::Result<Option<i64>, &'static str> {
    let mut statement = target
        .prepare("SELECT place_id FROM place_source_refs WHERE source_id=?1")
        .map_err(|_| "resolve-place-source-id-v1:lookup_failed")?;
    let matches = statement
        .query_map(params![source_id], |row| row.get::<_, i64>(0))
        .and_then(|rows| rows.collect::<std::result::Result<Vec<_>, _>>())
        .map_err(|_| "resolve-place-source-id-v1:lookup_failed")?;
    match matches.as_slice() {
        [] => Ok(None),
        [place_id] => Ok(Some(*place_id)),
        _ => Err("resolve-place-source-id-v1:duplicate_source_id"),
    }
}

fn required_subject(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
    agents: &BTreeMap<String, i64>,
) -> std::result::Result<i64, &'static str> {
    let agent_id = required_text(table, row, column)?;
    agents
        .get(&agent_id)
        .copied()
        .ok_or("resolve-subject-public-id-v1:unresolved_agent")
}

fn required_text(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
) -> std::result::Result<String, &'static str> {
    table
        .text(row, column)
        .map(str::to_owned)
        .ok_or("copy-bytes-v1:noncanonical_storage")
}

fn nullable_text(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
) -> std::result::Result<Option<String>, &'static str> {
    table
        .nullable_text(row, column)
        .map(|value| value.map(str::to_owned))
        .ok_or("copy-bytes-v1:noncanonical_storage")
}

fn required_integer(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
) -> std::result::Result<i64, &'static str> {
    table
        .integer(row, column)
        .ok_or("copy-bytes-v1:noncanonical_storage")
}

fn nullable_integer(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
) -> std::result::Result<Option<i64>, &'static str> {
    table
        .nullable_integer(row, column)
        .ok_or("copy-bytes-v1:noncanonical_storage")
}

fn required_bool(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
) -> std::result::Result<i64, &'static str> {
    match table.value(row, column) {
        Some(SqliteValue::Integer(value)) => Ok(i64::from(*value != 0)),
        Some(SqliteValue::Null) => Err("sqlite-bool-v1:null_required"),
        Some(_) => Err("sqlite-bool-v1:noncanonical_storage"),
        None => Err("sqlite-bool-v1:missing_column"),
    }
}

fn required_time(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
) -> std::result::Result<i64, &'static str> {
    table
        .text(row, column)
        .and_then(parse_utc_nanos)
        .ok_or("parse-utc-nanos-v1:invalid_timestamp")
}

fn nullable_json(
    table: &SourceTable,
    row: &SourceRow,
    column: &str,
) -> std::result::Result<Option<String>, &'static str> {
    match table.value(row, column) {
        Some(SqliteValue::Null) => Ok(None),
        Some(SqliteValue::Text(value)) => {
            crate::parse_json_without_duplicate_keys(value)
                .map_err(|_| "json-rfc8259-v1:invalid_json")?;
            let text = std::str::from_utf8(value).map_err(|_| "json-rfc8259-v1:invalid_utf8")?;
            Ok(Some(text.to_owned()))
        }
        Some(_) => Err("json-rfc8259-v1:noncanonical_storage"),
        None => Err("json-rfc8259-v1:missing_column"),
    }
}
