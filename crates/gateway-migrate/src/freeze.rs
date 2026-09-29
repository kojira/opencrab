//! Stopped, read-only verification of one post-QC matched snapshot set.

use crate::{
    backup::{self, BackupRecord},
    canonical,
    command::{self, ImportReport},
    destination,
    manifest::{Approval, IdentityEdge},
    projection, source,
};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct VerifyArgs<'a> {
    pub core_path: &'a Path,
    pub approval_path: &'a Path,
    pub import_report_path: &'a Path,
    pub verification_path: &'a Path,
    pub freeze_dir: &'a Path,
    pub freeze_manifest_path: &'a Path,
    pub destination_paths: BTreeMap<(String, String), PathBuf>,
}

#[derive(Deserialize)]
struct FreezeManifest {
    version: u64,
    freeze_id: String,
    approval_sha256: String,
    projection_manifest_sha256: String,
    projection_marker_sha256: String,
    identity_dispositions_sha256: String,
    snapshots: Vec<FreezeSnapshot>,
}

#[derive(Deserialize)]
struct FreezeSnapshot {
    freeze_id: String,
    live_file_sha256: String,
    #[serde(flatten)]
    record: BackupRecord,
}

pub fn verify(args: VerifyArgs<'_>) -> Result<()> {
    let approval = Approval::load(args.approval_path)?;
    let report_raw = fs::read(args.import_report_path)?;
    let report: ImportReport = serde_json::from_slice(&report_raw)?;
    ensure!(
        report_raw == canonical::bytes(&report)?,
        "import report not canonical"
    );
    ensure!(
        report.version == 1 && report.approval_sha256 == approval.sha256()?,
        "import approval mismatch"
    );
    ensure!(
        backup::set_sha256(&report.backups)? == report.backup_set_sha256,
        "import backup digest mismatch"
    );
    ensure!(
        canonical::hash(&report.destinations)? == report.destination_manifest_sha256,
        "import destination digest mismatch"
    );
    let verification_raw = fs::read(args.verification_path)?;
    let verification: Value = serde_json::from_slice(&verification_raw)?;
    ensure!(
        verification_raw == canonical::value_bytes(&verification)?,
        "projection verification not canonical"
    );
    let mut unsigned = verification.clone();
    let verification_digest = unsigned
        .as_object_mut()
        .context("projection verification object")?
        .remove("manifest_sha256")
        .context("projection verification digest missing")?;
    ensure!(
        verification_digest == canonical::hash(&unsigned)?,
        "projection verification digest mismatch"
    );
    ensure!(
        verification["version"] == 1
            && verification["approval_sha256"] == report.approval_sha256
            && verification["backup_set_sha256"] == report.backup_set_sha256
            && verification["backups"] == serde_json::to_value(&report.backups)?
            && verification["source_rows"] == serde_json::to_value(&report.source_rows)?
            && verification["destinations"] == report.destinations,
        "projection/import lineage mismatch"
    );

    let freeze: FreezeManifest = serde_json::from_slice(&fs::read(args.freeze_manifest_path)?)?;
    ensure!(
        freeze.version == 1 && uuid::Uuid::parse_str(&freeze.freeze_id).is_ok(),
        "freeze id/version invalid"
    );
    ensure!(
        freeze.approval_sha256 == report.approval_sha256
            && Value::String(freeze.projection_manifest_sha256.clone()) == verification_digest
            && freeze.identity_dispositions_sha256
                == canonical::hash(&approval.identity_dispositions)?,
        "freeze lineage mismatch"
    );
    ensure!(
        freeze
            .snapshots
            .iter()
            .all(|entry| entry.freeze_id == freeze.freeze_id),
        "snapshot freeze id mismatch"
    );
    let records = freeze
        .snapshots
        .iter()
        .map(|entry| entry.record.clone())
        .collect::<Vec<_>>();
    let mut paths = BTreeMap::new();
    paths.insert(("core".into(), "core".into()), args.core_path.to_path_buf());
    ensure!(
        args.destination_paths.len() == approval.destinations.len(),
        "freeze destination count mismatch"
    );
    for destination in &approval.destinations {
        let key = (destination.kind_id.clone(), destination.path_id.clone());
        let path = args
            .destination_paths
            .get(&key)
            .context("freeze destination missing")?;
        paths.insert(key, path.clone());
    }
    ensure!(
        records.len() == paths.len(),
        "freeze snapshot count mismatch"
    );
    let mut seen = BTreeMap::new();
    for snapshot in &freeze.snapshots {
        let record = &snapshot.record;
        let key = (record.kind_id.clone(), record.path_id.clone());
        let expected_schema = if key == ("core".into(), "core".into()) {
            "core-v58"
        } else {
            approval
                .destinations
                .iter()
                .find(|dest| dest.kind_id == record.kind_id && dest.path_id == record.path_id)
                .context("unapproved freeze snapshot")?
                .schema
                .as_str()
        };
        ensure!(
            record.schema == expected_schema && seen.insert(key, snapshot).is_none(),
            "freeze inventory/schema mismatch"
        );
    }
    ensure!(
        seen.len() == paths.len() && paths.keys().all(|key| seen.contains_key(key)),
        "freeze inventory incomplete"
    );
    backup::verify_record_set(args.freeze_dir, &records)?;

    // Keep every live database opened strictly read-only throughout validation. Neither this
    // preflight nor logical hashing creates a journal, marker, or replacement snapshot.
    let mut handles = BTreeMap::new();
    let mut before = BTreeMap::new();
    for (key, path) in &paths {
        let conn = source::open_read_only(path)?;
        let digest = (source::file_sha256(path)?, backup::logical_sha256(path)?);
        let recorded = seen.get(key).context("freeze snapshot missing")?;
        ensure!(
            digest.0 == recorded.live_file_sha256 && digest.1 == recorded.record.logical_sha256,
            "live/snapshot digest mismatch"
        );
        if key == &("core".into(), "core".into()) {
            ensure!(
                conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? == 58,
                "freeze core schema mismatch"
            );
        } else {
            let destination = approval
                .destinations
                .iter()
                .find(|dest| dest.kind_id == key.0 && dest.path_id == key.1)
                .context("freeze destination missing")?;
            destination::require_destination_shape(&conn, destination)?;
        }
        before.insert(key.clone(), digest);
        handles.insert(key.clone(), conn);
    }
    let core = handles
        .get(&("core".into(), "core".into()))
        .context("core handle missing")?;
    let rows = source::validate(core)?;
    ensure!(
        command::source_rows_proof(&rows)? == report.source_rows,
        "freeze source rows mismatch"
    );
    projection::verify_immutable_marker_for_freeze(
        core,
        &rows,
        &approval,
        &report.backup_set_sha256,
        &report.destination_manifest_sha256,
    )?;
    let marker = core.query_row(
        "SELECT operation_id,approval_sha256,backup_set_sha256,source_core_sha256,\
         source_fingerprint_sha256,subject_lineage_sha256,\
         initial_projection_sha256,destination_manifest_sha256 \
         FROM separation_migrations WHERE operation_id=?1",
        [&approval.operation_id],
        |row| {
            (0..8)
                .map(|index| row.get::<_, String>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        },
    )?;
    ensure!(
        canonical::hash(&marker)? == freeze.projection_marker_sha256,
        "freeze marker mismatch"
    );
    verify_identity_edges(core, &handles, &rows, &approval)?;
    for (key, path) in &paths {
        let after = (source::file_sha256(path)?, backup::logical_sha256(path)?);
        ensure!(
            Some(&after) == before.get(key),
            "read-only freeze preflight changed database"
        );
    }
    Ok(())
}

fn verify_identity_edges(
    core: &Connection,
    handles: &BTreeMap<(String, String), Connection>,
    rows: &[source::SourceRow],
    approval: &Approval,
) -> Result<()> {
    let identities = rows
        .iter()
        .filter(|row| row.table == "trusted_users")
        .collect::<Vec<_>>();
    ensure!(
        identities.len() == approval.identity_dispositions.len(),
        "freeze identity disposition count mismatch"
    );
    for row in identities {
        let disposition = approval
            .identity_dispositions
            .iter()
            .find(|item| item.source_fingerprint == row.fingerprint)
            .context("freeze identity disposition missing")?;
        let id = row.text("id")?;
        let user_id = row.text("user_id")?;
        let agent_id = row.text("agent_id")?;
        let permission = row.text("permission")?;
        let created_by = row.text("created_by")?;
        let created_at = row.text("created_at")?;
        let display_name = row.text("display_name")?;
        let platform = row.text("platform")?;
        let role = match permission {
            "owner" => "owner",
            "co-agent" => "co_agent",
            _ => "trusted_user",
        };
        for edge in &disposition.edges {
            match edge {
                IdentityEdge::ApiPrincipal => {
                    ensure!(platform == "rest", "only REST can target core principal");
                    let present: bool = core.query_row(
                        "SELECT EXISTS(SELECT 1 FROM api_principals WHERE id=?1 AND user_id=?2 AND agent_id=?3 AND permission=?4 AND created_by=?5 AND created_at=?6 AND display_name=?7)",
                        params![id, user_id, agent_id, permission, created_by, created_at, display_name],
                        |r| r.get(0),
                    )?;
                    ensure!(present, "frozen core identity mismatch");
                }
                IdentityEdge::Gateway {
                    kind_id,
                    instance_id,
                } => {
                    let destination = approval.destination(kind_id)?;
                    let conn = handles
                        .get(&(kind_id.clone(), destination.path_id.clone()))
                        .context("identity destination missing")?;
                    let same_agent: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM instances WHERE instance_id=?1 AND agent_id=?2)",
                        params![instance_id, agent_id],
                        |r| r.get(0),
                    )?;
                    ensure!(same_agent, "frozen identity instance owner mismatch");
                    let full: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM legacy_identity_sources WHERE instance_id=?1 AND id=?2 AND user_id=?3 AND agent_id=?4 AND permission=?5 AND created_by=?6 AND created_at=?7 AND display_name=?8 AND platform=?9)",
                        params![instance_id, id, user_id, agent_id, permission, created_by, created_at, display_name, platform],
                        |r| r.get(0),
                    )?;
                    ensure!(full, "frozen legacy identity source mismatch");
                    let projected: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM identity_projections WHERE instance_id=?1 AND role=?2 AND external_id=?3)",
                        params![instance_id, role, user_id],
                        |r| r.get(0),
                    )?;
                    ensure!(projected, "frozen identity projection mismatch");
                }
            }
        }
    }
    Ok(())
}
