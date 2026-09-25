use crate::{
    canonical,
    manifest::{Approval, IdentityEdge},
    source::SourceRow,
};
use anyhow::{ensure, Context, Result};
use opencrab_db::queries::{
    insert_api_principal_in_tx, project_stopped_session_heartbeat_target_in_tx,
    resolve_heartbeat_projection_sources, ApiPrincipalRow, HeartbeatProjectionSource,
    StoppedHeartbeatProjectionTarget,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectionOutcome {
    #[serde(skip)]
    pub already_applied: bool,
    pub before_logical_sha256: String,
    pub after_logical_sha256: String,
    pub inserted_api_principal_ids: Vec<String>,
    pub accepted_existing_api_principal_ids: Vec<String>,
    pub deliveries: DeliveryProof,
    pub heartbeat: HeartbeatProof,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryProof {
    pub row_count: u64,
    pub logical_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatProof {
    pub inserted_keys: Vec<HeartbeatKey>,
    pub accepted_existing_keys: Vec<HeartbeatKey>,
    pub initial_sha256: String,
    pub lineage_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct HeartbeatKey {
    pub agent_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Marker {
    operation_id: String,
    approval_sha256: String,
    backup_set_sha256: String,
    source_core_sha256: String,
    source_fingerprint_sha256: String,
    subject_lineage_sha256: String,
    heartbeat_lineage_sha256: String,
    initial_projection_sha256: String,
    destination_manifest_sha256: String,
}

pub fn verify_already_applied(
    conn: &Connection,
    rows: &[SourceRow],
    approval: &Approval,
    backup_set_sha256: &str,
    destination_manifest_sha256: &str,
) -> Result<Option<ProjectionOutcome>> {
    ensure!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? == 56,
        "core schema must be 56"
    );
    if !table_exists(conn, "separation_migrations")? {
        return Ok(None);
    }
    let Some(existing) = read_marker(conn, &approval.operation_id)? else {
        return Ok(None);
    };
    let deliveries = delivery_proof(conn)?;
    let protected = protected_digest(conn)?;
    let source_fp = all_source_fingerprint_hash(rows)?;
    let subject_lineage = subject_lineage(conn)?;
    let heartbeat_targets = build_heartbeat_targets(conn, rows, approval)?;
    let api = build_api_principals(rows, approval)?;
    let heartbeat = heartbeat_proof(conn, &heartbeat_targets, &BTreeSet::new())?;
    let initial = initial_projection_hash(conn, &api, &heartbeat)?;
    let expected = Marker {
        operation_id: approval.operation_id.clone(),
        approval_sha256: approval.sha256()?,
        backup_set_sha256: backup_set_sha256.into(),
        source_core_sha256: approval.source_core_sha256.clone(),
        source_fingerprint_sha256: source_fp,
        subject_lineage_sha256: subject_lineage,
        heartbeat_lineage_sha256: heartbeat.lineage_sha256.clone(),
        initial_projection_sha256: initial,
        destination_manifest_sha256: destination_manifest_sha256.into(),
    };
    ensure!(existing == expected, "projection marker mismatch");
    Ok(Some(ProjectionOutcome {
        already_applied: true,
        before_logical_sha256: protected.clone(),
        after_logical_sha256: protected,
        inserted_api_principal_ids: Vec::new(),
        accepted_existing_api_principal_ids: api.iter().map(|row| row.id.clone()).collect(),
        deliveries,
        heartbeat,
    }))
}

pub fn project(
    conn: &mut Connection,
    rows: &[SourceRow],
    approval: &Approval,
    backup_set_sha256: &str,
    destination_manifest_sha256: &str,
) -> Result<ProjectionOutcome> {
    ensure!(
        verify_already_applied(
            conn,
            rows,
            approval,
            backup_set_sha256,
            destination_manifest_sha256
        )?
        .is_none(),
        "projection is already applied"
    );
    let deliveries_before = delivery_proof(conn)?;
    let before = protected_digest(conn)?;
    let source_fp = all_source_fingerprint_hash(rows)?;
    let subject_lineage = subject_lineage(conn)?;
    let approval_hash = approval.sha256()?;
    let heartbeat_targets = build_heartbeat_targets(conn, rows, approval)?;
    let api = build_api_principals(rows, approval)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    validate_subject_safeguards(&tx)?;
    let mut inserted_api = Vec::new();
    let mut accepted_api = Vec::new();
    for row in &api {
        if insert_api_principal_in_tx(&tx, row)? {
            inserted_api.push(row.id.clone())
        } else {
            accepted_api.push(row.id.clone())
        }
    }
    let mut inserted_hb = BTreeSet::new();
    for target in &heartbeat_targets {
        let existed = heartbeat_pair_exists(&tx, &target.agent_id, &target.session_id)?;
        project_stopped_session_heartbeat_target_in_tx(&tx, target)
            .map_err(|error| anyhow::anyhow!(error))?;
        if !existed {
            inserted_hb.insert((target.agent_id.clone(), target.session_id.clone()));
        }
    }
    let heartbeat = heartbeat_proof(&tx, &heartbeat_targets, &inserted_hb)?;
    let initial = initial_projection_hash(&tx, &api, &heartbeat)?;
    tx.execute_batch(MARKER_SCHEMA)?;
    let marker = Marker {
        operation_id: approval.operation_id.clone(),
        approval_sha256: approval_hash,
        backup_set_sha256: backup_set_sha256.into(),
        source_core_sha256: approval.source_core_sha256.clone(),
        source_fingerprint_sha256: source_fp,
        subject_lineage_sha256: subject_lineage,
        heartbeat_lineage_sha256: heartbeat.lineage_sha256.clone(),
        initial_projection_sha256: initial,
        destination_manifest_sha256: destination_manifest_sha256.into(),
    };
    insert_marker(&tx, &marker)?;
    ensure!(
        delivery_proof(&tx)? == deliveries_before,
        "deliveries changed during projection"
    );
    tx.commit()?;
    let after = protected_digest(conn)?;
    inserted_api.sort();
    accepted_api.sort();
    Ok(ProjectionOutcome {
        already_applied: false,
        before_logical_sha256: before,
        after_logical_sha256: after,
        inserted_api_principal_ids: inserted_api,
        accepted_existing_api_principal_ids: accepted_api,
        deliveries: deliveries_before,
        heartbeat,
    })
}

fn build_api_principals(rows: &[SourceRow], approval: &Approval) -> Result<Vec<ApiPrincipalRow>> {
    let mut output = Vec::new();
    for row in rows.iter().filter(|row| row.table == "trusted_users") {
        let disposition = approval
            .identity_dispositions
            .iter()
            .find(|item| item.source_fingerprint == row.fingerprint)
            .context("identity disposition missing")?;
        let is_api = disposition
            .edges
            .iter()
            .any(|edge| matches!(edge, IdentityEdge::ApiPrincipal));
        if is_api {
            ensure!(
                row.text("platform")? == "rest",
                "only rest may target api_principals"
            );
            output.push(ApiPrincipalRow {
                id: row.text("id")?.into(),
                user_id: row.text("user_id")?.into(),
                agent_id: row.text("agent_id")?.into(),
                permission: row.text("permission")?.into(),
                created_by: row.text("created_by")?.into(),
                created_at: row.text("created_at")?.into(),
                display_name: row.text("display_name")?.into(),
            });
        }
    }
    output.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(output)
}

fn build_heartbeat_targets(
    conn: &Connection,
    rows: &[SourceRow],
    approval: &Approval,
) -> Result<Vec<StoppedHeartbeatProjectionTarget>> {
    let mut grouped: BTreeMap<(String, String), Vec<&SourceRow>> = BTreeMap::new();
    for edge in &approval.channel_edges {
        let source = rows
            .iter()
            .find(|row| row.fingerprint == edge.source_fingerprint && row.table == "channel_config")
            .context("channel source missing")?;
        let (agent_id,session_id):(String,String)=conn.query_row("SELECT a.agent_id,b.session_id FROM gate_bindings b JOIN gate_instances i ON i.instance_id=b.instance_id JOIN agents a ON a.subject_id=i.subject_id WHERE b.binding_id=?1 AND b.instance_id=?2 AND b.closed_at IS NULL",params![edge.binding_id,edge.instance_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(session_id == edge.session_id, "heartbeat session mismatch");
        ensure!(
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_sessions WHERE agent_id=?1 AND session_id=?2)",
                params![agent_id, session_id],
                |row| row.get::<_, bool>(0)
            )?,
            "heartbeat session membership missing"
        );
        grouped
            .entry((agent_id, session_id))
            .or_default()
            .push(source);
    }
    let mut targets = Vec::new();
    for ((agent_id, session_id), sources) in grouped {
        for scope in [agent_id.as_str(), ""] {
            let scoped = sources
                .iter()
                .filter(|row| row.text("agent_id").ok() == Some(scope))
                .collect::<Vec<_>>();
            if let Some(first) = scoped.first() {
                let reference = (heartbeat_source(first)?, first.text("updated_at")?);
                for row in scoped.iter().skip(1) {
                    ensure!(
                        (heartbeat_source(row)?, row.text("updated_at")?) == reference,
                        "ambiguous heartbeat target for one session"
                    );
                }
            }
        }
        let exact = sources
            .iter()
            .find(|row| row.text("agent_id").ok() == Some(agent_id.as_str()))
            .map(|row| heartbeat_source(row))
            .transpose()?;
        let global = sources
            .iter()
            .find(|row| row.text("agent_id").ok() == Some(""))
            .map(|row| heartbeat_source(row))
            .transpose()?;
        let resolved = resolve_heartbeat_projection_sources(exact.as_ref(), global.as_ref())
            .context("heartbeat source unavailable")?;
        let effective = sources
            .iter()
            .find(|row| row.text("agent_id").ok() == Some(agent_id.as_str()))
            .or_else(|| {
                sources
                    .iter()
                    .find(|row| row.text("agent_id").ok() == Some(""))
            })
            .unwrap();
        targets.push(StoppedHeartbeatProjectionTarget {
            agent_id,
            session_id,
            enabled: resolved.enabled,
            interval_secs: resolved.interval_secs,
            anchor_at: Some(effective.text("updated_at")?.into()),
            last_fired_at: None,
            override_text: resolved.override_text,
            updated_at: effective.text("updated_at")?.into(),
        });
    }
    targets.sort_by(|a, b| (&a.agent_id, &a.session_id).cmp(&(&b.agent_id, &b.session_id)));
    Ok(targets)
}
#[cfg(test)]
mod s8_minimal_heartbeat_red {
    use super::*;
    use crate::{manifest::ChannelEdge, source::Cell};

    #[test]
    fn conflicting_channels_for_one_session_cannot_choose_an_arbitrary_heartbeat() {
        let core = opencrab_db::init_memory().unwrap();
        core.execute("INSERT INTO agents(agent_id,name,persona_name,instructions,created_at,updated_at) VALUES ('agent-a','A','A','','2026','2026')", []).unwrap();
        let subject: i64 = core
            .query_row(
                "SELECT subject_id FROM agents WHERE agent_id='agent-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        core.execute(
            "INSERT INTO sessions(id,theme,created_at,updated_at) VALUES ('s','t','2026','2026')",
            [],
        )
        .unwrap();
        core.execute(
            "INSERT INTO agent_sessions(agent_id,session_id) VALUES ('agent-a','s')",
            [],
        )
        .unwrap();
        core.execute("INSERT INTO gate_instances(instance_id,kind_id,subject_id,revision,enabled,config_b64,config_digest,created_at,updated_at,association_grandfathered) VALUES ('i','discord',?1,1,1,'e30=','44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a',1,1,1)", [subject]).unwrap();
        core.execute("INSERT INTO gate_bindings(binding_id,instance_id,address,created_at,session_id) VALUES ('b1','i','discord-agent-a--42',1,'s'),('b2','i','discord-agent-a--43',1,'s')", []).unwrap();
        let rows = [600, 700]
            .into_iter()
            .enumerate()
            .map(|(index, interval)| SourceRow {
                table: "channel_config".into(),
                fingerprint: format!("{index:064x}"),
                columns: vec![
                    ("agent_id".into(), Cell::Text("agent-a".into())),
                    ("heartbeat_enabled".into(), Cell::Integer(1)),
                    ("heartbeat_interval_secs".into(), Cell::Integer(interval)),
                    ("heartbeat_instructions".into(), Cell::Text("Ping".into())),
                    ("updated_at".into(), Cell::Text("2026".into())),
                ],
            })
            .collect::<Vec<_>>();
        let mut approval = Approval {
            version: 1,
            operation_id: "00000000-0000-4000-8000-000000000008".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            core_user_version: 56,
            source_core_sha256: "a".repeat(64),
            destinations: vec![],
            identity_dispositions: vec![],
            channel_edges: vec![],
            watch_edges: vec![],
            credential_sources: vec![],
        };
        for (index, row) in rows.iter().enumerate() {
            approval.channel_edges.push(ChannelEdge {
                source_fingerprint: row.fingerprint.clone(),
                instance_id: "i".into(),
                binding_id: format!("b{}", index + 1),
                session_id: "s".into(),
            });
        }
        assert!(
            build_heartbeat_targets(&core, &rows, &approval).is_err(),
            "different eligible heartbeat settings for one session must be rejected"
        );
    }
}

fn heartbeat_source(row: &SourceRow) -> Result<HeartbeatProjectionSource> {
    Ok(HeartbeatProjectionSource {
        enabled: row.integer("heartbeat_enabled")? != 0,
        interval_secs: row.optional_integer("heartbeat_interval_secs")?,
        instruction_text: Some(row.text("heartbeat_instructions")?.into()),
    })
}

fn heartbeat_proof(
    conn: &Connection,
    targets: &[StoppedHeartbeatProjectionTarget],
    inserted: &BTreeSet<(String, String)>,
) -> Result<HeartbeatProof> {
    let mut initial = Vec::new();
    let mut lineage = Vec::new();
    let mut inserted_keys = Vec::new();
    let mut accepted_existing_keys = Vec::new();
    for target in targets {
        let fp = opencrab_db::queries::heartbeat_projection_fingerprints(
            conn,
            &target.agent_id,
            &target.session_id,
        )?
        .context("heartbeat target missing")?;
        initial.push(fp.initial_fingerprint);
        lineage.push(fp.lineage_digest);
        let key = HeartbeatKey {
            agent_id: target.agent_id.clone(),
            session_id: target.session_id.clone(),
        };
        if inserted.contains(&(target.agent_id.clone(), target.session_id.clone())) {
            inserted_keys.push(key)
        } else {
            accepted_existing_keys.push(key)
        }
    }
    Ok(HeartbeatProof {
        inserted_keys,
        accepted_existing_keys,
        initial_sha256: canonical::hash(&initial)?,
        lineage_sha256: canonical::hash(&lineage)?,
    })
}
fn heartbeat_pair_exists(conn: &Connection, agent: &str, session: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT (EXISTS(SELECT 1 FROM session_heartbeat_config WHERE agent_id=?1 AND session_id=?2) AND EXISTS(SELECT 1 FROM session_heartbeat_instructions WHERE agent_id=?1 AND session_id=?2))",params![agent,session],|r|r.get(0))?)
}

fn delivery_proof(conn: &Connection) -> Result<DeliveryProof> {
    let row_count: i64 = conn.query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))?;
    let mut stmt=conn.prepare("SELECT delivery_id,binding_id,payload_json,state,error,created_at,updated_at FROM deliveries ORDER BY delivery_id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?
            ]))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(DeliveryProof {
        row_count: row_count as u64,
        logical_sha256: canonical::hash(&rows)?,
    })
}
fn protected_digest(conn: &Connection) -> Result<String> {
    use base64::Engine as _;
    use rusqlite::types::ValueRef;
    let mut result: BTreeMap<String, Vec<Vec<serde_json::Value>>> = BTreeMap::new();
    for table in [
        "agents",
        "sessions",
        "agent_sessions",
        "gate_instances",
        "gate_bindings",
        "deliveries",
    ] {
        let columns = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let order = columns
            .iter()
            .map(|name| format!("\"{}\"", name.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = conn.prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))?;
        let count = statement.column_count();
        let rows=statement.query_map([],|row|{
            let mut values=Vec::with_capacity(count);
            for index in 0..count {
                values.push(match row.get_ref(index)? {
                    ValueRef::Null=>json!({"type":"null"}),
                    ValueRef::Integer(value)=>json!({"type":"integer","value":value.to_string()}),
                    ValueRef::Real(value)=>json!({"type":"real","value":value.to_bits().to_string()}),
                    ValueRef::Text(value)=>json!({"type":"text","value":String::from_utf8_lossy(value)}),
                    ValueRef::Blob(value)=>json!({"type":"blob","value":base64::engine::general_purpose::STANDARD.encode(value)}),
                });
            }
            Ok(values)
        })?.collect::<std::result::Result<Vec<_>,_>>()?;
        result.insert(table.into(), rows);
    }
    canonical::hash(&result)
}
fn subject_lineage(conn: &Connection) -> Result<String> {
    let mut stmt=conn.prepare("SELECT a.agent_id,a.subject_id,i.instance_id,i.subject_id,i.association_grandfathered FROM agents a LEFT JOIN gate_instances i ON i.subject_id=a.subject_id ORDER BY a.agent_id,i.instance_id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?
            ]))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    canonical::hash(&rows)
}
fn all_source_fingerprint_hash(rows: &[SourceRow]) -> Result<String> {
    let mut hashes = rows
        .iter()
        .map(|r| r.fingerprint.clone())
        .collect::<Vec<_>>();
    hashes.sort();
    canonical::hash(&hashes)
}
fn initial_projection_hash(
    conn: &Connection,
    api: &[ApiPrincipalRow],
    heartbeat: &HeartbeatProof,
) -> Result<String> {
    let ids = api.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    canonical::hash(
        &json!({"api_principal_ids":ids,"heartbeat_initial_sha256":heartbeat.initial_sha256,"subject_lineage":subject_lineage(conn)?}),
    )
}
fn validate_subject_safeguards(conn: &Connection) -> Result<()> {
    let invalid:i64=conn.query_row("SELECT COUNT(*) FROM agents WHERE subject_id IS NULL OR typeof(subject_id)<>'integer' OR subject_id<=0",[],|r|r.get(0))?;
    ensure!(invalid == 0, "invalid subject IDs");
    let bad:i64=conn.query_row("SELECT COUNT(*) FROM gate_instances i LEFT JOIN agents a ON a.subject_id=i.subject_id WHERE a.agent_id IS NULL OR i.association_grandfathered<>1",[],|r|r.get(0))?;
    ensure!(bad == 0, "invalid subject associations");
    let next: i64 = conn.query_row(
        "SELECT next_subject_id FROM subject_id_allocator WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let max: i64 = conn.query_row("SELECT COALESCE(MAX(subject_id),0) FROM agents", [], |r| {
        r.get(0)
    })?;
    ensure!(next > max, "subject allocator not above high-water");
    Ok(())
}

const MARKER_SCHEMA:&str="CREATE TABLE IF NOT EXISTS separation_migrations(operation_id TEXT PRIMARY KEY,migration_version INTEGER NOT NULL,approval_sha256 TEXT NOT NULL,backup_set_sha256 TEXT NOT NULL,source_core_sha256 TEXT NOT NULL,source_fingerprint_sha256 TEXT NOT NULL,subject_lineage_sha256 TEXT NOT NULL,heartbeat_lineage_sha256 TEXT NOT NULL,initial_projection_sha256 TEXT NOT NULL,destination_manifest_sha256 TEXT NOT NULL,applied_at TEXT NOT NULL); CREATE TRIGGER IF NOT EXISTS separation_migrations_no_update BEFORE UPDATE ON separation_migrations BEGIN SELECT RAISE(ABORT,'separation marker immutable'); END; CREATE TRIGGER IF NOT EXISTS separation_migrations_no_delete BEFORE DELETE ON separation_migrations BEGIN SELECT RAISE(ABORT,'separation marker immutable'); END;";
fn insert_marker(conn: &Connection, m: &Marker) -> Result<()> {
    conn.execute(
        "INSERT INTO separation_migrations VALUES (?1,1,?2,?3,?4,?5,?6,?7,?8,?9,datetime('now'))",
        params![
            m.operation_id,
            m.approval_sha256,
            m.backup_set_sha256,
            m.source_core_sha256,
            m.source_fingerprint_sha256,
            m.subject_lineage_sha256,
            m.heartbeat_lineage_sha256,
            m.initial_projection_sha256,
            m.destination_manifest_sha256
        ],
    )?;
    Ok(())
}
fn read_marker(conn: &Connection, id: &str) -> Result<Option<Marker>> {
    Ok(conn.query_row("SELECT operation_id,approval_sha256,backup_set_sha256,source_core_sha256,source_fingerprint_sha256,subject_lineage_sha256,heartbeat_lineage_sha256,initial_projection_sha256,destination_manifest_sha256 FROM separation_migrations WHERE operation_id=?1",[id],|r|Ok(Marker{operation_id:r.get(0)?,approval_sha256:r.get(1)?,backup_set_sha256:r.get(2)?,source_core_sha256:r.get(3)?,source_fingerprint_sha256:r.get(4)?,subject_lineage_sha256:r.get(5)?,heartbeat_lineage_sha256:r.get(6)?,initial_projection_sha256:r.get(7)?,destination_manifest_sha256:r.get(8)?})).optional()?)
}
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}
