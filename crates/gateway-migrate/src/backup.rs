use crate::{canonical, manifest::Destination, source};
use anyhow::{ensure, Result};
use rusqlite::{backup::Backup, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupRecord {
    pub kind_id: String,
    pub path_id: String,
    pub schema: String,
    pub file_sha256: String,
    pub logical_sha256: String,
}

pub fn create_or_load_set(
    core_path: &Path,
    destinations: &[(Destination, PathBuf)],
    backup_dir: &Path,
) -> Result<Vec<BackupRecord>> {
    let mut inputs = vec![(
        "core".to_string(),
        "core".to_string(),
        "core-v57".to_string(),
        core_path.to_path_buf(),
    )];
    inputs.extend(destinations.iter().map(|(item, path)| {
        (
            item.kind_id.clone(),
            item.path_id.clone(),
            item.schema.clone(),
            path.clone(),
        )
    }));
    inputs.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));

    ensure!(!backup_dir.exists(), "backup directory must be new");
    fs::create_dir(backup_dir)?;
    set_mode(backup_dir, 0o700)?;
    validate_path(backup_dir, 0o700, true)?;
    for (kind, path_id, _, source_path) in &inputs {
        sqlite_backup(source_path, &database_path(backup_dir, kind, path_id))?;
    }
    let expected_names = inputs
        .iter()
        .map(|(kind, path_id, _, _)| backup_name(kind, path_id))
        .collect::<std::collections::BTreeSet<_>>();
    let actual_names = fs::read_dir(backup_dir)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    ensure!(
        actual_names == expected_names,
        "backup set files do not match inputs"
    );

    inputs
        .into_iter()
        .map(|(kind_id, path_id, schema, _)| {
            let target = database_path(backup_dir, &kind_id, &path_id);
            validate_path(&target, 0o600, false)?;
            Ok(BackupRecord {
                kind_id,
                path_id,
                schema,
                file_sha256: source::file_sha256(&target)?,
                logical_sha256: logical_sha256(&target)?,
            })
        })
        .collect()
}

pub fn verify_record_set(backup_dir: &Path, records: &[BackupRecord]) -> Result<()> {
    validate_path(backup_dir, 0o700, true)?;
    let expected = records
        .iter()
        .map(|record| backup_name(&record.kind_id, &record.path_id))
        .collect::<std::collections::BTreeSet<_>>();
    let actual = fs::read_dir(backup_dir)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    ensure!(actual == expected, "recorded backup files differ");
    for record in records {
        let path = database_path(backup_dir, &record.kind_id, &record.path_id);
        validate_path(&path, 0o600, false)?;
        ensure!(
            source::file_sha256(&path)? == record.file_sha256
                && logical_sha256(&path)? == record.logical_sha256,
            "recorded backup digest mismatch"
        );
    }
    Ok(())
}

pub fn set_sha256(records: &[BackupRecord]) -> Result<String> {
    canonical::hash(records)
}

pub fn logical_sha256(path: &Path) -> Result<String> {
    let conn = source::open_read_only(path)?;
    let quick: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(quick == "ok", "backup quick_check failed");
    let mut tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    tables.sort();
    let mut digest = Sha256::new();
    for table in tables {
        digest.update((table.len() as u64).to_be_bytes());
        digest.update(table.as_bytes());
        let quoted = table.replace('"', "\"\"");
        let column_names = conn
            .prepare(&format!("PRAGMA table_info(\"{quoted}\")"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            !column_names.is_empty(),
            "table {table} has no inspectable columns"
        );
        let order = column_names
            .iter()
            .map(|name| format!("\"{}\"", name.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(",");
        let mut statement =
            conn.prepare(&format!("SELECT * FROM \"{quoted}\" ORDER BY {order}"))?;
        let columns = statement.column_count();
        let rows = statement.query_map([], |row| {
            let mut bytes = Vec::new();
            for index in 0..columns {
                use rusqlite::types::ValueRef;
                match row.get_ref(index)? {
                    ValueRef::Null => bytes.push(0),
                    ValueRef::Integer(value) => {
                        bytes.push(1);
                        bytes.extend_from_slice(value.to_string().as_bytes());
                    }
                    ValueRef::Real(value) => {
                        bytes.push(2);
                        bytes.extend_from_slice(value.to_bits().to_be_bytes().as_slice());
                    }
                    ValueRef::Text(value) => {
                        bytes.push(3);
                        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
                        bytes.extend_from_slice(value);
                    }
                    ValueRef::Blob(value) => {
                        bytes.push(4);
                        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
                        bytes.extend_from_slice(value);
                    }
                }
            }
            Ok(bytes)
        })?;
        for row in rows {
            digest.update(row?);
        }
    }
    Ok(canonical::hex(&digest.finalize()))
}

fn sqlite_backup(source_path: &Path, target_path: &Path) -> Result<()> {
    let source =
        Connection::open_with_flags(source_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut target = Connection::open(target_path)?;
    let backup = Backup::new(&source, &mut target)?;
    backup.run_to_completion(64, Duration::from_millis(5), None)?;
    drop(backup);
    let check: String = target.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(check == "ok", "backup verification failed");
    set_mode(target_path, 0o600)?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn validate_path(path: &Path, mode: u32, directory: bool) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.file_type().is_symlink(),
        "backup path may not be a symlink"
    );
    ensure!(
        if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        },
        "backup path type mismatch"
    );
    ensure!(
        meta.uid() == unsafe { libc::geteuid() },
        "backup path owner mismatch"
    );
    ensure!(meta.mode() & 0o777 == mode, "backup path mode mismatch");
    Ok(())
}

pub fn database_path(backup_dir: &Path, kind: &str, path_id: &str) -> PathBuf {
    backup_dir.join(backup_name(kind, path_id))
}

fn backup_name(kind: &str, path_id: &str) -> String {
    format!("{}-{}.sqlite", safe(kind), safe(path_id))
}

fn safe(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod s8_review_red_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_second_import_cannot_reuse_a_preexisting_backup_directory() {
        let temp = tempfile::tempdir().unwrap();
        let core = temp.path().join("core.db");
        let dest = temp.path().join("dest.db");
        let backups = temp.path().join("backups");
        Connection::open(&core)
            .unwrap()
            .execute_batch("CREATE TABLE x(v TEXT); INSERT INTO x VALUES ('old');")
            .unwrap();
        Connection::open(&dest)
            .unwrap()
            .execute_batch("CREATE TABLE x(v TEXT); INSERT INTO x VALUES ('dest');")
            .unwrap();
        let destination = Destination {
            kind_id: "discord".into(),
            path_id: "main".into(),
            schema: "s5-discord-v1".into(),
        };
        create_or_load_set(&core, &[(destination.clone(), dest.clone())], &backups).unwrap();
        let error = create_or_load_set(&core, &[(destination, dest.clone())], &backups)
            .expect_err("an existing directory cannot be the new matched backup set");
        assert!(error.to_string().contains("backup"));
        Connection::open(&core)
            .unwrap()
            .execute("INSERT INTO x VALUES ('new')", [])
            .unwrap();
        Connection::open(&dest)
            .unwrap()
            .execute("INSERT INTO x VALUES ('changed')", [])
            .unwrap();
        // The operator restores the complete set after failure, never a single database.
        fs::copy(database_path(&backups, "core", "core"), &core).unwrap();
        fs::copy(database_path(&backups, "discord", "main"), &dest).unwrap();
        assert_eq!(
            Connection::open(&core)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM x", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            Connection::open(&dest)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM x", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            fs::metadata(&backups).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
