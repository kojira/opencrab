use crate::{
    backup::{self, BackupRecord},
    canonical,
    destination::{self, Inputs},
    manifest::Approval,
    projection, source,
};
use anyhow::{ensure, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportReport {
    pub version: u64,
    pub approval: Approval,
    pub approval_sha256: String,
    pub backup_set_sha256: String,
    pub backups: Vec<BackupRecord>,
    pub source_rows: Vec<SourceRowsProof>,
    pub destinations: Value,
    pub destination_manifest_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceRowsProof {
    pub table: String,
    pub row_count: u64,
    pub fingerprint_set_sha256: String,
}

pub struct ImportArgs<'a> {
    pub core_path: &'a Path,
    pub approval_path: &'a Path,
    pub backup_dir: &'a Path,
    pub report_path: &'a Path,
    pub inputs: Inputs,
}
pub struct ProjectArgs<'a> {
    pub core_path: &'a Path,
    pub approval_path: &'a Path,
    pub import_report_path: &'a Path,
    pub verification_path: &'a Path,
    pub destination_paths: BTreeMap<(String, String), PathBuf>,
}

pub fn run_import(args: ImportArgs<'_>) -> Result<ImportReport> {
    let approval = Approval::load(args.approval_path)?;
    ensure!(
        source::file_sha256(args.core_path)? == approval.source_core_sha256,
        "source core hash mismatch"
    );
    let core = source::open_read_only(args.core_path)?;
    let rows = source::validate(&core)?;
    let paths = approval
        .destinations
        .iter()
        .map(|d| {
            let key = (d.kind_id.clone(), d.path_id.clone());
            let path = args
                .inputs
                .paths
                .get(&key)
                .context("missing destination input")?
                .clone();
            Ok((d.clone(), path))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        paths.len() == args.inputs.paths.len(),
        "extra destination input"
    );
    let before_core = source::file_sha256(args.core_path)?;
    // Build and validate the complete plan before snapshots or writes.
    destination::prevalidate(&core, &rows, &approval, &args.inputs)?;
    let backups = backup::create_or_load_set(args.core_path, &paths, args.backup_dir)?;
    let backup_set_sha256 = backup::set_sha256(&backups)?;
    let source_rows = source_rows_proof(&rows)?;
    if args.report_path.exists() {
        let raw = fs::read(args.report_path)?;
        let existing: ImportReport = serde_json::from_slice(&raw)?;
        ensure!(
            raw == canonical::bytes(&existing)?,
            "existing import report not canonical"
        );
        ensure!(
            existing.version == 1 && existing.approval == approval,
            "existing import report approval mismatch"
        );
        ensure!(
            existing.approval_sha256 == approval.sha256()?,
            "existing import report approval hash mismatch"
        );
        ensure!(
            existing.backups == backups && existing.backup_set_sha256 == backup_set_sha256,
            "existing import report backup mismatch"
        );
        ensure!(
            existing.source_rows == source_rows,
            "existing import report source mismatch"
        );
        ensure!(
            canonical::hash(&existing.destinations)? == existing.destination_manifest_sha256,
            "existing destination manifest mismatch"
        );
        destination::validate_project_artifacts(
            args.approval_path,
            &approval,
            &args.inputs.paths,
            &backups,
            &backup_set_sha256,
            &existing.destinations,
        )?;
        ensure!(
            source::file_sha256(args.core_path)? == before_core,
            "import modified core"
        );
        return Ok(existing);
    }
    let destinations = destination::import(
        &core,
        &rows,
        &approval,
        &args.inputs,
        &backups,
        args.backup_dir,
        &backup_set_sha256,
        args.approval_path,
    )?;
    ensure!(
        source::file_sha256(args.core_path)? == before_core,
        "import modified core"
    );
    let destination_manifest_sha256 = canonical::hash(&destinations)?;
    let report = ImportReport {
        version: 1,
        approval_sha256: approval.sha256()?,
        approval,
        backup_set_sha256,
        backups,
        source_rows,
        destinations,
        destination_manifest_sha256,
    };
    write_or_accept_secure(args.report_path, &canonical::bytes(&report)?)?;
    Ok(report)
}

// gateway-legacy offline writer: project-core-state
pub fn run_project(args: ProjectArgs<'_>) -> Result<Value> {
    let raw = fs::read(args.import_report_path)?;
    let report: ImportReport = serde_json::from_slice(&raw)?;
    ensure!(
        raw == canonical::bytes(&report)?,
        "import report not canonical"
    );
    ensure!(report.version == 1, "report version");
    let approval = Approval::load(args.approval_path)?;
    ensure!(approval == report.approval, "approval/report mismatch");
    ensure!(
        report.approval.sha256()? == report.approval_sha256,
        "approval hash mismatch"
    );
    ensure!(
        backup::set_sha256(&report.backups)? == report.backup_set_sha256,
        "backup set mismatch"
    );
    ensure!(
        canonical::hash(&report.destinations)? == report.destination_manifest_sha256,
        "destination manifest mismatch"
    );
    let conn_ro = source::open_read_only(args.core_path)?;
    let rows = source::validate(&conn_ro)?;
    destination::validate_project_artifacts(
        args.approval_path,
        &approval,
        &args.destination_paths,
        &report.backups,
        &report.backup_set_sha256,
        &report.destinations,
    )?;
    let outcome = match projection::verify_already_applied(
        &conn_ro,
        &rows,
        &approval,
        &report.backup_set_sha256,
        &report.destination_manifest_sha256,
    )? {
        Some(outcome) => outcome,
        None => {
            ensure!(
                source::file_sha256(args.core_path)? == approval.source_core_sha256,
                "source changed"
            );
            drop(conn_ro);
            let mut conn = Connection::open(args.core_path)?;
            projection::project(
                &mut conn,
                &rows,
                &approval,
                &report.backup_set_sha256,
                &report.destination_manifest_sha256,
            )?
        }
    };
    let core_projection = serde_json::to_value(&outcome)?;
    let mut value = serde_json::json!({"version":1,"approval":report.approval,"approval_sha256":report.approval_sha256,"backup_set_sha256":report.backup_set_sha256,"backups":report.backups,"source_rows":report.source_rows,"destinations":report.destinations,"core_projection":core_projection});
    let digest = canonical::hash(&value)?;
    value
        .as_object_mut()
        .unwrap()
        .insert("manifest_sha256".into(), Value::String(digest));
    write_or_accept_secure(args.verification_path, &canonical::value_bytes(&value)?)?;
    Ok(value)
}

fn source_rows_proof(rows: &[source::SourceRow]) -> Result<Vec<SourceRowsProof>> {
    let mut output = Vec::new();
    for (table, _) in source::CONCRETE_TABLES {
        let selected = rows
            .iter()
            .filter(|row| row.table == *table)
            .collect::<Vec<_>>();
        if !selected.is_empty()
            || matches!(
                *table,
                "trusted_users" | "channel_config" | "session_watches"
            )
        {
            output.push(SourceRowsProof {
                table: (*table).into(),
                row_count: selected.len() as u64,
                fingerprint_set_sha256: source::fingerprint_set_sha256(&selected)?,
            });
        }
    }
    output.sort_by(|a, b| a.table.cmp(&b.table));
    Ok(output)
}

fn write_new_secure(path: &Path, content: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(path)?;
    use std::io::Write as _;
    file.write_all(content)?;
    file.sync_all()?;
    Ok(())
}
fn write_or_accept_secure(path: &Path, content: &[u8]) -> Result<()> {
    match write_new_secure(path, content) {
        Ok(()) => Ok(()),
        Err(_error) if path.is_file() => {
            ensure!(fs::read(path)? == content, "existing verification differs");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub fn parse_kv_paths(values: &[String]) -> Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    for value in values {
        let (key, path) = value
            .split_once('=')
            .context("expected key=/absolute/path")?;
        let path = PathBuf::from(path);
        ensure!(path.is_absolute(), "path must be absolute");
        ensure!(out.insert(key.into(), path).is_none(), "duplicate path key");
    }
    Ok(out)
}

pub fn parse_destination_paths(values: &[String]) -> Result<BTreeMap<(String, String), PathBuf>> {
    let mut out = BTreeMap::new();
    for value in values {
        let mut parts = value.splitn(3, '=');
        let kind = parts.next().unwrap_or_default();
        let path_id = parts.next().context("destination kind=path_id=/path")?;
        let path = PathBuf::from(parts.next().context("destination kind=path_id=/path")?);
        ensure!(path.is_absolute(), "destination path absolute");
        ensure!(
            out.insert((kind.into(), path_id.into()), path).is_none(),
            "duplicate destination"
        );
    }
    Ok(out)
}

#[cfg(test)]
mod s8_review_red_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn import_report_rejects_unknown_destination_fields() {
        let value = json!({
            "version":1,
            "approval":{"version":1,"operation_id":"00000000-0000-4000-8000-000000000008","created_at":"2026-01-01T00:00:00Z","core_user_version":56,"source_core_sha256":"a".repeat(64),"destinations":[],"identity_dispositions":[],"channel_edges":[],"watch_edges":[],"credential_sources":[]},
            "approval_sha256":"b".repeat(64),"backup_set_sha256":"c".repeat(64),"backups":[],"source_rows":[],
            "destinations":[{"kind_id":"discord","path_id":"main","schema":"s5-discord-v1","before_logical_sha256":"d".repeat(64),"after_logical_sha256":"e".repeat(64),"counts":{"instances":0,"endpoints":0,"identity_projections":0,"policies":0,"credentials":0},"inserted_keys":[],"accepted_existing_keys":[],"credentials":[],"plaintext":"forbidden"}],
            "destination_manifest_sha256":"f".repeat(64)
        });
        assert!(
            serde_json::from_value::<ImportReport>(value).is_err(),
            "unknown destination fields must fail typed decoding"
        );
    }
}
