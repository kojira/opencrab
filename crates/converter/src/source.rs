use crate::{ConverterError, Result};
use rusqlite::{types::ValueRef, Connection};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub(crate) struct SourceRow {
    pub source_key: Vec<u8>,
    pub row_values: Vec<u8>,
    pub row_digest: [u8; 32],
    values: Vec<SqliteValue>,
}

#[derive(Clone, Debug)]
pub(crate) enum SqliteValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

#[derive(Debug)]
pub(crate) struct SourceTable {
    pub name: &'static str,
    columns: Vec<String>,
    pub rows: Vec<SourceRow>,
}

impl SourceTable {
    pub fn empty(name: &'static str) -> Self {
        Self {
            name,
            columns: Vec::new(),
            rows: Vec::new(),
        }
    }

    pub fn exists(conn: &Connection, name: &str) -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get::<_, bool>(0),
        )?)
    }

    pub fn load(conn: &Connection, name: &'static str) -> Result<Self> {
        let exists = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Err(ConverterError::SourceSchema(format!(
                "required source table {name} is absent"
            )));
        }

        let quoted = quote_identifier(name);
        let mut info = conn.prepare(&format!("PRAGMA table_info({quoted})"))?;
        let definitions = info
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(info);
        let columns = definitions
            .iter()
            .map(|(column, _)| column.clone())
            .collect::<Vec<_>>();
        let mut primary = definitions
            .iter()
            .filter(|(_, ordinal)| *ordinal > 0)
            .map(|(column, ordinal)| (column.clone(), *ordinal))
            .collect::<Vec<_>>();
        primary.sort_by_key(|(_, ordinal)| *ordinal);
        let primary_indices = primary
            .iter()
            .map(|(column, _)| {
                columns
                    .iter()
                    .position(|candidate| candidate == column)
                    .ok_or_else(|| {
                        ConverterError::SourceSchema(format!(
                            "source table {name} has an unreadable primary key"
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut statement =
            conn.prepare(&format!("SELECT rowid,* FROM {quoted} ORDER BY rowid"))?;
        let loaded = statement
            .query_map([], |row| {
                let rowid = row.get::<_, i64>(0)?;
                let values = (1..row.as_ref().column_count())
                    .map(|index| {
                        Ok(match row.get_ref(index)? {
                            ValueRef::Null => SqliteValue::Null,
                            ValueRef::Integer(value) => SqliteValue::Integer(value),
                            ValueRef::Real(value) => SqliteValue::Real(value),
                            ValueRef::Text(value) => SqliteValue::Text(value.to_vec()),
                            ValueRef::Blob(value) => SqliteValue::Blob(value.to_vec()),
                        })
                    })
                    .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
                Ok((rowid, values))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);

        let mut primary_counts = BTreeMap::<Vec<u8>, usize>::new();
        for (_, values) in &loaded {
            if !primary_indices.is_empty()
                && primary_indices
                    .iter()
                    .all(|index| !matches!(values[*index], SqliteValue::Null))
            {
                let encoded = encode_sqlite_values(
                    &primary_indices
                        .iter()
                        .map(|index| values[*index].clone())
                        .collect::<Vec<_>>(),
                );
                *primary_counts.entry(encoded).or_default() += 1;
            }
        }

        let rows = loaded
            .into_iter()
            .map(|(rowid, values)| {
                let primary_key = if !primary_indices.is_empty()
                    && primary_indices
                        .iter()
                        .all(|index| !matches!(values[*index], SqliteValue::Null))
                {
                    let encoded = encode_sqlite_values(
                        &primary_indices
                            .iter()
                            .map(|index| values[*index].clone())
                            .collect::<Vec<_>>(),
                    );
                    (primary_counts.get(&encoded) == Some(&1)).then_some(encoded)
                } else {
                    None
                };
                let mut source_key = Vec::new();
                match primary_key {
                    Some(key) => {
                        source_key.push(b'P');
                        source_key.extend_from_slice(&key);
                    }
                    None => {
                        source_key.push(b'R');
                        source_key.extend_from_slice(&encode_sqlite_values(&[
                            SqliteValue::Integer(rowid),
                        ]));
                    }
                }
                let row_values = encode_sqlite_values(&values);
                let row_digest = Sha256::digest(&row_values).into();
                SourceRow {
                    source_key,
                    row_values,
                    row_digest,
                    values,
                }
            })
            .collect();
        Ok(Self {
            name,
            columns,
            rows,
        })
    }

    pub fn load_schema(conn: &Connection, name: &'static str) -> Result<Self> {
        if !Self::exists(conn, name)? {
            return Err(ConverterError::SourceSchema(format!(
                "required source table {name} is absent"
            )));
        }
        let quoted = quote_identifier(name);
        let mut info = conn.prepare(&format!("PRAGMA table_info({quoted})"))?;
        let columns = info
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self {
            name,
            columns,
            rows: Vec::new(),
        })
    }

    pub fn for_each_row(
        &self,
        conn: &Connection,
        order_by: &str,
        mut consume: impl FnMut(&SourceRow) -> Result<()>,
    ) -> Result<()> {
        let quoted = quote_identifier(self.name);
        let mut statement =
            conn.prepare(&format!("SELECT rowid,* FROM {quoted} ORDER BY {order_by}"))?;
        let primary_indices = {
            let mut info = conn.prepare(&format!("PRAGMA table_info({quoted})"))?;
            let mut primary = info
                .query_map([], |row| {
                    Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|(_, ordinal)| *ordinal > 0)
                .collect::<Vec<_>>();
            primary.sort_by_key(|(_, ordinal)| *ordinal);
            primary
                .iter()
                .map(|(column, _)| {
                    self.columns
                        .iter()
                        .position(|candidate| candidate == column)
                        .ok_or_else(|| {
                            ConverterError::SourceSchema(format!(
                                "source table {} has an unreadable primary key",
                                self.name
                            ))
                        })
                })
                .collect::<Result<Vec<_>>>()?
        };
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let rowid = row.get::<_, i64>(0)?;
            let values = (1..row.as_ref().column_count())
                .map(|index| {
                    Ok(match row.get_ref(index)? {
                        ValueRef::Null => SqliteValue::Null,
                        ValueRef::Integer(value) => SqliteValue::Integer(value),
                        ValueRef::Real(value) => SqliteValue::Real(value),
                        ValueRef::Text(value) => SqliteValue::Text(value.to_vec()),
                        ValueRef::Blob(value) => SqliteValue::Blob(value.to_vec()),
                    })
                })
                .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
            let primary_key = (!primary_indices.is_empty()
                && primary_indices
                    .iter()
                    .all(|index| !matches!(values[*index], SqliteValue::Null)))
            .then(|| {
                encode_sqlite_values(
                    &primary_indices
                        .iter()
                        .map(|index| values[*index].clone())
                        .collect::<Vec<_>>(),
                )
            });
            let mut source_key = Vec::new();
            if let Some(key) = primary_key {
                source_key.push(b'P');
                source_key.extend_from_slice(&key);
            } else {
                source_key.push(b'R');
                source_key.extend_from_slice(&encode_sqlite_values(&[SqliteValue::Integer(rowid)]));
            }
            let row_values = encode_sqlite_values(&values);
            let row_digest = Sha256::digest(&row_values).into();
            consume(&SourceRow {
                source_key,
                row_values,
                row_digest,
                values,
            })?;
        }
        Ok(())
    }

    pub fn value<'a>(&'a self, row: &'a SourceRow, column: &str) -> Option<&'a SqliteValue> {
        self.columns
            .iter()
            .position(|candidate| candidate == column)
            .and_then(|index| row.values.get(index))
    }

    pub fn require_exact_columns(&self, expected: &[&str]) -> Result<()> {
        let actual = self
            .columns
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let expected = expected.iter().copied().collect::<BTreeSet<_>>();
        if actual != expected {
            return Err(ConverterError::SourceSchema(format!(
                "source table {} columns differ: actual={actual:?}, expected={expected:?}",
                self.name
            )));
        }
        Ok(())
    }

    pub fn text<'a>(&'a self, row: &'a SourceRow, column: &str) -> Option<&'a str> {
        match self.value(row, column)? {
            SqliteValue::Text(value) => std::str::from_utf8(value).ok(),
            _ => None,
        }
    }

    pub fn nullable_text<'a>(
        &'a self,
        row: &'a SourceRow,
        column: &str,
    ) -> Option<Option<&'a str>> {
        match self.value(row, column)? {
            SqliteValue::Null => Some(None),
            SqliteValue::Text(value) => Some(Some(std::str::from_utf8(value).ok()?)),
            _ => None,
        }
    }

    pub fn integer(&self, row: &SourceRow, column: &str) -> Option<i64> {
        match self.value(row, column)? {
            SqliteValue::Integer(value) => Some(*value),
            _ => None,
        }
    }

    pub fn nullable_integer(&self, row: &SourceRow, column: &str) -> Option<Option<i64>> {
        match self.value(row, column)? {
            SqliteValue::Null => Some(None),
            SqliteValue::Integer(value) => Some(Some(*value)),
            _ => None,
        }
    }

    pub fn bytes<'a>(&'a self, row: &'a SourceRow, column: &str) -> Option<&'a [u8]> {
        match self.value(row, column)? {
            SqliteValue::Text(value) | SqliteValue::Blob(value) => Some(value),
            _ => None,
        }
    }

    pub fn nullable_bytes<'a>(
        &'a self,
        row: &'a SourceRow,
        column: &str,
    ) -> Option<Option<&'a [u8]>> {
        match self.value(row, column)? {
            SqliteValue::Null => Some(None),
            SqliteValue::Text(value) | SqliteValue::Blob(value) => Some(Some(value)),
            _ => None,
        }
    }

    pub fn encoded_value(&self, row: &SourceRow, column: &str) -> Option<Vec<u8>> {
        self.value(row, column)
            .map(|value| encode_sqlite_values(std::slice::from_ref(value)))
    }
}

pub(crate) fn encode_sqlite_values(values: &[SqliteValue]) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(&(values.len() as u64).to_be_bytes());
    for value in values {
        match value {
            SqliteValue::Null => output.push(0),
            SqliteValue::Integer(value) => {
                output.push(1);
                output.extend_from_slice(&value.to_be_bytes());
            }
            SqliteValue::Real(value) => {
                output.push(2);
                output.extend_from_slice(&value.to_bits().to_be_bytes());
            }
            SqliteValue::Text(value) => {
                output.push(3);
                output.extend_from_slice(&(value.len() as u64).to_be_bytes());
                output.extend_from_slice(value);
            }
            SqliteValue::Blob(value) => {
                output.push(4);
                output.extend_from_slice(&(value.len() as u64).to_be_bytes());
                output.extend_from_slice(value);
            }
        }
    }
    output
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_value_encoder_is_byte_stable() {
        let encoded = encode_sqlite_values(&[
            SqliteValue::Null,
            SqliteValue::Integer(-2),
            SqliteValue::Real(-0.0),
            SqliteValue::Text(b"A".to_vec()),
            SqliteValue::Blob(vec![0, 255]),
        ]);
        assert_eq!(
            encoded,
            [
                0, 0, 0, 0, 0, 0, 0, 5, 0, 1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, 2,
                0x80, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 1, b'A', 4, 0, 0, 0, 0, 0, 0, 0,
                2, 0, 0xff,
            ]
        );
    }

    #[test]
    fn invalid_utf8_text_is_loaded_and_encoded_without_loss() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE dirty(id TEXT PRIMARY KEY,value TEXT);
                 INSERT INTO dirty VALUES('row',CAST(x'80' AS TEXT));",
            )
            .unwrap();
        let table = SourceTable::load(&connection, "dirty").unwrap();
        assert_eq!(table.text(&table.rows[0], "value"), None);
        assert!(table.rows[0]
            .row_values
            .ends_with(&[3, 0, 0, 0, 0, 0, 0, 0, 1, 0x80]));
    }
}
