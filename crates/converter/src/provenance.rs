use crate::source::{encode_sqlite_values, SourceRow, SourceTable, SqliteValue};
use crate::Result;
use rusqlite::{params, Transaction};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub(crate) struct MigrationProvenance {
    source_database_digest: [u8; 32],
}

impl MigrationProvenance {
    pub(crate) fn new(source_database_digest: [u8; 32]) -> Self {
        Self {
            source_database_digest,
        }
    }

    pub(crate) fn write(
        &self,
        target: &Transaction<'_>,
        target_entity: &str,
        target_key: &[u8],
        table: &SourceTable,
        row: &SourceRow,
    ) -> Result<()> {
        self.write_parts(
            target,
            target_entity,
            target_key,
            table.name,
            &row.source_key,
            &row.row_digest,
        )
    }

    pub(crate) fn write_parts(
        &self,
        target: &Transaction<'_>,
        target_entity: &str,
        target_key: &[u8],
        source_table: &str,
        source_key: &[u8],
        source_row_digest: &[u8; 32],
    ) -> Result<()> {
        let source_locator = format!("table:{source_table}");
        target.execute(
            "INSERT INTO migration_provenance(
               target_entity,target_key,source_database_digest,source_locator,source_key,
               source_row_digest
             ) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                target_entity,
                target_key,
                self.source_database_digest.as_slice(),
                source_locator,
                source_key,
                source_row_digest.as_slice(),
            ],
        )?;
        Ok(())
    }
}

pub(crate) fn digest_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

pub(crate) fn integer_key(value: i64) -> Vec<u8> {
    encode_sqlite_values(&[SqliteValue::Integer(value)])
}

pub(crate) fn composite_key(values: &[SqliteValue]) -> Vec<u8> {
    encode_sqlite_values(values)
}

pub(crate) fn text(value: &str) -> SqliteValue {
    SqliteValue::Text(value.as_bytes().to_vec())
}
