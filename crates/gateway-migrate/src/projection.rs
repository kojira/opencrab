use crate::{
    canonical,
    destination::LegacyNostrCoreUpdate,
    manifest::{Approval, IdentityEdge},
    source::SourceRow,
};
use anyhow::{ensure, Context, Result};
use opencrab_db::queries::{
    insert_api_principal_in_tx, revise_gate_instance_in_tx, ApiPrincipalRow,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectionOutcome {
    #[serde(skip)]
    pub already_applied: bool,
    pub before_logical_sha256: String,
    pub after_logical_sha256: String,
    pub inserted_api_principal_ids: Vec<String>,
    pub accepted_existing_api_principal_ids: Vec<String>,
    pub deliveries: DeliveryProof,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryProof {
    pub row_count: u64,
    pub logical_sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Marker {
    operation_id: String,
    approval_sha256: String,
    backup_set_sha256: String,
    source_core_sha256: String,
    source_fingerprint_sha256: String,
    subject_lineage_sha256: String,
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
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? == 57,
        "core schema must be 57"
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
    let api = build_api_principals(rows, approval)?;
    let initial = initial_projection_hash(conn, &api)?;
    let expected = Marker {
        operation_id: approval.operation_id.clone(),
        approval_sha256: approval.sha256()?,
        backup_set_sha256: backup_set_sha256.into(),
        source_core_sha256: approval.source_core_sha256.clone(),
        source_fingerprint_sha256: source_fp,
        subject_lineage_sha256: subject_lineage,
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
    }))
}

/// Post-QC verification binds the initial projection fingerprint through the persisted marker.
pub fn verify_immutable_marker_for_freeze(
    conn: &Connection,
    rows: &[SourceRow],
    approval: &Approval,
    backup_set_sha256: &str,
    destination_manifest_sha256: &str,
) -> Result<()> {
    ensure!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? == 57,
        "core schema must be 57"
    );
    let existing =
        read_marker(conn, &approval.operation_id)?.context("projection marker missing")?;
    let expected = Marker {
        operation_id: approval.operation_id.clone(),
        approval_sha256: approval.sha256()?,
        backup_set_sha256: backup_set_sha256.into(),
        source_core_sha256: approval.source_core_sha256.clone(),
        source_fingerprint_sha256: all_source_fingerprint_hash(rows)?,
        subject_lineage_sha256: subject_lineage(conn)?,
        initial_projection_sha256: existing.initial_projection_sha256.clone(),
        destination_manifest_sha256: destination_manifest_sha256.into(),
    };
    ensure!(existing == expected, "projection marker mismatch");
    Ok(())
}

pub(crate) fn project(
    conn: &mut Connection,
    rows: &[SourceRow],
    approval: &Approval,
    backup_set_sha256: &str,
    destination_manifest_sha256: &str,
    legacy_updates: &[LegacyNostrCoreUpdate],
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
    let api = build_api_principals(rows, approval)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    validate_subject_safeguards(&tx)?;
    let revision_at = chrono::DateTime::parse_from_rfc3339(&approval.created_at)?.timestamp();
    for update in legacy_updates {
        let revised = revise_gate_instance_in_tx(
            &tx,
            &update.instance_id,
            update.expected_revision,
            update.enabled,
            &update.config_b64,
            &update.config_digest,
            revision_at,
        )
        .map_err(|error| anyhow::anyhow!("legacy Nostr core revision failed: {error:?}"))?;
        ensure!(
            revised == update.expected_revision + 1,
            "legacy Nostr revision mismatch"
        );
    }
    let mut inserted_api = Vec::new();
    let mut accepted_api = Vec::new();
    for row in &api {
        if insert_api_principal_in_tx(&tx, row)? {
            inserted_api.push(row.id.clone())
        } else {
            accepted_api.push(row.id.clone())
        }
    }
    let initial = initial_projection_hash(&tx, &api)?;
    tx.execute_batch(MARKER_SCHEMA)?;
    let marker = Marker {
        operation_id: approval.operation_id.clone(),
        approval_sha256: approval_hash,
        backup_set_sha256: backup_set_sha256.into(),
        source_core_sha256: approval.source_core_sha256.clone(),
        source_fingerprint_sha256: source_fp,
        subject_lineage_sha256: subject_lineage,
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
fn initial_projection_hash(conn: &Connection, api: &[ApiPrincipalRow]) -> Result<String> {
    let ids = api.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    canonical::hash(&json!({"api_principal_ids":ids,"subject_lineage":subject_lineage(conn)?}))
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

const MARKER_SCHEMA:&str="CREATE TABLE IF NOT EXISTS separation_migrations(operation_id TEXT PRIMARY KEY,migration_version INTEGER NOT NULL,approval_sha256 TEXT NOT NULL,backup_set_sha256 TEXT NOT NULL,source_core_sha256 TEXT NOT NULL,source_fingerprint_sha256 TEXT NOT NULL,subject_lineage_sha256 TEXT NOT NULL,initial_projection_sha256 TEXT NOT NULL,destination_manifest_sha256 TEXT NOT NULL,applied_at TEXT NOT NULL); CREATE TRIGGER IF NOT EXISTS separation_migrations_no_update BEFORE UPDATE ON separation_migrations BEGIN SELECT RAISE(ABORT,'separation marker immutable'); END; CREATE TRIGGER IF NOT EXISTS separation_migrations_no_delete BEFORE DELETE ON separation_migrations BEGIN SELECT RAISE(ABORT,'separation marker immutable'); END;";
fn insert_marker(conn: &Connection, m: &Marker) -> Result<()> {
    conn.execute(
        "INSERT INTO separation_migrations VALUES (?1,1,?2,?3,?4,?5,?6,?7,?8,datetime('now'))",
        params![
            m.operation_id,
            m.approval_sha256,
            m.backup_set_sha256,
            m.source_core_sha256,
            m.source_fingerprint_sha256,
            m.subject_lineage_sha256,
            m.initial_projection_sha256,
            m.destination_manifest_sha256
        ],
    )?;
    Ok(())
}
fn read_marker(conn: &Connection, id: &str) -> Result<Option<Marker>> {
    Ok(conn.query_row("SELECT operation_id,approval_sha256,backup_set_sha256,source_core_sha256,source_fingerprint_sha256,subject_lineage_sha256,initial_projection_sha256,destination_manifest_sha256 FROM separation_migrations WHERE operation_id=?1",[id],|r|Ok(Marker{operation_id:r.get(0)?,approval_sha256:r.get(1)?,backup_set_sha256:r.get(2)?,source_core_sha256:r.get(3)?,source_fingerprint_sha256:r.get(4)?,subject_lineage_sha256:r.get(5)?,initial_projection_sha256:r.get(6)?,destination_manifest_sha256:r.get(7)?})).optional()?)
}
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}
