//! Second stopped offline writer: verify one frozen all-store set before deleting legacy sources.

use crate::{
    canonical,
    freeze::{self, VerifyArgs},
    manifest::Approval,
    source,
};
use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::{collections::BTreeMap, fs};

const LEGACY_TABLES: &[&str] = &[
    "trusted_users",
    "channel_config",
    "session_watches",
    "agent_discord_config",
    "agent_nostr_config",
];

pub fn run(args: VerifyArgs<'_>) -> Result<()> {
    let approval = Approval::load(args.approval_path)?;
    // Each live gateway is opened read-only and held through the core commit/rollback.
    // An established read transaction also prevents a stopped rollback-journal store from
    // being written between validation and cleanup.
    let mut gateway_handles: BTreeMap<(String, String), Connection> = BTreeMap::new();
    ensure!(
        args.destination_paths.len() == approval.destinations.len(),
        "cleanup destination count mismatch"
    );
    for destination in &approval.destinations {
        let key = (destination.kind_id.clone(), destination.path_id.clone());
        let path = args
            .destination_paths
            .get(&key)
            .context("cleanup destination missing")?;
        let handle = source::open_read_only(path)?;
        handle.execute_batch("BEGIN")?;
        let _: i64 = handle.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            gateway_handles.insert(key, handle).is_none(),
            "duplicate destination"
        );
    }
    let mut core = Connection::open_with_flags(
        args.core_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let tx = core.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Reopen the frozen manifest and all stores under the core write lock, before any deletion.
    // verify checks the exact complete inventory, physical live SHA, logical snapshot SHA,
    // immutable projection lineage, and all eight identity-source fields at every approved edge.
    freeze::verify(args.clone())?;
    let raw_manifest = fs::read(args.freeze_manifest_path)?;
    let manifest: serde_json::Value = serde_json::from_slice(&raw_manifest)?;
    let freeze_id = manifest["freeze_id"]
        .as_str()
        .context("freeze id missing")?;
    let manifest_sha256 = canonical::hash(&manifest)?;
    let operation_id = approval.operation_id;
    tx.execute_batch(
        "CREATE TABLE separation_cleanup_applied (
            freeze_id TEXT PRIMARY KEY,
            projection_operation_id TEXT NOT NULL,
            freeze_manifest_sha256 TEXT NOT NULL
        );",
    )?;
    for table in LEGACY_TABLES {
        tx.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
    }
    tx.execute(
        "INSERT INTO separation_cleanup_applied (freeze_id, projection_operation_id, freeze_manifest_sha256) VALUES (?1,?2,?3)",
        rusqlite::params![freeze_id, operation_id, manifest_sha256],
    )?;
    tx.commit()?;
    drop(gateway_handles);
    Ok(())
}
